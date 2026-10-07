//! Materialize recent canonical reflection receipts into the existing outbox.
//! No model calls, destination inference, delivery ledger or approval authority.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;
use zeroclaw_config::schema::Config;
use zeroclaw_infra::bridge_outbox::BridgeOutbox;
use zeroclaw_memory::companion::{SOUL_PROFILE_DB_FILE, SoulProfileStore};

const RECENT_SECONDS: u64 = 24 * 60 * 60;
const RECEIPTS_PER_AGENT: usize = 200;
const CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// Destination configuration owns the target; the startup data root owns this
/// gateway's stores. A changed root must wait for the owning gateway to restart.
fn eligible(config: &Config, data_dir: &Path) -> bool {
    config.data_dir == data_dir
        && config
            .companion_memory
            .review_notification
            .as_ref()
            .is_some_and(|target| {
                target.validate().is_ok() && config.gateway.bridges.contains_key(&target.bridge)
            })
}

pub(crate) fn reconcile(
    config: &RwLock<Config>,
    data_dir: &Path,
    path_prefix: &str,
    now: u64,
    stop: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<usize> {
    if stop.is_cancelled() {
        return Ok(0);
    }
    let aliases: Vec<String> = {
        let config = config.read();
        if !eligible(&config, data_dir) || !data_dir.join(SOUL_PROFILE_DB_FILE).is_file() {
            return Ok(0);
        }
        config
            .agents
            .iter()
            .filter(|(_, agent)| agent.enabled)
            .map(|(alias, _)| alias.clone())
            .collect()
    };
    // The router's startup prefix is authoritative; a live config edit does
    // not remount this running gateway.
    let review_path = format!("{path_prefix}/review");
    let store = SoulProfileStore::shared(data_dir)?;
    let mut reconciled = 0;
    for agent in aliases {
        if stop.is_cancelled() {
            return Ok(reconciled);
        }
        let receipts = store.reflections(&agent, RECEIPTS_PER_AGENT)?;
        if receipts.len() == RECEIPTS_PER_AGENT {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(serde_json::json!({"agent":agent,"limit":RECEIPTS_PER_AGENT})),
                "weekly review receipt scan reached its bounded limit; older receipts were not reconciled"
            );
        }
        for (id, receipt) in receipts {
            if receipt.ran_at_unix > now
                || now.saturating_sub(receipt.ran_at_unix) >= RECENT_SECONDS
                || (receipt.proposals_created == 0 && receipt.user_model_candidates_created == 0)
            {
                continue;
            }
            let soul_count = receipt.proposals_created.to_string();
            let user_count = receipt.user_model_candidates_created.to_string();
            let content = zeroclaw_runtime::i18n::get_required_cli_string_with_args(
                "soul-weekly-review-notice",
                &[
                    ("soul_count", &soul_count),
                    ("user_count", &user_count),
                    ("review_path", &review_path),
                ],
            );
            // Resolve and hold the current destination through enqueue. A
            // simultaneous config swap cannot send to a superseded target.
            let current = config.read();
            if stop.is_cancelled() {
                return Ok(reconciled);
            }
            if !eligible(&current, data_dir)
                || !current.agents.get(&agent).is_some_and(|a| a.enabled)
            {
                return Ok(reconciled);
            }
            let Some(target) = current.companion_memory.review_notification.as_ref() else {
                return Ok(reconciled);
            };
            BridgeOutbox::shared(data_dir)?.enqueue_source(
                &target.bridge,
                &target.recipient,
                target.thread_id.as_deref(),
                &content,
                "weekly_review",
                &agent,
                &id.to_string(),
            )?;
            reconciled += 1;
        }
    }
    Ok(reconciled)
}

/// Only unclaimed weekly-review candidates follow this opt-in destination.
/// Existing transport receipts remain authoritative once delivery is in flight.
pub(crate) fn permits_delivery(
    config: &Config,
    outbox: &BridgeOutbox,
    row: &zeroclaw_infra::bridge_outbox::OutboxItem,
) -> bool {
    if row.source_kind != "weekly_review" {
        return true;
    }
    outbox.is_for_data_dir(&config.data_dir)
        && config
            .agents
            .get(&row.source_id)
            .is_some_and(|agent| agent.enabled)
        && config
            .companion_memory
            .review_notification
            .as_ref()
            .is_some_and(|target| {
                target.validate().is_ok()
                    && config.gateway.bridges.contains_key(&target.bridge)
                    && target.bridge == row.bridge
                    && target.recipient == row.to
                    && target.thread_id == row.thread_id
            })
}

pub(crate) struct ReviewNotificationTask(tokio::task::JoinHandle<()>);

impl Drop for ReviewNotificationTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Owned by the gateway serve future and cancelled on its shutdown signal or
/// when the serve future exits. A receipt is retried without re-running reflection.
pub(crate) fn start(
    config: Arc<RwLock<Config>>,
    path_prefix: String,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> ReviewNotificationTask {
    let data_dir: PathBuf = config.read().data_dir.clone();
    let stop = tokio_util::sync::CancellationToken::new();
    let stop_on_drop = stop.clone().drop_guard();
    let task = ::zeroclaw_spawn::spawn!(async move {
        let mut interval = tokio::time::interval(CHECK_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                _ = interval.tick() => {}
            }
            let config = Arc::clone(&config);
            let data_dir = data_dir.clone();
            let path_prefix = path_prefix.clone();
            let now = match SystemTime::now().duration_since(UNIX_EPOCH) {
                Ok(now) => now.as_secs(),
                Err(error) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                            .with_attrs(serde_json::json!({"error":error.to_string()})),
                        "weekly review clock unavailable"
                    );
                    continue;
                }
            };
            let pass_stop = stop.clone();
            let pass = tokio::task::spawn_blocking(move || {
                reconcile(&config, &data_dir, &path_prefix, now, &pass_stop)
            });
            let result = tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                result = pass => result,
            };
            let error = match result {
                Ok(Ok(_)) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(error) => Some(error.to_string()),
            };
            if let Some(error) = error {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(serde_json::json!({"error":error})),
                    "weekly review notification reconciliation failed; pending receipts will be retried"
                );
            }
        }
        drop(stop_on_drop);
    });
    ReviewNotificationTask(task)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_config::companion::ReviewNotificationConfig;
    use zeroclaw_config::schema::{AliasedAgentConfig, GatewayBridgeConfig};
    use zeroclaw_memory::companion::SoulReflectionReceipt;

    fn reconcile(config: &RwLock<Config>, data_dir: &Path, now: u64) -> anyhow::Result<usize> {
        super::reconcile(
            config,
            data_dir,
            "",
            now,
            &tokio_util::sync::CancellationToken::new(),
        )
    }

    const NOW: u64 = 1_800_000_000;

    fn fixture() -> (tempfile::TempDir, RwLock<Config>, Arc<SoulProfileStore>) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config {
            data_dir: dir.path().to_path_buf(),
            ..Default::default()
        };
        config.agents.insert(
            "nova".into(),
            AliasedAgentConfig {
                enabled: true,
                ..Default::default()
            },
        );
        config
            .gateway
            .bridges
            .insert("tg".into(), GatewayBridgeConfig::default());
        config.companion_memory.review_notification = Some(ReviewNotificationConfig {
            bridge: "tg".into(),
            recipient: "owner".into(),
            thread_id: None,
        });
        let soul = SoulProfileStore::shared(dir.path()).unwrap();
        (dir, RwLock::new(config), soul)
    }

    fn receipt(soul: &SoulProfileStore, at: u64, count: u64, outcome: &str) {
        soul.record_reflection(
            "nova",
            &SoulReflectionReceipt {
                period_from_unix: at.saturating_sub(604800),
                messages_read: 1,
                proposals_created: count,
                user_model_candidates_created: count,
                outcome: outcome.into(),
                ran_at_unix: at,
            },
        )
        .unwrap();
    }

    #[tokio::test]
    async fn weekly_review_task_honors_shutdown_before_initial_reconciliation() {
        let (dir, config, soul) = fixture();
        receipt(&soul, NOW, 1, "ok");
        let (_shutdown, receiver) = tokio::sync::watch::channel(true);
        let mut task = start(Arc::new(config), String::new(), receiver);
        tokio::time::timeout(Duration::from_secs(1), &mut task.0)
            .await
            .unwrap()
            .unwrap();
        assert!(!dir.path().join("sessions/bridge_outbox.db").exists());
    }

    #[test]
    fn weekly_review_link_uses_startup_prefix_not_live_config() {
        for prefix in ["", "/controller"] {
            let (dir, config, soul) = fixture();
            receipt(&soul, NOW, 1, "ok");
            config.write().gateway.path_prefix = Some("/edited-after-start".into());
            super::reconcile(
                &config,
                dir.path(),
                prefix,
                NOW,
                &tokio_util::sync::CancellationToken::new(),
            )
            .unwrap();
            let rows = BridgeOutbox::open(dir.path())
                .unwrap()
                .pending("tg", 0, 100)
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert!(
                rows[0]
                    .content
                    .contains(&format!("Review: {prefix}/review")),
                "{}",
                rows[0].content
            );
            assert!(!rows[0].content.contains("edited-after-start"));
        }
    }

    #[test]
    fn weekly_review_reconciles_committed_partial_receipts_and_deduplicates_after_reopen() {
        let (dir, config, soul) = fixture();
        receipt(&soul, NOW, 2, "storage_write_failed");
        receipt(&soul, NOW, 0, "clock_started");
        receipt(&soul, NOW - RECENT_SECONDS, 1, "ok");
        receipt(&soul, NOW + 1, 1, "ok");
        assert_eq!(reconcile(&config, dir.path(), NOW).unwrap(), 1);
        reconcile(&config, dir.path(), NOW).unwrap();
        let reopened = BridgeOutbox::open(dir.path()).unwrap();
        let rows = reopened.pending("tg", 0, 100).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source_kind, "weekly_review");
        assert_eq!(rows[0].source_id, "nova");
        assert_eq!(
            rows[0].event_id,
            soul.reflections("nova", 200)
                .unwrap()
                .last()
                .unwrap()
                .0
                .to_string()
        );
        assert!(rows[0].content.contains("/review"));
        assert!(!rows[0].content.contains("storage_write_failed"));
        assert!(!rows[0].content.contains("nova"));
        assert!(permits_delivery(&config.read(), &reopened, &rows[0]));
        config
            .write()
            .companion_memory
            .review_notification
            .as_mut()
            .unwrap()
            .thread_id = Some("new-thread".into());
        assert!(!permits_delivery(&config.read(), &reopened, &rows[0]));
        reconcile(&config, dir.path(), NOW).unwrap();
        assert_eq!(
            reopened.len("tg").unwrap(),
            1,
            "thread change must not rewrite dedup identity"
        );
    }

    #[test]
    fn weekly_review_uses_live_target_and_fails_closed_on_disable_invalid_or_changed_root() {
        let (dir, config, soul) = fixture();
        receipt(&soul, NOW, 1, "ok");
        let target = config
            .write()
            .companion_memory
            .review_notification
            .take()
            .unwrap();
        assert_eq!(reconcile(&config, dir.path(), NOW).unwrap(), 0);
        assert!(!dir.path().join("sessions/bridge_outbox.db").exists());
        config.write().companion_memory.review_notification = Some(ReviewNotificationConfig {
            recipient: " ".into(),
            ..target.clone()
        });
        assert_eq!(reconcile(&config, dir.path(), NOW).unwrap(), 0);
        config.write().companion_memory.review_notification = Some(ReviewNotificationConfig {
            bridge: "missing".into(),
            ..target.clone()
        });
        assert_eq!(reconcile(&config, dir.path(), NOW).unwrap(), 0);
        config.write().companion_memory.review_notification = Some(target.clone());
        reconcile(&config, dir.path(), NOW).unwrap();
        let outbox = BridgeOutbox::shared(dir.path()).unwrap();
        let original = outbox.pending("tg", 0, 100).unwrap().remove(0);
        config
            .write()
            .companion_memory
            .review_notification
            .as_mut()
            .unwrap()
            .recipient = "other-owner-target".into();
        assert!(!permits_delivery(&config.read(), &outbox, &original));
        reconcile(&config, dir.path(), NOW).unwrap();
        assert_eq!(outbox.len("tg").unwrap(), 2);
        config.write().companion_memory.review_notification = None;
        assert!(!permits_delivery(&config.read(), &outbox, &original));
        assert_eq!(reconcile(&config, dir.path(), NOW).unwrap(), 0);
        config.write().companion_memory.review_notification = Some(target);
        config.write().data_dir = dir.path().join("new-root");
        assert_eq!(reconcile(&config, dir.path(), NOW).unwrap(), 0);
        assert!(!permits_delivery(&config.read(), &outbox, &original));
        assert!(!dir.path().join("new-root").exists());
    }

    #[test]
    fn weekly_review_enqueue_failure_retries_the_same_receipt_without_new_reflection() {
        let (dir, config, soul) = fixture();
        receipt(&soul, NOW, 1, "ok");
        let outbox = BridgeOutbox::shared(dir.path()).unwrap();
        let conn =
            rusqlite::Connection::open(dir.path().join("sessions/bridge_outbox.db")).unwrap();
        conn.execute_batch("CREATE TRIGGER refuse_notice BEFORE INSERT ON bridge_outbox BEGIN SELECT RAISE(ABORT,'fixture enqueue failure'); END;").unwrap();
        assert!(reconcile(&config, dir.path(), NOW).is_err());
        assert_eq!(outbox.len("tg").unwrap(), 0);
        conn.execute_batch("DROP TRIGGER refuse_notice;").unwrap();
        reconcile(&config, dir.path(), NOW).unwrap();
        assert_eq!(outbox.len("tg").unwrap(), 1);
        assert_eq!(soul.reflections("nova", 200).unwrap().len(), 1);
    }
}

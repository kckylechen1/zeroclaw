//! Materialize recent canonical reflection receipts, and the deterministic Soul
//! drift report (ADR-018 §4.3), into the existing outbox.
//! No model calls, destination inference, delivery ledger or approval authority.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;
use zeroclaw_config::schema::Config;
use zeroclaw_infra::bridge_outbox::BridgeOutbox;
use zeroclaw_memory::companion::{SOUL_PROFILE_DB_FILE, SoulDrift, SoulProfileStore};

const WEEKLY_REVIEW: &str = "weekly_review";
const SOUL_DRIFT: &str = "soul_drift";
/// A dial this many steps (either direction) from the anchor is reported.
const DRIFT_THRESHOLD_STEPS: u8 = 2;
/// Drift that exists is reported again once per this period since the anchor.
const DRIFT_PERIOD_SECONDS: u64 = 28 * 24 * 60 * 60;
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
            if !enqueue_notice(
                config,
                data_dir,
                stop,
                &agent,
                &content,
                WEEKLY_REVIEW,
                &id.to_string(),
            )? {
                return Ok(reconciled);
            }
            reconciled += 1;
        }
        let configured = config
            .read()
            .persona_for_agent(&agent)
            .copied()
            .unwrap_or_default();
        let drift = store.drift(&agent, configured, now)?;
        for event_id in drift_event_ids(&drift, now) {
            let content = zeroclaw_runtime::i18n::get_required_cli_string_with_args(
                "soul-drift-notice",
                &[
                    ("dials_moved", &moved_dials(&drift).to_string()),
                    ("max_steps", &drift.max_abs_steps().to_string()),
                    ("story_added", &drift.story_added.to_string()),
                    ("story_retired", &drift.story_retired.to_string()),
                    ("review_path", &review_path),
                ],
            );
            if !enqueue_notice(
                config, data_dir, stop, &agent, &content, SOUL_DRIFT, &event_id,
            )? {
                return Ok(reconciled);
            }
            reconciled += 1;
        }
    }
    Ok(reconciled)
}

/// Resolve and hold the current destination through enqueue. A simultaneous
/// config swap cannot send to a superseded target. Returns `false` when the
/// pass must stop (cancelled, or the destination or agent is no longer
/// eligible); the notice is then not queued.
fn enqueue_notice(
    config: &RwLock<Config>,
    data_dir: &Path,
    stop: &tokio_util::sync::CancellationToken,
    agent: &str,
    content: &str,
    source_kind: &str,
    event_id: &str,
) -> anyhow::Result<bool> {
    let current = config.read();
    if stop.is_cancelled() {
        return Ok(false);
    }
    if !eligible(&current, data_dir) || !current.agents.get(agent).is_some_and(|a| a.enabled) {
        return Ok(false);
    }
    let Some(target) = current.companion_memory.review_notification.as_ref() else {
        return Ok(false);
    };
    BridgeOutbox::shared(data_dir)?.enqueue_source(
        &target.bridge,
        &target.recipient,
        target.thread_id.as_deref(),
        content,
        source_kind,
        agent,
        event_id,
    )?;
    Ok(true)
}

fn moved_dials(drift: &SoulDrift) -> usize {
    drift.dials.iter().filter(|dial| dial.steps != 0).count()
}

/// Event ids of the drift reports due for `drift` at `now`. Each id names a
/// state, so the outbox's `(bridge, recipient, source_kind, source_id,
/// event_id)` de-duplication reports it once.
///
/// - threshold: any dial `DRIFT_THRESHOLD_STEPS` or more from the anchor;
///   `threshold:{anchor_key}:{fingerprint}`, where the fingerprint lists every
///   dial's current level, so a new state is reported again.
/// - periodic: any drift at all, once a full 28-day bucket since the anchor
///   was set has elapsed; `periodic:{anchor_key}:{bucket}`.
///
/// `anchor_key` is the anchor row id, or `default` for a derived anchor.
fn drift_event_ids(drift: &SoulDrift, now: u64) -> Vec<String> {
    let anchor_key = drift
        .anchor
        .id
        .map_or_else(|| "default".to_string(), |id| id.to_string());
    let mut ids = Vec::new();
    if drift.max_abs_steps() >= DRIFT_THRESHOLD_STEPS {
        let fingerprint = drift
            .dials
            .iter()
            .map(|dial| format!("{}={}", dial.key, dial.current_level))
            .collect::<Vec<_>>()
            .join(",");
        ids.push(format!("threshold:{anchor_key}:{fingerprint}"));
    }
    if !drift.is_empty() {
        let bucket = now.saturating_sub(drift.anchor.set_at_unix) / DRIFT_PERIOD_SECONDS;
        if bucket >= 1 {
            ids.push(format!("periodic:{anchor_key}:{bucket}"));
        }
    }
    ids
}

/// Only unclaimed weekly-review and Soul-drift candidates follow this opt-in
/// destination. Existing transport receipts remain authoritative once delivery
/// is in flight.
pub(crate) fn permits_delivery(
    config: &Config,
    outbox: &BridgeOutbox,
    row: &zeroclaw_infra::bridge_outbox::OutboxItem,
) -> bool {
    if row.source_kind != WEEKLY_REVIEW && row.source_kind != SOUL_DRIFT {
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
                drift: None,
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

    // ADR-018 §4.3: Soul drift report.

    const DAY: u64 = 24 * 60 * 60;

    fn tone(level: zeroclaw_config::persona::PersonaLevel) -> zeroclaw_memory::companion::SoulTone {
        zeroclaw_memory::companion::SoulTone {
            heads: [("warmth".to_string(), level)].into(),
        }
    }

    fn story(texts: &[&str]) -> zeroclaw_memory::companion::SoulStory {
        zeroclaw_memory::companion::SoulStory {
            entries: texts
                .iter()
                .map(|text| zeroclaw_memory::companion::StoryEntry {
                    kind: zeroclaw_memory::companion::StoryKind::Bond,
                    text: (*text).to_string(),
                })
                .collect(),
        }
    }

    /// The default anchor is Tone revision 1 (warmth `medium`) at `at`.
    fn anchored_at(soul: &SoulProfileStore, at: u64) {
        use zeroclaw_config::persona::PersonaLevel::Medium;
        soul.set_tone("nova", tone(Medium), 0, at).unwrap();
    }

    fn drift_rows(dir: &Path) -> Vec<zeroclaw_infra::bridge_outbox::OutboxItem> {
        BridgeOutbox::open(dir)
            .unwrap()
            .pending("tg", 0, 100)
            .unwrap()
            .into_iter()
            .filter(|row| row.source_kind == "soul_drift")
            .collect()
    }

    #[test]
    fn a_two_step_drift_is_reported_once_per_state() {
        use zeroclaw_config::persona::PersonaLevel::{High, Minimal, Xhigh};
        let (dir, config, soul) = fixture();
        anchored_at(&soul, NOW - 1000);
        // One step is below the threshold.
        soul.set_tone("nova", tone(High), 1, NOW - 900).unwrap();
        assert_eq!(reconcile(&config, dir.path(), NOW).unwrap(), 0);
        assert!(!dir.path().join("sessions/bridge_outbox.db").exists());

        soul.set_tone("nova", tone(Xhigh), 2, NOW - 800).unwrap();
        assert_eq!(reconcile(&config, dir.path(), NOW).unwrap(), 1);
        assert_eq!(reconcile(&config, dir.path(), NOW + 60).unwrap(), 1);
        let rows = drift_rows(dir.path());
        assert_eq!(rows.len(), 1, "the same state is reported once");
        assert_eq!(rows[0].source_id, "nova");
        assert_eq!(
            rows[0].event_id,
            "threshold:default:warmth=xhigh,directness=medium,explanation_density=medium,challenge=medium,humor=medium"
        );
        assert!(rows[0].content.contains("/review"), "{}", rows[0].content);
        assert!(
            rows[0].content.contains("1 Tone dials moved"),
            "{}",
            rows[0].content
        );
        assert!(
            rows[0].content.contains("largest move: 2 steps"),
            "{}",
            rows[0].content
        );
        assert!(!rows[0].content.contains("nova"));
        assert!(permits_delivery(
            &config.read(),
            &BridgeOutbox::open(dir.path()).unwrap(),
            &rows[0]
        ));

        // A new state is reported again, also in the other direction.
        soul.set_tone("nova", tone(Minimal), 3, NOW + 10).unwrap();
        assert_eq!(reconcile(&config, dir.path(), NOW + 120).unwrap(), 1);
        let rows = drift_rows(dir.path());
        assert_eq!(rows.len(), 2);
        assert!(
            rows[1]
                .event_id
                .starts_with("threshold:default:warmth=minimal,")
        );
    }

    #[test]
    fn an_owner_anchor_keys_the_threshold_event_and_resets_the_state() {
        use zeroclaw_config::persona::PersonaLevel::{Minimal, Xhigh};
        let (dir, config, soul) = fixture();
        anchored_at(&soul, NOW - 1000);
        soul.set_tone("nova", tone(Xhigh), 1, NOW - 900).unwrap();
        reconcile(&config, dir.path(), NOW).unwrap();
        // Keeping the current Soul as the anchor ends the drift.
        let anchor = soul.set_anchor_to_current("nova", 2, 0, NOW + 1).unwrap();
        assert_eq!(reconcile(&config, dir.path(), NOW + 60).unwrap(), 0);
        assert_eq!(drift_rows(dir.path()).len(), 1);
        // Drifting away from the new anchor is a new report under its row id.
        soul.set_tone("nova", tone(Minimal), 2, NOW + 100).unwrap();
        assert_eq!(reconcile(&config, dir.path(), NOW + 120).unwrap(), 1);
        let rows = drift_rows(dir.path());
        assert_eq!(rows.len(), 2);
        assert!(
            rows[1]
                .event_id
                .starts_with(&format!("threshold:{}:warmth=minimal,", anchor.id.unwrap()))
        );
    }

    #[test]
    fn nonzero_drift_is_reported_once_per_elapsed_28_day_bucket_and_not_before() {
        use zeroclaw_config::persona::PersonaLevel::High;
        let (dir, config, soul) = fixture();
        let set_at = NOW - 100 * DAY;
        anchored_at(&soul, set_at);
        soul.set_tone("nova", tone(High), 1, set_at + DAY).unwrap();
        // One step: below the threshold, so only the periodic report applies.
        assert_eq!(
            reconcile(&config, dir.path(), set_at + 28 * DAY - 1).unwrap(),
            0
        );
        assert_eq!(
            reconcile(&config, dir.path(), set_at + 28 * DAY).unwrap(),
            1
        );
        assert_eq!(
            reconcile(&config, dir.path(), set_at + 41 * DAY).unwrap(),
            1
        );
        assert_eq!(drift_rows(dir.path()).len(), 1);
        assert_eq!(drift_rows(dir.path())[0].event_id, "periodic:default:1");
        assert_eq!(
            reconcile(&config, dir.path(), set_at + 56 * DAY).unwrap(),
            1
        );
        let rows = drift_rows(dir.path());
        assert_eq!(
            rows.iter()
                .map(|row| row.event_id.as_str())
                .collect::<Vec<_>>(),
            ["periodic:default:1", "periodic:default:2"]
        );
    }

    #[test]
    fn a_story_only_drift_is_periodic_and_zero_drift_enqueues_nothing() {
        let (dir, config, soul) = fixture();
        let set_at = NOW - 100 * DAY;
        soul.set_story("nova", story(&["a", "b"]), 0, set_at)
            .unwrap();
        // No drift at all, however old the anchor.
        assert_eq!(reconcile(&config, dir.path(), NOW).unwrap(), 0);
        // A reordering is not drift either.
        soul.set_story("nova", story(&["b", "a"]), 1, set_at + DAY)
            .unwrap();
        assert_eq!(reconcile(&config, dir.path(), NOW).unwrap(), 0);
        assert!(!dir.path().join("sessions/bridge_outbox.db").exists());

        soul.set_story("nova", story(&["b"]), 2, set_at + 2 * DAY)
            .unwrap();
        assert_eq!(reconcile(&config, dir.path(), NOW).unwrap(), 1);
        let rows = drift_rows(dir.path());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_id, format!("periodic:default:{}", 100 / 28));
        assert!(
            rows[0].content.contains("0 Story entries added, 1 retired"),
            "{}",
            rows[0].content
        );
    }

    #[test]
    fn drift_uses_the_same_opt_in_target_and_fails_closed() {
        use zeroclaw_config::persona::PersonaLevel::Xhigh;
        let (dir, config, soul) = fixture();
        anchored_at(&soul, NOW - 1000);
        soul.set_tone("nova", tone(Xhigh), 1, NOW - 900).unwrap();
        let target = config
            .write()
            .companion_memory
            .review_notification
            .take()
            .unwrap();
        assert_eq!(reconcile(&config, dir.path(), NOW).unwrap(), 0);
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
        config.write().agents.get_mut("nova").unwrap().enabled = false;
        assert_eq!(reconcile(&config, dir.path(), NOW).unwrap(), 0);
        config.write().agents.get_mut("nova").unwrap().enabled = true;
        config.write().data_dir = dir.path().join("new-root");
        assert_eq!(reconcile(&config, dir.path(), NOW).unwrap(), 0);
        assert!(!dir.path().join("sessions/bridge_outbox.db").exists());
        assert!(!dir.path().join("new-root").exists());

        config.write().data_dir = dir.path().to_path_buf();
        assert_eq!(reconcile(&config, dir.path(), NOW).unwrap(), 1);
        let outbox = BridgeOutbox::shared(dir.path()).unwrap();
        let row = drift_rows(dir.path()).remove(0);
        assert!(permits_delivery(&config.read(), &outbox, &row));
        // Delivery of an unclaimed drift row follows the live target exactly
        // like a weekly review row.
        config
            .write()
            .companion_memory
            .review_notification
            .as_mut()
            .unwrap()
            .recipient = "other-owner-target".into();
        assert!(!permits_delivery(&config.read(), &outbox, &row));
        config.write().companion_memory.review_notification = Some(target.clone());
        config
            .write()
            .companion_memory
            .review_notification
            .as_mut()
            .unwrap()
            .thread_id = Some("new-thread".into());
        assert!(!permits_delivery(&config.read(), &outbox, &row));
        config.write().companion_memory.review_notification = Some(target.clone());
        config.write().agents.get_mut("nova").unwrap().enabled = false;
        assert!(!permits_delivery(&config.read(), &outbox, &row));
        config.write().agents.get_mut("nova").unwrap().enabled = true;
        config.write().companion_memory.review_notification = None;
        assert!(!permits_delivery(&config.read(), &outbox, &row));
        config.write().companion_memory.review_notification = Some(target);
        assert!(permits_delivery(&config.read(), &outbox, &row));
        // Other sources are not this module's to gate.
        let other = zeroclaw_infra::bridge_outbox::OutboxItem {
            source_kind: "cron".into(),
            ..row
        };
        config.write().companion_memory.review_notification = None;
        assert!(permits_delivery(&config.read(), &outbox, &other));
    }

    #[test]
    fn drift_event_ids_follow_the_documented_scheme() {
        use zeroclaw_memory::companion::{AnchorSource, DialDrift, SoulAnchor};
        let drift = |id: Option<i64>, set_at: u64, steps: [i8; 2], story: u32| SoulDrift {
            anchor: SoulAnchor {
                id,
                tone_revision_id: None,
                story_revision_id: None,
                set_at_unix: set_at,
                source: if id.is_some() {
                    AnchorSource::Owner
                } else {
                    AnchorSource::Default
                },
            },
            dials: ["warmth", "humor"]
                .iter()
                .zip(steps)
                .map(|(key, steps)| DialDrift {
                    key: (*key).to_string(),
                    anchor_level: "medium".into(),
                    current_level: if steps == 0 { "medium" } else { "high" }.into(),
                    steps,
                })
                .collect(),
            story_added: story,
            story_retired: 0,
            approved_proposals_since_anchor: 0,
        };
        // Approved proposals alone are not drift.
        let mut idle = drift(None, 0, [0, 0], 0);
        idle.approved_proposals_since_anchor = 9;
        assert!(drift_event_ids(&idle, 1000 * DAY).is_empty());
        assert_eq!(
            drift_event_ids(&drift(Some(7), 10, [-2, 0], 0), 10),
            ["threshold:7:warmth=high,humor=medium"]
        );
        assert_eq!(
            drift_event_ids(&drift(Some(7), 10, [0, 1], 0), 10 + 28 * DAY * 3 + 5),
            ["periodic:7:3"]
        );
        assert_eq!(
            drift_event_ids(&drift(None, 0, [0, 0], 1), 28 * DAY),
            ["periodic:default:1"]
        );
        assert_eq!(
            drift_event_ids(&drift(None, 0, [2, 0], 0), 28 * DAY),
            [
                "threshold:default:warmth=high,humor=medium",
                "periodic:default:1"
            ]
        );
        // A set time in the future never underflows into a bucket.
        assert!(drift_event_ids(&drift(None, 500, [1, 0], 0), 100).is_empty());
    }
}

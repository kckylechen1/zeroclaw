//! #63 delegated-result delivery: observe terminal states of bound Tachi
//! dispatches and enqueue one attention notice per canonical outcome into the
//! existing bridge outbox. No second task table, notifier, or policy — the
//! outbox's `(source_kind, source_id, event_id)` identity de-duplicates, the
//! attention policy (#447) gates delivery, and execution state remains owned
//! by Tachi's receipts. A status query failure is an unknown outcome and is
//! never itself a notification.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use parking_lot::RwLock;
use zeroclaw_api::tachi_staff::{RunState, RunStatus};
use zeroclaw_config::schema::Config;
use zeroclaw_infra::bridge_outbox::BridgeOutbox;
use zeroclaw_infra::session_sqlite::SqliteSessionBackend;

const DELEGATION: &str = "delegation";
const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
const MAX_BINDINGS: usize = 500;

/// Destination configuration owns the target; the startup data root owns this
/// gateway's stores (a changed root waits for the owning gateway to restart).
fn eligible(config: &Config, data_dir: &Path) -> bool {
    config.data_dir == data_dir
        && config.tachi.enabled
        && config
            .tachi
            .result_notification
            .as_ref()
            .is_some_and(|target| {
                target.validate().is_ok() && config.gateway.bridges.contains_key(&target.bridge)
            })
}

/// The outcome a terminal projection maps to, or `None` when the run is not
/// (yet) attention-worthy. `InputRequired` counts only when the daemon closed
/// it as `partial` — the same terminal rule the status projections use.
fn terminal_outcome(status: &RunStatus) -> Option<&'static str> {
    match status.state {
        RunState::Completed => Some("completed"),
        RunState::Failed => Some("failed"),
        RunState::Canceled => Some("canceled"),
        RunState::InputRequired if status.closure_kind.as_deref() == Some("partial") => {
            Some("partial")
        }
        _ => None,
    }
}

fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// One reconcile pass. `fetch_status` is injected so tests drive terminal,
/// non-terminal and failing queries without a daemon; production passes the
/// TachiStaffClient adapter. Bindings whose agent is disabled are skipped
/// (the dispatch may still run — only the notification is withheld).
pub(crate) async fn reconcile(
    config: &RwLock<Config>,
    data_dir: &Path,
    stop: &tokio_util::sync::CancellationToken,
    fetch_status: &(
         dyn Fn(&str, &str) -> BoxFuture<'static, anyhow::Result<RunStatus>> + Send + Sync
     ),
) -> anyhow::Result<usize> {
    if stop.is_cancelled() {
        return Ok(0);
    }
    if !eligible(&config.read(), data_dir) {
        return Ok(0);
    }
    // Scope the session handle: it must not live across the status awaits
    // below (keeps this future Send). The bounded listing is fully owned.
    let bindings = {
        let sessions = SqliteSessionBackend::new(data_dir)?;
        sessions.list_dispatch_bindings(MAX_BINDINGS)?
    };
    let outbox = BridgeOutbox::shared(data_dir)?;
    let mut notified = 0;
    for (agent, request_id, dispatch_id) in bindings {
        if stop.is_cancelled() {
            return Ok(notified);
        }
        if !config.read().agents.get(&agent).is_some_and(|a| a.enabled) {
            continue;
        }
        // A failing status query is an UNKNOWN outcome, not a notification:
        // the next pass retries and only a terminal receipt ever enqueues.
        let Ok(status) = fetch_status(&agent, &dispatch_id).await else {
            continue;
        };
        let Some(outcome) = terminal_outcome(&status) else {
            continue;
        };
        let content = zeroclaw_runtime::i18n::get_required_cli_string_with_args(
            "delegation-result-notice",
            &[
                ("request", short(&request_id)),
                ("outcome", outcome),
                ("dispatch", short(&dispatch_id)),
            ],
        );
        // Hold the current destination through enqueue: a simultaneous config
        // swap cannot send to a superseded target.
        let current = config.read();
        if stop.is_cancelled() || !eligible(&current, data_dir) {
            return Ok(notified);
        }
        let target = current
            .tachi
            .result_notification
            .as_ref()
            .expect("eligible() checked the target");
        outbox.enqueue_source(
            &target.bridge,
            &target.recipient,
            target.thread_id.as_deref(),
            &content,
            DELEGATION,
            &request_id,
            outcome,
        )?;
        notified += 1;
    }
    Ok(notified)
}

/// Delegation notices deliver only while this opt-in destination still
/// matches; queued-but-not-yet-sent rows are withheld when the target, the
/// agent's enablement or the section disappears. Mirrors the review gate.
pub(crate) fn permits_delivery(
    config: &Config,
    outbox: &BridgeOutbox,
    row: &zeroclaw_infra::bridge_outbox::OutboxItem,
) -> bool {
    if row.source_kind != DELEGATION {
        return true;
    }
    outbox.is_for_data_dir(&config.data_dir)
        && config
            .agents
            .get(&row.source_id)
            .is_some_and(|agent| agent.enabled)
        && config
            .tachi
            .result_notification
            .as_ref()
            .is_some_and(|target| {
                target.validate().is_ok()
                    && config.gateway.bridges.contains_key(&target.bridge)
                    && target.bridge == row.bridge
                    && target.recipient == row.to
                    && target.thread_id == row.thread_id
            })
}

pub(crate) struct DelegationNotificationTask(tokio::task::JoinHandle<()>);

impl Drop for DelegationNotificationTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Owned by the gateway serve future; cancelled on shutdown. One notice per
/// canonical (request, outcome); delivery retries are the outbox's.
pub(crate) fn start(
    config: Arc<RwLock<Config>>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> DelegationNotificationTask {
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
            // The live status adapter: a client per agent from the current
            // [tachi] section; no network traffic until a call, and failures
            // inside reconcile are skipped, never fatal.
            let config_for_fetch = Arc::clone(&config);
            let fetch = move |agent: &str, dispatch: &str| {
                let tachi = config_for_fetch.read().tachi.clone();
                let agent = agent.to_string();
                let dispatch = dispatch.to_string();
                Box::pin(async move {
                    let client =
                        zeroclaw_tools::tachi_staff::TachiStaffClient::from_config(&tachi, &agent)?;
                    client.status(&dispatch).await.map_err(anyhow::Error::from)
                }) as BoxFuture<'static, anyhow::Result<RunStatus>>
            };
            if let Err(error) = reconcile(&config, &data_dir, &stop, &fetch).await {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"error": format!("{error:#}")})),
                    "delegation result observation pass failed"
                );
            }
            tokio::select! {
                _ = interval.tick() => {}
                _ = shutdown.changed() => break,
            }
        }
        let _ = stop_on_drop;
    });
    DelegationNotificationTask(task)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;

    fn fixture_config() -> (tempfile::TempDir, Config) {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = Config {
            data_dir: tmp.path().to_path_buf(),
            ..Config::default()
        };
        config.tachi.enabled = true;
        config.tachi.result_notification =
            Some(zeroclaw_config::tachi::DelegationNotificationConfig {
                bridge: "tg".into(),
                recipient: "owner".into(),
                thread_id: None,
            });
        config.gateway.bridges.insert(
            "tg".into(),
            zeroclaw_config::schema::GatewayBridgeConfig {
                token_hash: "ab".repeat(32),
                ..Default::default()
            },
        );
        config.agents.insert("butler".into(), Default::default());
        (tmp, config)
    }

    fn seed_binding(backend: &SqliteSessionBackend, agent: &str, request: &str, dispatch: &str) {
        backend
            .claim_delegation_request(agent, request, "request-digest", "route-digest")
            .expect("claim");
        backend
            .bind_delegation_request(agent, request, "request-digest", "route-digest", dispatch)
            .expect("bind");
    }

    fn status(state: RunState) -> RunStatus {
        RunStatus {
            dispatch_id: "d".into(),
            state,
            revision: None,
            closure_kind: None,
            result_written: None,
            updated_at: None,
            read_projection: None,
        }
    }

    fn delegation_rows(tmp: &tempfile::TempDir) -> Vec<serde_json::Value> {
        BridgeOutbox::shared(tmp.path())
            .unwrap()
            .list("tg", 0, 100)
            .unwrap()
            .into_iter()
            .filter(|item| item["source_kind"] == "delegation")
            .collect()
    }

    #[tokio::test]
    async fn terminal_states_notify_once_and_unknown_never_notifies() {
        let (tmp, config) = fixture_config();
        let shared = RwLock::new(config);
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        seed_binding(&backend, "butler", "req-00000001", "disp-1");
        seed_binding(&backend, "butler", "req-00000002", "disp-2");
        seed_binding(&backend, "butler", "req-00000003", "disp-3");
        let stop = tokio_util::sync::CancellationToken::new();

        let fetch = |agent: &str, dispatch: &str| {
            assert_eq!(agent, "butler");
            let dispatch = dispatch.to_string();
            async move {
                match dispatch.as_str() {
                    "disp-1" => Ok(status(RunState::Completed)),
                    "disp-2" => Ok(status(RunState::Working)),
                    "disp-3" => Err(anyhow::Error::msg("daemon unreachable")),
                    _ => unreachable!(),
                }
            }
            .boxed()
        };
        let notified = reconcile(&shared, tmp.path(), &stop, &fetch).await.unwrap();
        assert_eq!(notified, 1, "only the terminal run notifies");

        let rows = delegation_rows(&tmp);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["source_id"], "req-00000001");
        assert_eq!(rows[0]["event_id"], "completed");
        // Control views deliberately do not expose content; delivery_state
        // starts accepted and the notification itself rides the payload row.
        assert_eq!(rows[0]["delivery_state"], "accepted");

        // Repeated observation of the same terminal state is deduplicated.
        let again = reconcile(&shared, tmp.path(), &stop, &fetch).await.unwrap();
        assert_eq!(again, 1);
        assert_eq!(
            delegation_rows(&tmp).len(),
            1,
            "same canonical identity must not create a second notification"
        );

        // A later, different outcome on the same request is a new event.
        let fetch_failed = |_agent: &str, dispatch: &str| {
            let dispatch = dispatch.to_string();
            async move {
                if dispatch == "disp-1" {
                    Ok(status(RunState::Failed))
                } else {
                    Err(anyhow::Error::msg("daemon unreachable"))
                }
            }
            .boxed()
        };
        reconcile(&shared, tmp.path(), &stop, &fetch_failed)
            .await
            .unwrap();
        assert_eq!(
            delegation_rows(&tmp).len(),
            2,
            "a distinct outcome is a distinct canonical event"
        );
    }

    #[tokio::test]
    async fn disabled_agent_and_uneigible_config_withhold_notifications() {
        let (tmp, mut config) = fixture_config();
        config.agents.insert(
            "butler".into(),
            zeroclaw_config::schema::AliasedAgentConfig {
                enabled: false,
                ..Default::default()
            },
        );
        let shared = RwLock::new(config.clone());
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        seed_binding(&backend, "butler", "req-00000001", "disp-1");
        let stop = tokio_util::sync::CancellationToken::new();
        let fetch =
            |_agent: &str, _dispatch: &str| async { Ok(status(RunState::Completed)) }.boxed();
        assert_eq!(
            reconcile(&shared, tmp.path(), &stop, &fetch).await.unwrap(),
            0
        );

        // Eligibility off entirely: no outbox write, no error.
        config.tachi.enabled = false;
        let shared = RwLock::new(config);
        assert_eq!(
            reconcile(&shared, tmp.path(), &stop, &fetch).await.unwrap(),
            0
        );
    }

    #[test]
    fn partial_input_required_is_attention_worthy() {
        let mut partial = status(RunState::InputRequired);
        partial.closure_kind = Some("partial".into());
        assert_eq!(terminal_outcome(&partial), Some("partial"));
        let blocked = status(RunState::InputRequired);
        assert_eq!(terminal_outcome(&blocked), None);
        assert_eq!(terminal_outcome(&status(RunState::Submitted)), None);
    }
}

//! #63 delegated-result delivery: observe terminal states of bound Tachi
//! dispatches and enqueue one attention notice per canonical outcome into the
//! existing bridge outbox. No second task table, notifier, policy, or store —
//! the outbox's `(source_kind, source_id, event_id)` identity de-duplicates,
//! the attention policy (#447) gates delivery, and execution state remains
//! owned by Tachi's receipts. A status query failure is an unknown outcome
//! and is never itself a notification.
//!
//! Identity rules this producer owes (review round 1 of #566):
//! - `source_id` is `agent \x1f request_id` — the outbox row carries BOTH the
//!   owning agent (for delivery gating and per-source mutes) and the request,
//!   so two agents sharing a request_id never collapse into one notification.
//! - Route provenance: a dispatch is only queried when the CURRENT `[tachi]`
//!   route fingerprint equals the binding's stored digest — the same
//!   `route_conflict` protection the interactive tools apply.
//! - The binding's `notified_outcome` marker outlives the outbox's dedup
//!   retention, so an evicted receipt can never resurrect a notification.
//! - Pagination rotates a rowid cursor across passes: bounded pages, no
//!   starvation of later rows.

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
const PAGE_SIZE: usize = 500;

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

/// The outbox source identity: owning agent + request, length-delimited so
/// no alias+request pair can alias another.
fn source_identity(agent: &str, request_id: &str) -> String {
    format!("{agent}\u{1f}{request_id}")
}

/// The owning agent encoded in a delegation outbox row's source identity.
fn owning_agent(source_id: &str) -> &str {
    source_id
        .split_once('\u{1f}')
        .map_or("", |(agent, _)| agent)
}

/// One reconcile pass: walk ONE bounded page of dispatch bindings past the
/// rotation cursor (a short page resets the cursor for the next pass), and
/// for each binding whose agent is enabled and whose route provenance matches
/// the current `[tachi]` route, ask the dispatch for its status and enqueue a
/// notice for a terminal outcome unless that outcome was already notified.
/// `fetch_status` is injected so tests drive terminal, non-terminal and
/// failing queries without a daemon.
pub(crate) async fn reconcile(
    config: &RwLock<Config>,
    data_dir: &Path,
    stop: &tokio_util::sync::CancellationToken,
    rotation: &mut i64,
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
    // below (keeps this future Send). The bounded page is fully owned; the
    // notified marker is written through a fresh short-lived handle.
    let page = {
        let sessions = SqliteSessionBackend::new(data_dir)?;
        sessions.list_dispatch_bindings_page(*rotation, PAGE_SIZE)?
    };
    if page.len() < PAGE_SIZE {
        *rotation = 0;
    } else {
        *rotation = page.last().expect("full page is nonempty").rowid;
    }
    let outbox = BridgeOutbox::shared(data_dir)?;
    let mut notified = 0;
    for row in &page {
        if stop.is_cancelled() {
            return Ok(notified);
        }
        if !config
            .read()
            .agents
            .get(&row.agent_alias)
            .is_some_and(|a| a.enabled)
        {
            continue;
        }
        // Route provenance: refuse to query a dispatch whose stored route
        // fingerprint does not match the CURRENT [tachi] route — the same
        // protection as the interactive tools' `route_conflict`. The config
        // read guard is scoped so it never crosses the status await.
        let route_matches = {
            let current = config.read();
            let fingerprint = zeroclaw_tools::tachi_delegation::route_fingerprint(
                &current.tachi,
                &row.agent_alias,
            );
            row.route_digest.as_deref() == Some(fingerprint.as_str())
        };
        if !route_matches {
            continue;
        }
        // A failing status query is an UNKNOWN outcome, not a notification:
        // the next pass retries and only a terminal receipt ever enqueues.
        let Ok(status) = fetch_status(&row.agent_alias, &row.dispatch_id).await else {
            continue;
        };
        let Some(outcome) = terminal_outcome(&status) else {
            continue;
        };
        if row.notified_outcome.as_deref() == Some(outcome) {
            continue;
        }
        let content = zeroclaw_runtime::i18n::get_required_cli_string_with_args(
            "delegation-result-notice",
            &[
                ("request", short(&row.request_id)),
                ("outcome", outcome),
                ("dispatch", short(&row.dispatch_id)),
            ],
        );
        // Hold the current destination through enqueue: a simultaneous config
        // swap cannot send to a superseded target. The owning agent's
        // eligibility is re-read AFTER the await so a mid-flight disable
        // withholds too.
        let current = config.read();
        if stop.is_cancelled() || !eligible(&current, data_dir) {
            return Ok(notified);
        }
        if !current
            .agents
            .get(&row.agent_alias)
            .is_some_and(|a| a.enabled)
        {
            continue;
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
            &source_identity(&row.agent_alias, &row.request_id),
            outcome,
        )?;
        // Persist the notified marker on the permanent claim BEFORE the next
        // binding: it outlives the outbox's dedup retention, so an evicted
        // receipt can never resurrect this notification on a later pass.
        SqliteSessionBackend::new(data_dir)?.mark_binding_notified(row.rowid, outcome)?;
        notified += 1;
    }
    Ok(notified)
}

/// Delegation notices deliver only while this opt-in destination still
/// matches; queued-but-not-yet-sent rows are withheld when the target, the
/// owning agent's enablement, or the section disappears. Mirrors the review
/// gate. The owning agent is parsed from the row's agent-scoped source
/// identity — never the request id.
pub(crate) fn permits_delivery(
    config: &Config,
    outbox: &BridgeOutbox,
    row: &zeroclaw_infra::bridge_outbox::OutboxItem,
) -> bool {
    if row.source_kind != DELEGATION {
        return true;
    }
    outbox.is_for_data_dir(&config.data_dir)
        && config.tachi.enabled
        && config
            .agents
            .get(owning_agent(&row.source_id))
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
/// canonical (agent, request, outcome); delivery retries are the outbox's.
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
        let mut rotation: i64 = 0;
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
            if let Err(error) = reconcile(&config, &data_dir, &stop, &mut rotation, &fetch).await {
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
    use zeroclaw_infra::session_sqlite::SqliteSessionBackend;
    use zeroclaw_tools::tachi_delegation::route_fingerprint;

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

    /// Seed a binding under the CURRENT route so the observer's provenance
    /// check passes (pass `None` to seed a stale route for mismatch tests).
    fn seed_binding(
        backend: &SqliteSessionBackend,
        config: &Config,
        agent: &str,
        request: &str,
        dispatch: &str,
        route: Option<&str>,
    ) {
        let digest = route
            .unwrap_or(&route_fingerprint(&config.tachi, agent))
            .to_string();
        backend
            .claim_delegation_request(agent, request, "request-digest", &digest)
            .expect("claim");
        backend
            .bind_delegation_request(agent, request, "request-digest", &digest, dispatch)
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

    fn completed_fetch(
        agent: &str,
        dispatch: &str,
    ) -> BoxFuture<'static, anyhow::Result<RunStatus>> {
        let _ = (agent, dispatch);
        async { Ok(status(RunState::Completed)) }.boxed()
    }

    #[tokio::test]
    async fn terminal_states_notify_once_agents_are_scoped_and_unknown_never_notifies() {
        let (tmp, mut config) = fixture_config();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        // Two agents sharing the SAME request id must produce two distinct
        // notifications (agent-scoped identity).
        config.agents.insert("maid".into(), Default::default());
        let shared = RwLock::new(config);
        seed_binding(
            &backend,
            &shared.read(),
            "butler",
            "req-00000001",
            "disp-1",
            None,
        );
        seed_binding(
            &backend,
            &shared.read(),
            "maid",
            "req-00000001",
            "disp-2",
            None,
        );
        seed_binding(
            &backend,
            &shared.read(),
            "butler",
            "req-00000002",
            "disp-3",
            None,
        );
        seed_binding(
            &backend,
            &shared.read(),
            "butler",
            "req-00000003",
            "disp-4",
            None,
        );
        let stop = tokio_util::sync::CancellationToken::new();
        let mut rotation = 0;

        let fetch = |agent: &str, dispatch: &str| {
            let (agent, dispatch) = (agent.to_string(), dispatch.to_string());
            async move {
                match (agent.as_str(), dispatch.as_str()) {
                    (_, "disp-4") => Err(anyhow::Error::msg("daemon unreachable")),
                    (_, "disp-3") => Ok(status(RunState::Working)),
                    _ => Ok(status(RunState::Completed)),
                }
            }
            .boxed()
        };
        let notified = reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
            .await
            .unwrap();
        assert_eq!(notified, 2, "only terminal runs notify");

        let rows = delegation_rows(&tmp);
        assert_eq!(rows.len(), 2, "two agents, same request id, two rows");
        assert_eq!(rows[0]["source_id"], "butler\u{1f}req-00000001");
        assert_eq!(rows[1]["source_id"], "maid\u{1f}req-00000001");
        assert_eq!(rows[0]["event_id"], "completed");
        assert_eq!(rows[0]["delivery_state"], "accepted");

        // Re-observation is suppressed by the binding's notified marker even
        // if the outbox's own dedup receipts were evicted entirely.
        let evicted = BridgeOutbox::shared(tmp.path()).unwrap();
        let _ = evicted; // rows deleted below through a fresh connection
        {
            let path = tmp.path().join("sessions").join("bridge_outbox.db");
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute("DELETE FROM bridge_outbox", []).unwrap();
        }
        let again = reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
            .await
            .unwrap();
        assert_eq!(again, 0, "notified marker outlives outbox retention");
        assert!(delegation_rows(&tmp).is_empty());

        // A later, different outcome on the same request is a new event.
        let fetch_failed = |agent: &str, dispatch: &str| {
            let (agent, dispatch) = (agent.to_string(), dispatch.to_string());
            async move {
                if dispatch == "disp-1" {
                    Ok(status(RunState::Failed))
                } else {
                    Err(anyhow::Error::msg("skip others"))
                }
            }
            .boxed()
        };
        reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch_failed)
            .await
            .unwrap();
        let rows = delegation_rows(&tmp);
        assert_eq!(
            rows.len(),
            1,
            "distinct outcome is a distinct canonical event"
        );
        assert_eq!(rows[0]["source_id"], "butler\u{1f}req-00000001");
        assert_eq!(rows[0]["event_id"], "failed");
    }

    #[tokio::test]
    async fn route_provenance_mismatch_and_pagination() {
        let (tmp, config) = fixture_config();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        // Stale route: the observer must refuse to query this dispatch even
        // though the fetcher would report it completed.
        seed_binding(
            &backend,
            &config,
            "butler",
            "req-stale",
            "disp-stale",
            Some("stale"),
        );
        // Fresh bindings for pagination.
        seed_binding(&backend, &config, "butler", "req-p1", "disp-p1", None);
        seed_binding(&backend, &config, "butler", "req-p2", "disp-p2", None);
        let shared = RwLock::new(config);
        let stop = tokio_util::sync::CancellationToken::new();
        let mut rotation = 0;
        let notified = reconcile(&shared, tmp.path(), &stop, &mut rotation, &|a, d| {
            completed_fetch(a, d)
        })
        .await
        .unwrap();
        assert_eq!(notified, 2, "stale route skipped, both fresh notified");
        let rows = delegation_rows(&tmp);
        assert_eq!(rows.len(), 2);
        assert!(
            !rows
                .iter()
                .any(|row| row["source_id"].as_str().unwrap().contains("req-stale"))
        );

        // Rotation cursor advanced and wrapped (page shorter than PAGE_SIZE).
        assert_eq!(rotation, 0, "short page resets the rotation cursor");
    }

    #[tokio::test]
    async fn disabled_agent_and_uneigible_config_withhold_notifications() {
        let (tmp, mut config) = fixture_config();
        let shared = RwLock::new(config.clone());
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        seed_binding(
            &backend,
            &shared.read(),
            "butler",
            "req-00000001",
            "disp-1",
            None,
        );
        let stop = tokio_util::sync::CancellationToken::new();
        let mut rotation = 0;

        // Agent disabled, section still eligible: zero enqueues.
        config.agents.insert(
            "butler".into(),
            zeroclaw_config::schema::AliasedAgentConfig {
                enabled: false,
                ..Default::default()
            },
        );
        let shared = RwLock::new(config.clone());
        assert_eq!(
            reconcile(&shared, tmp.path(), &stop, &mut rotation, &|a, d| {
                completed_fetch(a, d)
            })
            .await
            .unwrap(),
            0
        );

        // Agent re-enabled, section disabled: still zero, no error.
        config.agents.insert("butler".into(), Default::default());
        config.tachi.enabled = false;
        let shared = RwLock::new(config);
        assert_eq!(
            reconcile(&shared, tmp.path(), &stop, &mut rotation, &|a, d| {
                completed_fetch(a, d)
            })
            .await
            .unwrap(),
            0
        );
    }

    #[test]
    fn permits_delivery_gates_on_owning_agent_and_section() {
        let (tmp, mut config) = fixture_config();
        let outbox = BridgeOutbox::shared(tmp.path()).unwrap();
        let row =
            |source_id: &str, bridge: &str, to: &str| zeroclaw_infra::bridge_outbox::OutboxItem {
                seq: 1,
                id: "id".into(),
                bridge: bridge.into(),
                to: to.into(),
                thread_id: None,
                content: String::new(),
                source_kind: DELEGATION.into(),
                source_id: source_id.into(),
                event_id: "completed".into(),
                delivery_state: "accepted".into(),
                snoozed_until: None,
                expires_at: 0,
            };
        assert!(permits_delivery(
            &config,
            &outbox,
            &row("butler\u{1f}req-1", "tg", "owner")
        ));
        // Wrong owner scoping: an agent that does not exist.
        assert!(!permits_delivery(
            &config,
            &outbox,
            &row("ghost\u{1f}req-1", "tg", "owner")
        ));
        // Disabled owner withholds even though the request ran.
        config.agents.insert(
            "butler".into(),
            zeroclaw_config::schema::AliasedAgentConfig {
                enabled: false,
                ..Default::default()
            },
        );
        assert!(!permits_delivery(
            &config,
            &outbox,
            &row("butler\u{1f}req-1", "tg", "owner")
        ));
        config.agents.insert("butler".into(), Default::default());
        // Section off withholds queued-unsent rows.
        config.tachi.enabled = false;
        assert!(!permits_delivery(
            &config,
            &outbox,
            &row("butler\u{1f}req-1", "tg", "owner")
        ));
        config.tachi.enabled = true;
        // Mismatched destination withholds.
        assert!(!permits_delivery(
            &config,
            &outbox,
            &row("butler\u{1f}req-1", "tg", "other")
        ));
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

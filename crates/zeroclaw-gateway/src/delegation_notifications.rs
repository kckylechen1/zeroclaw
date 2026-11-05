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

/// Status fetcher: (agent, dispatch, validated route snapshot) → status.
pub(crate) type StatusFetch<'a> = &'a (
        dyn Fn(
    &str,
    &str,
    &zeroclaw_config::tachi::TachiConfig,
) -> BoxFuture<'static, anyhow::Result<RunStatus>>
            + Send
            + Sync
    );

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

/// Scan position across passes: `cursor` is the last rowid examined; a wrap
/// happens ONLY on a short page (natural end of the rowid order). Monotone
/// consumption cannot starve any row: new claims always land at rowids ABOVE
/// every existing one, so unvisited older rows are always consumed before
/// newer inserts, and the sweep reaches its natural end in
/// ceil(total/page_size) passes. (A restart resets the cursor — coverage is
/// delayed, never lost.) Tests shrink `page_size` to exercise pagination.
pub(crate) struct Rotation {
    pub cursor: i64,
    pub page_size: usize,
    /// Full pages seen since the last wrap.
    pub full_pages_since_wrap: usize,
    /// The table's highest rowid at sweep start (0 = new sweep). A forced
    /// rewind fires only when a page contains rows BEYOND this extent —
    /// churn is outrunning the sweep — so a finite static table always
    /// walks to its natural end at ANY depth, while sustained insertion
    /// bounds head re-observation: the wait is finite and grows with sweep
    /// depth (the rewind fires once the sweep passes the recorded extent
    /// with the counter at threshold), not a fixed every-N-pages cadence.
    pub sweep_max_rowid: i64,
}

const REWIND_PASSES: usize = 8;

impl Rotation {
    fn wrap(&mut self) {
        self.cursor = 0;
        self.full_pages_since_wrap = 0;
        self.sweep_max_rowid = 0;
    }
}

impl Default for Rotation {
    fn default() -> Self {
        Self {
            cursor: 0,
            page_size: PAGE_SIZE,
            full_pages_since_wrap: 0,
            sweep_max_rowid: 0,
        }
    }
}

/// The persistent notified marker: a bounded, `\x1e`-separated list of
/// `outcome \x1f bridge \x1f recipient` entries (most recent last). Every
/// destination this outcome was already notified to is remembered, so a
/// target flip A→B→A does not re-notify A after outbox retention evicted A's
/// receipt — while a genuinely new destination still gets its notice. The
/// list is capped (oldest entries dropped) so a lifetime of target churn
/// cannot grow it unboundedly.
const MARKER_LIST_CAP: usize = 8;

fn marker_entry(outcome: &str, bridge: &str, recipient: &str) -> String {
    format!("{outcome}\u{1f}{bridge}\u{1f}{recipient}")
}

/// True when the marker list already contains `entry`.
fn marker_contains(list: &str, entry: &str) -> bool {
    list.split('\u{1e}').any(|item| item == entry)
}

/// Append `entry` to the list, dropping the oldest beyond the cap.
fn marker_push(list: &str, entry: &str) -> String {
    let mut items: Vec<&str> = list
        .split('\u{1e}')
        .filter(|item| !item.is_empty())
        .collect();
    items.push(entry);
    let start = items.len().saturating_sub(MARKER_LIST_CAP);
    items[start..].join("\u{1e}")
}

/// One reconcile pass: walk ONE bounded page of dispatch bindings past the
/// rotation cursor, and for each binding whose agent is enabled and whose
/// route provenance matches, ask the dispatch for its status (through the
/// SAME owned route snapshot the fingerprint was validated against — no
/// TOCTOU between validation and request) and enqueue a notice for a new
/// terminal outcome. `fetch_status` is injected so tests drive terminal,
/// non-terminal and failing queries without a daemon.
pub(crate) async fn reconcile(
    config: &RwLock<Config>,
    data_dir: &Path,
    stop: &tokio_util::sync::CancellationToken,
    rotation: &mut Rotation,
    fetch_status: StatusFetch<'_>,
) -> anyhow::Result<usize> {
    if stop.is_cancelled() {
        return Ok(0);
    }
    if !eligible(&config.read(), data_dir) {
        return Ok(0);
    }
    // Scope the session handle: it must not live across the status awaits
    // below (keeps this future Send). The bounded page is fully owned; the
    // notified markers are written through one short-lived handle AFTER the
    // loop (batched, once per pass).
    let (page, table_extent) = {
        let sessions = SqliteSessionBackend::new(data_dir)?;
        let page = sessions.list_dispatch_bindings_page(rotation.cursor, rotation.page_size)?;
        (page, sessions.max_binding_rowid()?)
    };
    if page.len() < rotation.page_size {
        rotation.wrap();
    } else {
        if rotation.sweep_max_rowid == 0 {
            rotation.sweep_max_rowid = table_extent;
        }
        let churn_outran_sweep =
            page.last().expect("full page is nonempty").rowid > rotation.sweep_max_rowid;
        if churn_outran_sweep && rotation.full_pages_since_wrap >= REWIND_PASSES {
            // Forced rewind: insertions beyond the sweep's original extent
            // would otherwise postpone the natural wrap forever, and head
            // rows would never be RE-observed after completing.
            rotation.wrap();
        } else {
            rotation.cursor = page.last().expect("full page is nonempty").rowid;
            rotation.full_pages_since_wrap += 1;
        }
    }
    let outbox = BridgeOutbox::shared(data_dir)?;
    let mut notified = 0;
    let mut marks: Vec<(i64, String)> = Vec::new();
    let mut enqueue_failure: Option<anyhow::Error> = None;
    for row in &page {
        if stop.is_cancelled() {
            break;
        }
        // Route provenance AND the request snapshot come from ONE owned
        // config read: the fingerprint is validated against this snapshot and
        // the same snapshot is handed to the fetcher, so a config swap mid-row
        // cannot validate on route A and query through route B.
        let (agent_enabled, route_snapshot, route_matches) = {
            let current = config.read();
            let enabled = current
                .agents
                .get(&row.agent_alias)
                .is_some_and(|a| a.enabled);
            let snapshot = current.tachi.clone();
            let fingerprint =
                zeroclaw_tools::tachi_delegation::route_fingerprint(&snapshot, &row.agent_alias);
            (
                enabled,
                snapshot,
                row.route_digest.as_deref() == Some(fingerprint.as_str()),
            )
        };
        if !agent_enabled || !route_matches {
            continue;
        }
        // A failing status query is an UNKNOWN outcome, not a notification:
        // the next pass retries and only a terminal receipt ever enqueues.
        let Ok(status) = fetch_status(&row.agent_alias, &row.dispatch_id, &route_snapshot).await
        else {
            continue;
        };
        let Some(outcome) = terminal_outcome(&status) else {
            continue;
        };
        // Hold the current destination through enqueue: a simultaneous config
        // swap cannot send to a superseded target. The owning agent's
        // eligibility is re-read AFTER the await so a mid-flight disable
        // withholds too.
        let current = config.read();
        if stop.is_cancelled() || !eligible(&current, data_dir) {
            break;
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
        // Destination-scoped suppression: skip only when THIS outcome was
        // already notified to THIS destination (across the whole remembered
        // target history, so A→B→A stays silent after retention eviction).
        let entry = marker_entry(outcome, &target.bridge, &target.recipient);
        if row
            .notified_outcome
            .as_deref()
            .is_some_and(|list| marker_contains(list, &entry))
        {
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
        // An enqueue failure (e.g. outbox capacity) must not discard the
        // markers of rows already enqueued this pass: record the error, stop
        // enqueueing further rows, and still flush what succeeded.
        if let Err(error) = outbox.enqueue_source(
            &target.bridge,
            &target.recipient,
            target.thread_id.as_deref(),
            &content,
            DELEGATION,
            &source_identity(&row.agent_alias, &row.request_id),
            outcome,
        ) {
            enqueue_failure = Some(error);
            break;
        }
        marks.push((row.rowid, entry));
        notified += 1;
    }
    // Batched marker persistence: one short-lived handle per pass, after the
    // awaits — reached on success, cancellation AND enqueue failure alike.
    // A crash between enqueue and this write is bounded by the outbox dedup
    // window (documented residual: at most one re-notification per outcome
    // after retention eviction, requiring a crash in that window).
    if !marks.is_empty() {
        let sessions = SqliteSessionBackend::new(data_dir)?;
        for (rowid, entry) in &marks {
            // Per-row read with a propagated error: treating a read failure
            // as empty history would silently drop remembered destinations.
            let existing = sessions.binding_notified_marker(*rowid)?;
            let merged = marker_push(existing.as_deref().unwrap_or(""), entry);
            sessions.mark_binding_notified(*rowid, &merged)?;
        }
    }
    if let Some(error) = enqueue_failure {
        return Err(error);
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
        let mut rotation = Rotation::default();
        loop {
            if *shutdown.borrow() {
                break;
            }
            // The live status adapter: a client per agent built from the
            // VALIDATED route snapshot handed in by reconcile (never a fresh
            // config read — that would reintroduce the validate/query race).
            // No network traffic until a call; failures are skipped, never fatal.
            let fetch = move |agent: &str,
                              dispatch: &str,
                              route: &zeroclaw_config::tachi::TachiConfig| {
                let route = route.clone();
                let agent = agent.to_string();
                let dispatch = dispatch.to_string();
                Box::pin(async move {
                    let client =
                        zeroclaw_tools::tachi_staff::TachiStaffClient::from_config(&route, &agent)?;
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
        _agent: &str,
        _dispatch: &str,
        _route: &zeroclaw_config::tachi::TachiConfig,
    ) -> BoxFuture<'static, anyhow::Result<RunStatus>> {
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
        let mut rotation = Rotation::default();

        let fetch = |_agent: &str, dispatch: &str, _route: &zeroclaw_config::tachi::TachiConfig| {
            let dispatch = dispatch.to_string();
            async move {
                match dispatch.as_str() {
                    "disp-4" => Err(anyhow::Error::msg("daemon unreachable")),
                    "disp-3" => Ok(status(RunState::Working)),
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
        let fetch_failed =
            |_agent: &str, dispatch: &str, _route: &zeroclaw_config::tachi::TachiConfig| {
                let dispatch = dispatch.to_string();
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
    async fn stale_route_is_never_queried_and_pagination_wraps() {
        let (tmp, config) = fixture_config();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        // Stale route: the observer must refuse to QUERY this dispatch at
        // all (not merely skip the enqueue) — proven by a fetch counter.
        seed_binding(
            &backend,
            &config,
            "butler",
            "req-stale",
            "disp-stale",
            Some("stale"),
        );
        // Fresh bindings; page size 2 forces multi-page coverage.
        for index in 1..=3 {
            seed_binding(
                &backend,
                &config,
                "butler",
                &format!("req-p{index}"),
                &format!("disp-p{index}"),
                None,
            );
        }
        let shared = RwLock::new(config);
        let stop = tokio_util::sync::CancellationToken::new();
        let queried = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut rotation = Rotation {
            cursor: 0,
            page_size: 2,
            full_pages_since_wrap: 0,
            sweep_max_rowid: 0,
        };
        let counter = std::sync::Arc::clone(&queried);
        let fetch =
            move |_agent: &str, _dispatch: &str, _route: &zeroclaw_config::tachi::TachiConfig| {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move { Ok(status(RunState::Completed)) }.boxed()
            };
        // First page: rows 1-2 (stale first, then p1). The stale dispatch
        // must not be counted.
        let notified = reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
            .await
            .unwrap();
        assert_eq!(notified, 1);
        assert_eq!(queried.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(rotation.cursor > 0, "full page advances the cursor");

        // Second page: p2 AND p3 (page size 2) — both terminal, both notified.
        let notified = reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
            .await
            .unwrap();
        assert_eq!(notified, 2, "p2 and p3 terminal on the second page");
        assert!(rotation.cursor > 0);
        // Third pass: page past the end is short → wraps to 0, nothing new.
        let notified = reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
            .await
            .unwrap();
        assert_eq!(notified, 0, "short page wraps with nothing left");
        assert_eq!(rotation.cursor, 0, "short page resets the cursor");
        // Fourth pass from 0: (stale, p1) — stale is NEVER queried; p1 is
        // queried but marker-suppressed.
        let notified = reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
            .await
            .unwrap();
        assert_eq!(notified, 0, "stale row never notifies; p1 already marked");
        // Total queries: p1 once per sweep-window visit + p2 + p3 — never stale.
        assert_eq!(queried.load(std::sync::atomic::Ordering::SeqCst), 4);
        // Wait: p3's second pass queried p3 only (rows after cursor 4 = stale? no:
        // rows are (stale, p1, p2, p3) by rowid; page from cursor>0 picks the rest.
        let rows = delegation_rows(&tmp);
        assert_eq!(rows.len(), 3, "three fresh terminals, stale absent");
        assert!(
            !rows
                .iter()
                .any(|row| row["source_id"].as_str().unwrap().contains("req-stale"))
        );
    }

    #[tokio::test]
    async fn static_table_walks_to_full_depth_and_churn_forces_the_rewind() {
        let (tmp, config) = fixture_config();
        let shared = RwLock::new(config);
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        for index in 0..18 {
            seed_binding(
                &backend,
                &shared.read(),
                "butler",
                &format!("req-r{index:02}"),
                &format!("disp-r{index:02}"),
                None,
            );
        }
        let stop = tokio_util::sync::CancellationToken::new();
        let queries = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut rotation = Rotation {
            cursor: 0,
            page_size: 2,
            full_pages_since_wrap: 0,
            sweep_max_rowid: 0,
        };
        let counter = std::sync::Arc::clone(&queries);
        let fetch =
            move |_agent: &str, _dispatch: &str, _route: &zeroclaw_config::tachi::TachiConfig| {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Ok(status(RunState::Completed)) }.boxed()
            };

        // STATIC table, 9 full pages: the rewind must NOT fire — the sweep
        // walks every page to its natural end regardless of depth.
        for _ in 0..9 {
            reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
                .await
                .unwrap();
            assert!(rotation.cursor > 0, "static pages always advance");
        }
        assert_eq!(
            queries.load(std::sync::atomic::Ordering::SeqCst),
            18,
            "all 18 rows covered — the tail is reachable at any depth"
        );
        let _ = reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch).await;
        assert_eq!(rotation.cursor, 0, "the empty page wraps naturally");

        // CHURN: with the counter already high, insert rows BEYOND the
        // sweep's original extent — the next full page must force the wrap.
        for index in 18..22 {
            seed_binding(
                &backend,
                &shared.read(),
                "butler",
                &format!("req-c{index:02}"),
                &format!("disp-c{index:02}"),
                None,
            );
        }
        // One pass re-walks from the top (fresh sweep, extent 22).
        reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
            .await
            .unwrap();
        assert!(rotation.sweep_max_rowid > 0);
        // Simulate the sweep being outrun: park the cursor AT the sweep's
        // original extent, set the counter at the threshold, and insert
        // beyond it — the next page is entirely churn.
        rotation.cursor = rotation.sweep_max_rowid;
        rotation.full_pages_since_wrap = REWIND_PASSES;
        for index in 22..26 {
            seed_binding(
                &backend,
                &shared.read(),
                "butler",
                &format!("req-x{index:02}"),
                &format!("disp-x{index:02}"),
                None,
            );
        }
        reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
            .await
            .unwrap();
        assert_eq!(
            rotation.cursor, 0,
            "a full page beyond the sweep extent forces the rewind"
        );
        assert_eq!(rotation.full_pages_since_wrap, 0);
    }

    #[tokio::test]
    async fn target_flip_a_to_b_to_a_stays_silent_after_retention_loss() {
        let (tmp, config) = fixture_config();
        let shared = RwLock::new(config.clone());
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        seed_binding(
            &backend,
            &shared.read(),
            "butler",
            "req-flip",
            "disp-flip",
            None,
        );
        let stop = tokio_util::sync::CancellationToken::new();

        async fn notify_once(
            config: Config,
            tmp: &tempfile::TempDir,
            stop: &tokio_util::sync::CancellationToken,
        ) -> usize {
            let shared = RwLock::new(config);
            let mut rotation = Rotation::default();
            // Evict the outbox before every observation: only the marker
            // list can suppress re-notification. Ensure the schema exists
            // first (the very first eviction precedes any enqueue).
            BridgeOutbox::shared(tmp.path()).unwrap();
            let path = tmp.path().join("sessions").join("bridge_outbox.db");
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute("DELETE FROM bridge_outbox", []).unwrap();
            drop(conn);
            reconcile(&shared, tmp.path(), stop, &mut rotation, &|a, d, r| {
                completed_fetch(a, d, r)
            })
            .await
            .unwrap()
        }

        // A → B → A with eviction each time: A once, B once, A silent.
        assert_eq!(
            notify_once(config.clone(), &tmp, &stop).await,
            1,
            "A notified"
        );
        let mut to_b = config.clone();
        to_b.tachi
            .result_notification
            .as_mut()
            .expect("target")
            .recipient = "owner-b".into();
        assert_eq!(
            notify_once(to_b, &tmp, &stop).await,
            1,
            "B notified (new destination)"
        );
        assert_eq!(
            notify_once(config, &tmp, &stop).await,
            0,
            "A stays silent: remembered across the flip despite eviction"
        );
    }

    #[tokio::test]
    async fn target_change_re_notifies_the_new_destination() {
        let (tmp, config) = fixture_config();
        let shared = RwLock::new(config.clone());
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        seed_binding(
            &backend,
            &shared.read(),
            "butler",
            "req-t1",
            "disp-t1",
            None,
        );
        let stop = tokio_util::sync::CancellationToken::new();
        let mut rotation = Rotation::default();
        // First destination notified + marked.
        reconcile(&shared, tmp.path(), &stop, &mut rotation, &|a, d, r| {
            completed_fetch(a, d, r)
        })
        .await
        .unwrap();
        assert_eq!(delegation_rows(&tmp).len(), 1);

        // Evict the outbox entirely (retention loss) AND change the target:
        // the destination-scoped marker must let the new target be notified.
        {
            let path = tmp.path().join("sessions").join("bridge_outbox.db");
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute("DELETE FROM bridge_outbox", []).unwrap();
        }
        let mut retargeted = config;
        retargeted
            .tachi
            .result_notification
            .as_mut()
            .expect("target")
            .recipient = "owner-new".into();
        let shared = RwLock::new(retargeted);
        let notified = reconcile(&shared, tmp.path(), &stop, &mut rotation, &|a, d, r| {
            completed_fetch(a, d, r)
        })
        .await
        .unwrap();
        assert_eq!(notified, 1, "a new destination is a new notification");
        let rows = delegation_rows(&tmp);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["recipient"], "owner-new");

        // Same destination again after another eviction: suppressed by the
        // now-destination-scoped marker.
        {
            let path = tmp.path().join("sessions").join("bridge_outbox.db");
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute("DELETE FROM bridge_outbox", []).unwrap();
        }
        let notified = reconcile(&shared, tmp.path(), &stop, &mut rotation, &|a, d, r| {
            completed_fetch(a, d, r)
        })
        .await
        .unwrap();
        assert_eq!(
            notified, 0,
            "same destination stays suppressed past eviction"
        );
        assert!(delegation_rows(&tmp).is_empty());
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
        let mut rotation = Rotation::default();

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
            reconcile(&shared, tmp.path(), &stop, &mut rotation, &|a, d, r| {
                completed_fetch(a, d, r)
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
            reconcile(&shared, tmp.path(), &stop, &mut rotation, &|a, d, r| {
                completed_fetch(a, d, r)
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

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
//! - A destination is `(bridge, recipient, thread_id)` in BOTH the notified
//!   marker and the outbox `event_id`, matching what `permits_delivery`
//!   gates on, so retargeting a queued notice to another thread of the same
//!   recipient enqueues a fresh notice instead of being suppressed (#566
//!   review).
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

/// The outbox source identity: owning agent + request. Admitted request ids
/// exclude the separator, so the final separator unambiguously splits them.
fn source_identity(agent: &str, request_id: &str) -> String {
    format!("{agent}\u{1f}{request_id}")
}

/// The owning agent encoded in a delegation outbox row's source identity.
fn owning_agent(source_id: &str) -> &str {
    source_id
        .rsplit_once('\u{1f}')
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
/// destination-scoped entries (most recent last). Every destination this
/// outcome was already notified to is remembered, so a target flip A→B→A
/// does not re-notify A after outbox retention evicted A's receipt — while a
/// genuinely new destination still gets its notice. The list is capped
/// (oldest entries dropped) so a lifetime of target churn cannot grow it
/// unboundedly.
///
/// A destination is `(bridge, recipient, thread_id)` — the SAME triple
/// [`permits_delivery`] gates on. Entries written before the thread became
/// part of the identity carry only `outcome \x1f bridge \x1f recipient`;
/// an existing legacy outbox receipt recovers its exact thread. Without
/// that evidence, retain conservative suppression for that recipient rather
/// than risk re-sending an expired or uncertain notification.
const MARKER_LIST_CAP: usize = 8;

struct MarkerUpdate {
    rowid: i64,
    entry: String,
    replaces: Option<String>,
}

/// Encode an optional thread so `None` and every `Some(_)` are distinct and
/// no thread value can collide with another (the `=` prefix keeps an empty
/// thread distinct from "no thread").
fn thread_token(thread_id: Option<&str>) -> String {
    match thread_id {
        None => "-".to_string(),
        Some(thread) => format!("={thread}"),
    }
}

fn marker_entry(outcome: &str, bridge: &str, recipient: &str, thread_id: Option<&str>) -> String {
    format!(
        "{outcome}\u{1f}{bridge}\u{1f}{recipient}\u{1f}{}",
        thread_token(thread_id)
    )
}

/// The pre-thread (legacy) form of an entry, matched for compatibility.
fn legacy_marker_entry(outcome: &str, bridge: &str, recipient: &str) -> String {
    format!("{outcome}\u{1f}{bridge}\u{1f}{recipient}")
}

/// The outbox `event_id` for one outcome at one destination. The outbox
/// dedupes on `(bridge, recipient, source_kind, source_id, event_id)`, which
/// has no thread column, so the thread is folded into the event id: a notice
/// queued for thread A and a retarget to thread B of the SAME recipient are
/// two rows, each gated by [`permits_delivery`] on its own thread. The
/// thread-less destination is also explicit: a legacy threaded row used the
/// bare outcome, and must not absorb a new notice targeting no thread.
fn destination_event_id(outcome: &str, thread_id: Option<&str>) -> String {
    format!("{outcome}\u{1f}thread\u{1f}{}", thread_token(thread_id))
}

/// True when this outcome was already enqueued to this exact destination.
/// Legacy markers need the receipt lookup in `reconcile` before they can be
/// interpreted as an exact destination.
fn already_notified(
    list: &str,
    outcome: &str,
    bridge: &str,
    recipient: &str,
    thread_id: Option<&str>,
) -> bool {
    marker_contains(list, &marker_entry(outcome, bridge, recipient, thread_id))
}

/// True when the marker list already contains `entry`.
fn marker_contains(list: &str, entry: &str) -> bool {
    list.split('\u{1e}').any(|item| item == entry)
}

/// Append `entry` to the list, dropping the oldest beyond the cap.
fn marker_push(list: &str, entry: &str) -> String {
    let mut items: Vec<&str> = list
        .split('\u{1e}')
        .filter(|item| !item.is_empty() && *item != entry)
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
    let mut marks: Vec<MarkerUpdate> = Vec::new();
    let mut outbox_failure: Option<anyhow::Error> = None;
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
        // already notified to THIS destination — bridge, recipient AND
        // thread, the same triple permits_delivery gates on (across the whole remembered
        // target history, so A→B→A stays silent after retention eviction).
        let thread = target.thread_id.as_deref();
        let entry = marker_entry(outcome, &target.bridge, &target.recipient, thread);
        if row.notified_outcome.as_deref().is_some_and(|list| {
            already_notified(list, outcome, &target.bridge, &target.recipient, thread)
        }) {
            continue;
        }
        let source_id = source_identity(&row.agent_alias, &row.request_id);
        let legacy_entry = legacy_marker_entry(outcome, &target.bridge, &target.recipient);
        // The old producer marked at enqueue, not delivery, and used a bare
        // outcome event id even for threaded rows. Recover the old destination
        // from that receipt before interpreting its thread-blind marker. This
        // also handles a crash after enqueue but before the marker was saved.
        let legacy_receipt = match outbox.source_event(
            &target.bridge,
            &target.recipient,
            DELEGATION,
            &source_id,
            outcome,
        ) {
            Ok(receipt) => receipt,
            Err(error) => {
                outbox_failure = Some(error);
                break;
            }
        };
        if let Some(receipt) = legacy_receipt {
            marks.push(MarkerUpdate {
                rowid: row.rowid,
                entry: marker_entry(
                    outcome,
                    &target.bridge,
                    &target.recipient,
                    receipt.thread_id.as_deref(),
                ),
                replaces: Some(legacy_entry),
            });
            if receipt.thread_id.as_deref() == thread {
                // Reuse the original candidate/receipt. In particular, an
                // uncertain or sent attempt must never be automatically replayed.
                continue;
            }
            // A different configured thread is a new destination. Leave the
            // original row and its owner actions untouched, as for new-format
            // A→B retargeting; only the new destination may get a new row.
        } else if row
            .notified_outcome
            .as_deref()
            .is_some_and(|list| marker_contains(list, &legacy_entry))
        {
            // Retention has erased the evidence needed to identify the old
            // thread. Do not guess that an old notification was never attempted.
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
            &source_id,
            &destination_event_id(outcome, thread),
        ) {
            outbox_failure = Some(error);
            break;
        }
        marks.push(MarkerUpdate {
            rowid: row.rowid,
            entry,
            replaces: None,
        });
        notified += 1;
    }
    // Batched marker persistence: one short-lived handle per pass, after the
    // awaits — reached on success, cancellation AND enqueue failure alike.
    // A crash between enqueue and this write is bounded by the outbox dedup
    // window (documented residual: at most one re-notification per outcome
    // after retention eviction, requiring a crash in that window).
    if !marks.is_empty() {
        let sessions = SqliteSessionBackend::new(data_dir)?;
        for update in &marks {
            // Per-row read with a propagated error: treating a read failure
            // as empty history would silently drop remembered destinations.
            let existing = sessions.binding_notified_marker(update.rowid)?;
            let retained = existing
                .as_deref()
                .unwrap_or("")
                .split('\u{1e}')
                .filter(|entry| Some(*entry) != update.replaces.as_deref())
                .collect::<Vec<_>>()
                .join("\u{1e}");
            let merged = marker_push(&retained, &update.entry);
            sessions.mark_binding_notified(update.rowid, &merged)?;
        }
    }
    if let Some(error) = outbox_failure {
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
        assert_eq!(rows[0]["event_id"], "completed\u{1f}thread\u{1f}-");
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
        assert_eq!(rows[0]["event_id"], "failed\u{1f}thread\u{1f}-");
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

    fn with_thread(config: &Config, thread: Option<&str>) -> Config {
        let mut next = config.clone();
        next.tachi
            .result_notification
            .as_mut()
            .expect("target")
            .thread_id = thread.map(str::to_string);
        next
    }

    fn deliverable(config: &Config, tmp: &tempfile::TempDir) -> Vec<Option<String>> {
        let outbox = BridgeOutbox::shared(tmp.path()).unwrap();
        outbox
            .pending("tg", 0, 100)
            .unwrap()
            .into_iter()
            .filter(|row| row.source_kind == DELEGATION && permits_delivery(config, &outbox, row))
            .map(|row| row.thread_id)
            .collect()
    }

    #[tokio::test]
    async fn queued_notice_retargeted_to_another_thread_same_recipient_is_not_lost() {
        // Regression (#566 review): notice queued for thread A, NOT yet
        // delivered; the target moves to thread B of the SAME recipient.
        // A is withheld by permits_delivery, so B must get its own row —
        // neither the notified marker nor the outbox dedup may suppress it.
        let (tmp, base) = fixture_config();
        let thread_a = with_thread(&base, Some("thread-a"));
        let thread_b = with_thread(&base, Some("thread-b"));
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        seed_binding(&backend, &thread_a, "butler", "req-th", "disp-th", None);
        let stop = tokio_util::sync::CancellationToken::new();
        let mut rotation = Rotation::default();
        let fetch =
            |a: &str, d: &str, r: &zeroclaw_config::tachi::TachiConfig| completed_fetch(a, d, r);

        let shared = RwLock::new(thread_a.clone());
        assert_eq!(
            reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
                .await
                .unwrap(),
            1
        );
        assert_eq!(deliverable(&thread_a, &tmp), vec![Some("thread-a".into())]);

        // Retarget to thread B before A was delivered.
        let shared = RwLock::new(thread_b.clone());
        assert_eq!(
            reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
                .await
                .unwrap(),
            1,
            "thread B is a new destination"
        );
        assert_eq!(
            deliverable(&thread_b, &tmp),
            vec![Some("thread-b".into())],
            "exactly the B notice is deliverable under the B target"
        );
        let rows = delegation_rows(&tmp);
        assert_eq!(rows.len(), 2, "A and B rows coexist");
        assert_ne!(rows[0]["event_id"], rows[1]["event_id"]);

        // Steady state at B: no further rows.
        assert_eq!(
            reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
                .await
                .unwrap(),
            0
        );
        // Flip back to A: already marked → no new row, and the still-queued
        // A row becomes deliverable again (exactly one A notice).
        let shared = RwLock::new(thread_a.clone());
        assert_eq!(
            reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
                .await
                .unwrap(),
            0
        );
        assert_eq!(deliverable(&thread_a, &tmp), vec![Some("thread-a".into())]);
        assert_eq!(delegation_rows(&tmp).len(), 2);
    }

    #[tokio::test]
    async fn thread_less_and_threaded_targets_are_distinct_destinations() {
        let (tmp, base) = fixture_config();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        seed_binding(&backend, &base, "butler", "req-nt", "disp-nt", None);
        let stop = tokio_util::sync::CancellationToken::new();
        let mut rotation = Rotation::default();
        let fetch =
            |a: &str, d: &str, r: &zeroclaw_config::tachi::TachiConfig| completed_fetch(a, d, r);
        let shared = RwLock::new(base.clone());
        reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
            .await
            .unwrap();
        // New thread-less events cannot collide with a legacy threaded row.
        assert_eq!(
            delegation_rows(&tmp)[0]["event_id"],
            "completed\u{1f}thread\u{1f}-"
        );
        let shared = RwLock::new(with_thread(&base, Some("t")));
        assert_eq!(
            reconcile(&shared, tmp.path(), &stop, &mut rotation, &fetch)
                .await
                .unwrap(),
            1
        );
    }

    #[test]
    fn marker_destinations_are_thread_exact() {
        let list = marker_push("", &marker_entry("completed", "tg", "owner", Some("a")));
        assert!(already_notified(
            &list,
            "completed",
            "tg",
            "owner",
            Some("a")
        ));
        assert!(!already_notified(
            &list,
            "completed",
            "tg",
            "owner",
            Some("b")
        ));
        assert!(!already_notified(&list, "completed", "tg", "owner", None));
        // Empty thread is distinct from no thread.
        assert_ne!(
            marker_entry("completed", "tg", "owner", Some("")),
            marker_entry("completed", "tg", "owner", None)
        );
        assert_ne!(
            destination_event_id("completed", Some("")),
            destination_event_id("completed", None)
        );
    }

    fn legacy_notice_fixture(
        thread: Option<&str>,
        marked: bool,
    ) -> (tempfile::TempDir, Config, String, i64) {
        let (tmp, base) = fixture_config();
        let config = with_thread(&base, thread);
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        seed_binding(
            &backend,
            &config,
            "butler",
            "legacy-request",
            "legacy-dispatch",
            None,
        );
        let rowid = backend.list_dispatch_bindings_page(0, 1).unwrap()[0].rowid;
        let id = BridgeOutbox::shared(tmp.path())
            .unwrap()
            .enqueue_source(
                "tg",
                "owner",
                thread,
                "legacy notice",
                DELEGATION,
                "butler\u{1f}legacy-request",
                "completed",
            )
            .unwrap();
        if marked {
            backend
                .mark_binding_notified(rowid, "completed\u{1f}tg\u{1f}owner")
                .unwrap();
        }
        (tmp, config, id, rowid)
    }

    async fn observe(config: &Config, tmp: &tempfile::TempDir) -> usize {
        reconcile(
            &RwLock::new(config.clone()),
            tmp.path(),
            &tokio_util::sync::CancellationToken::new(),
            &mut Rotation::default(),
            &completed_fetch,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn legacy_pending_receipt_recovers_thread_before_retarget() {
        for (old_thread, new_thread) in [
            (Some("thread-a"), Some("thread-b")),
            (Some("thread-a"), None),
            (None, Some("thread-b")),
        ] {
            let (tmp, original, old_id, rowid) = legacy_notice_fixture(old_thread, true);
            let outbox = BridgeOutbox::shared(tmp.path()).unwrap();
            let old_row = outbox.pending("tg", 0, 100).unwrap().remove(0);
            let retargeted = with_thread(&original, new_thread);

            assert_eq!(observe(&retargeted, &tmp).await, 1);
            assert_eq!(
                deliverable(&retargeted, &tmp),
                vec![new_thread.map(str::to_string)],
                "the new target has one deliverable notice, including a target with no thread"
            );
            let rows = outbox.pending("tg", 0, 100).unwrap();
            assert_eq!(rows.len(), 2);
            assert_eq!(rows.iter().find(|row| row.id == old_id), Some(&old_row));
            assert_ne!(rows[0].event_id, rows[1].event_id);
            assert_eq!(observe(&retargeted, &tmp).await, 0);
            assert_eq!(observe(&original, &tmp).await, 0);
            assert_eq!(
                deliverable(&original, &tmp),
                vec![old_thread.map(str::to_string)],
                "returning to the old target resumes its original pending row"
            );

            let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
            let marker = backend.binding_notified_marker(rowid).unwrap().unwrap();
            assert!(!marker_contains(&marker, "completed\u{1f}tg\u{1f}owner"));
            for thread in [old_thread, new_thread] {
                assert!(already_notified(
                    &marker,
                    "completed",
                    "tg",
                    "owner",
                    thread
                ));
            }

            // Once the old destination is recovered, retention cannot turn
            // its legacy marker back into a wildcard over future destinations.
            assert!(
                outbox
                    .claim("tg", &old_id, chrono::Utc::now().timestamp())
                    .unwrap()
            );
            outbox.mark_sent("tg", &old_id).unwrap();
            assert!(outbox.ack("tg", &old_id).unwrap());
            let connection =
                rusqlite::Connection::open(tmp.path().join("sessions").join("bridge_outbox.db"))
                    .unwrap();
            connection
                .execute("DELETE FROM bridge_outbox WHERE id = ?1", [&old_id])
                .unwrap();
            assert_eq!(observe(&original, &tmp).await, 0);
            assert_eq!(
                observe(&with_thread(&original, Some("thread-c")), &tmp).await,
                1
            );
        }
    }

    #[tokio::test]
    async fn legacy_attempts_and_owner_actions_survive_recovery_without_replay() {
        for state in [
            "accepted",
            "sent",
            "unknown",
            "confirmed",
            "dismissed",
            "expired",
        ] {
            // With no marker, this is the crash-after-enqueue window. With
            // the old marker, it is an ordinary pre-upgrade notification.
            for marked in [false, true] {
                let (tmp, original, old_id, _) = legacy_notice_fixture(Some("thread-a"), marked);
                let outbox = BridgeOutbox::shared(tmp.path()).unwrap();
                let source_id = "butler\u{1f}legacy-request";
                let now = chrono::Utc::now().timestamp();
                match state {
                    "accepted" => {
                        outbox
                            .owner_action(
                                "tg",
                                "owner",
                                DELEGATION,
                                source_id,
                                &old_id,
                                "snooze",
                                Some(now + 3600),
                                "operator",
                            )
                            .unwrap();
                    }
                    "sent" | "unknown" | "confirmed" => {
                        assert!(outbox.claim("tg", &old_id, now).unwrap());
                        if state != "unknown" {
                            outbox.mark_sent("tg", &old_id).unwrap();
                        }
                        if state == "confirmed" {
                            assert!(outbox.ack("tg", &old_id).unwrap());
                        }
                    }
                    "dismissed" => {
                        outbox
                            .owner_action(
                                "tg", "owner", DELEGATION, source_id, &old_id, "dismiss", None,
                                "operator",
                            )
                            .unwrap();
                    }
                    "expired" => {
                        let connection = rusqlite::Connection::open(
                            tmp.path().join("sessions").join("bridge_outbox.db"),
                        )
                        .unwrap();
                        connection
                            .execute(
                                "UPDATE bridge_outbox SET expires_at = ?1 WHERE id = ?2",
                                rusqlite::params![now - 1, &old_id],
                            )
                            .unwrap();
                        outbox.purge_expired().unwrap();
                    }
                    _ => unreachable!(),
                }
                outbox
                    .owner_action(
                        "tg", "owner", DELEGATION, source_id, &old_id, "mute", None, "operator",
                    )
                    .unwrap();
                let before = outbox.inspect("tg", &old_id).unwrap().unwrap();
                assert_eq!(before["delivery_state"], state);
                assert_eq!(observe(&original, &tmp).await, 0);
                assert_eq!(
                    delegation_rows(&tmp).len(),
                    1,
                    "a recovered receipt never gets replayed"
                );
                assert_eq!(outbox.inspect("tg", &old_id).unwrap().unwrap(), before);

                let retargeted = with_thread(&original, Some("thread-b"));
                assert_eq!(observe(&retargeted, &tmp).await, 1);
                assert_eq!(outbox.inspect("tg", &old_id).unwrap().unwrap(), before);
                let new_row = outbox
                    .pending("tg", 0, 100)
                    .unwrap()
                    .into_iter()
                    .find(|row| row.thread_id.as_deref() == Some("thread-b"))
                    .unwrap();
                assert!(
                    !outbox.claim("tg", &new_row.id, now).unwrap(),
                    "source mute still applies at the new thread"
                );
                assert_eq!(observe(&original, &tmp).await, 0);
            }
        }
    }

    #[tokio::test]
    async fn legacy_marker_without_receipt_keeps_conservative_suppression() {
        let (tmp, original, old_id, _) = legacy_notice_fixture(Some("thread-a"), true);
        let outbox = BridgeOutbox::shared(tmp.path()).unwrap();
        assert!(
            outbox
                .claim("tg", &old_id, chrono::Utc::now().timestamp())
                .unwrap()
        );
        outbox.mark_sent("tg", &old_id).unwrap();
        assert!(outbox.ack("tg", &old_id).unwrap());
        let connection =
            rusqlite::Connection::open(tmp.path().join("sessions").join("bridge_outbox.db"))
                .unwrap();
        connection.execute("DELETE FROM bridge_outbox", []).unwrap();
        for thread in [Some("thread-a"), Some("thread-b"), None] {
            assert_eq!(observe(&with_thread(&original, thread), &tmp).await, 0);
        }
        assert!(delegation_rows(&tmp).is_empty());
        // Evidence was lost only for this recipient/outcome, not all targets.
        let mut new_recipient = original;
        new_recipient
            .tachi
            .result_notification
            .as_mut()
            .unwrap()
            .recipient = "owner-new".into();
        assert_eq!(observe(&new_recipient, &tmp).await, 1);
    }

    #[tokio::test]
    async fn legacy_receipt_read_failure_preserves_prior_enqueue_markers() {
        let (tmp, config) = fixture_config();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        for request in ["first-request", "legacy-request"] {
            seed_binding(&backend, &config, "butler", request, request, None);
        }
        let bindings = backend.list_dispatch_bindings_page(0, 10).unwrap();
        backend
            .mark_binding_notified(bindings[1].rowid, "completed\u{1f}tg\u{1f}owner")
            .unwrap();
        let outbox = BridgeOutbox::shared(tmp.path()).unwrap();
        let old_id = outbox
            .enqueue_source(
                "tg",
                "owner",
                Some("thread-a"),
                "legacy notice",
                DELEGATION,
                "butler\u{1f}legacy-request",
                "completed",
            )
            .unwrap();
        let connection =
            rusqlite::Connection::open(tmp.path().join("sessions").join("bridge_outbox.db"))
                .unwrap();
        connection
            .execute(
                "UPDATE bridge_outbox SET thread_id = x'80' WHERE id = ?1",
                [&old_id],
            )
            .unwrap();
        let result = reconcile(
            &RwLock::new(config),
            tmp.path(),
            &tokio_util::sync::CancellationToken::new(),
            &mut Rotation::default(),
            &completed_fetch,
        )
        .await;
        assert!(
            result.is_err(),
            "a malformed receipt is not treated as missing"
        );
        assert_eq!(delegation_rows(&tmp).len(), 2);
        let first = backend
            .binding_notified_marker(bindings[0].rowid)
            .unwrap()
            .unwrap();
        assert!(already_notified(&first, "completed", "tg", "owner", None));
        assert_eq!(
            backend
                .binding_notified_marker(bindings[1].rowid)
                .unwrap()
                .as_deref(),
            Some("completed\u{1f}tg\u{1f}owner"),
            "failed recovery must not erase the legacy history"
        );
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
        // A malformed or manually stored composite owner cannot borrow an
        // enabled prefix's authority. Request ids contain no separator.
        assert!(!permits_delivery(
            &config,
            &outbox,
            &row("butler\u{1f}ghost\u{1f}req-1", "tg", "owner")
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

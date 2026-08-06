use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};

use super::capability::SopCapabilityRegistry;
use super::load_sops;
use super::metrics::SopMetricsCollector;
use super::route::{self, NextStep, RouteCtx};
use super::rundata::RunData;
use super::schema;
use super::store::{
    ClaimToken, InMemoryRunStore, PersistedRun, ProposalRecord, ProposalStatus, RetentionPolicy,
    SopEventRecord, SopRunStore, StoreError,
};
use super::time::cooldown_elapsed;
use super::types::{
    DeterministicRunState, DeterministicSavings, Sop, SopAdmission, SopAdmissionPolicy, SopEvent,
    SopExecutionMode, SopPriority, SopRun, SopRunAction, SopRunStatus, SopRunSummary, SopStep,
    SopStepKind, SopStepResult, SopStepStatus, SopTrigger, SopTriggerSource,
};
use crate::security::{ContentSafety, new_marker_id};

// Stable path for callers that historically imported timestamps from
// `sop::engine::now_iso8601`. Prefer `sop::time` / `sop::now_iso8601` for new
// code. Trigger matchers live in `sop::triggers` (used by `trigger_source`).
pub use super::time::now_iso8601;
use serde_json::Value;
use zeroclaw_config::schema::SopConfig;

mod admission;
mod claims;
mod persist;
pub use claims::err_is_resume_at_capacity;
pub(super) use persist::{ParkPersistOutcome, TerminalPersistenceRetained};
mod deterministic;

/// Central SOP orchestrator: loads SOPs, matches triggers, manages run lifecycle.
pub struct SopEngine {
    sops: Vec<Sop>,
    active_runs: HashMap<String, SopRun>,
    /// Completed/failed/cancelled runs (kept for status queries).
    finished_runs: Vec<SopRun>,
    config: SopConfig,
    run_counter: u64,
    /// Cumulative savings from deterministic execution.
    deterministic_savings: DeterministicSavings,
    /// Durable run-state store. Defaults to an ephemeral in-memory store
    /// (current behavior); `build_sop_engine` injects the configured backend.
    store: Arc<dyn SopRunStore>,
    /// Run-execution metrics collector. Per-engine fresh in `new()` (test
    /// isolation); `build_sop_engine` swaps in the process-shared collector.
    metrics: Arc<SopMetricsCollector>,
    /// Optional live run-change notifier. When present, every run mutation
    /// (admission, step advance, terminal finish) publishes the run's fresh
    /// summary so push surfaces (the Runs WebSocket) can forward it without
    /// polling. `None` in tests and any embedder that does not want a feed.
    run_notifier: Option<tokio::sync::broadcast::Sender<SopRunSummary>>,
    /// Deterministic capability registry for `kind = "capability"` SOP steps.
    capabilities: Arc<SopCapabilityRegistry>,
    /// Run IDs parked (`WaitingApproval`/`PausedCheckpoint`) whose exec claim was
    /// deliberately KEPT because the parked snapshot could not be durably
    /// persisted (`persist_parked_snapshot_then_release_claim`'s fail-closed
    /// branch). `retry_pending_park_persists` retries these each maintenance
    /// tick, which renews the kept claim's lease as a side effect even while the
    /// retry keeps failing, so the reaper's expired-claim sweep never reclaims a
    /// claim standing in for a park that still is not durable. Cleared (and the
    /// claim released) once a later retry persists successfully.
    claims_pending_persist: std::collections::HashSet<String>,
    /// Approval broker (EPIC G): membership + quorum authorization wrapping the
    /// `resolve_gate` chokepoint. Defaults to a pass-through (no policies) so
    /// behavior is unchanged until a `[sop.approval]` policy is configured.
    approval_broker: Arc<super::approval::ApprovalBroker>,
    /// A2: per-message dispatch idempotency for at-least-once transports. Maps a
    /// redelivery-stable `(sop_name, delivery key)` to the run that already started for
    /// it, so an AMQP broker redelivery of the same message (e.g. after a partial
    /// multi-SOP dispatch requeued the whole delivery) coalesces instead of starting a
    /// second run. Bounded FIFO (`DISPATCH_DEDUP_CAP`); the window need only outlast a
    /// broker redelivery, not persist forever, so it is in-memory like `finished_runs`.
    ///
    /// CONTRACT (best-effort): the delivery key derives from the AMQP `message-id`, so
    /// this is exactly-once ONLY when publishers set a UNIQUE `message-id` per logical
    /// message (the AMQP-recommended practice). That is the sole cross-redelivery-stable
    /// identity the broker exposes: `redelivered` is set for ANY requeue and the delivery
    /// tag changes across a redelivery, so neither can prove two deliveries are the same
    /// message. Under `message-id` REUSE (a publisher contract violation), a redelivery of
    /// a reused id can coalesce a genuinely distinct trigger into the wrong run and ACK it
    /// away: at-most-once, a dropped trigger. This is an accepted, documented limitation of
    /// keying on a publisher-controlled id; the safe direction elsewhere is always a
    /// duplicate run, never a silent drop, and a delivery with no `message-id` is never
    /// deduplicated. A requeue-free design (ACK every delivery, retry deferred SOPs
    /// in-process) would remove the redelivery and thus this dependency entirely - tracked
    /// as a follow-up, out of scope for the dedup window here.
    dispatch_dedup: std::collections::VecDeque<(String, String)>,
    /// Run IDs parked at a checkpoint whose denial tried to take the terminal
    /// path, but the terminal write failed after the run's exec claim was
    /// reacquired. The parked snapshot is already durable, so this set only
    /// renews the retained claim during maintenance; it must not release the
    /// claim until the operator retries to a durable outcome.
    claims_retained_after_terminal_rollback: std::collections::HashSet<String>,
}

/// Outcome of one [`SopEngine::run_maintenance_tick`] pass (EPIC A1), for
/// observability. All counts are 0 on a quiet tick.
#[derive(Debug, Default, Clone)]
pub struct MaintenanceSummary {
    /// Approval gates that hit their timeout this pass.
    pub timed_out: usize,
    /// Expired concurrency-claim leases reclaimed.
    pub reaped_claims: usize,
    /// Terminal runs pruned past the retention policy.
    pub pruned_runs: usize,
    /// Timeout actions produced. Mostly self-applied (`Escalate` re-stamps,
    /// `Cancel` finalizes); an opt-in `AutoApprove` yields a resumed `ExecuteStep`
    /// the caller logs until EPIC A2's live executor exists.
    pub timeout_actions: Vec<SopRunAction>,
}

impl MaintenanceSummary {
    /// True when the pass did nothing (no timeouts, reaps, or prunes).
    pub fn is_empty(&self) -> bool {
        self.timed_out == 0 && self.reaped_claims == 0 && self.pruned_runs == 0
    }
}

enum GateClearTransition {
    Active {
        // Boxed: `SopRunAction` is large; keeping it inline makes this the
        // dominant variant (clippy::large_enum_variant).
        action: Box<SopRunAction>,
        follow_up: Option<GateResolutionFollowUp>,
    },
    Terminal {
        status: SopRunStatus,
        reason: Option<String>,
        follow_up: Option<GateResolutionFollowUp>,
    },
}

enum GateResolutionFollowUp {
    StepSchemaReject {
        step: u32,
        phase: &'static str,
        reason: String,
    },
    StepSkipped {
        sop_name: String,
        step: u32,
        reason: String,
    },
}

/// A held execution-slot reservation from phase 1 of a start (`reserve_run_slot`),
/// awaiting phase 2 (`activate_reserved_run`) or release (`release_reservation`).
/// Carries the CAS claim that keeps the slot held so the AMQP multi-match batch path
/// can reserve every matched SOP before activating any of them.
pub(crate) struct StartReservation {
    run_id: String,
    claim: ClaimToken,
    sop: Sop,
    deterministic: bool,
}

impl StartReservation {
    /// The SOP this reservation holds a slot for.
    pub(crate) fn sop_name(&self) -> &str {
        &self.sop.name
    }
}

impl SopEngine {
    /// Create a new engine with the given config. Call `reload()` to load SOPs.
    pub fn new(config: SopConfig) -> Self {
        Self {
            sops: Vec::new(),
            active_runs: HashMap::new(),
            finished_runs: Vec::new(),
            config,
            run_counter: 0,
            deterministic_savings: DeterministicSavings::default(),
            store: Arc::new(InMemoryRunStore::new()),
            metrics: Arc::new(SopMetricsCollector::new()),
            run_notifier: None,
            capabilities: Arc::new(SopCapabilityRegistry::with_builtins()),
            claims_pending_persist: std::collections::HashSet::new(),
            approval_broker: Arc::new(super::approval::ApprovalBroker::disabled()),
            dispatch_dedup: std::collections::VecDeque::new(),
            claims_retained_after_terminal_rollback: std::collections::HashSet::new(),
        }
    }

    /// Inject a durable run-state store (used by `build_sop_engine`). Default is
    /// an ephemeral in-memory store, so callers that don't set one keep today's
    /// behavior exactly.
    pub fn with_store(mut self, store: Arc<dyn SopRunStore>) -> Self {
        self.store = store;
        self
    }

    /// Inject the metrics collector. `build_sop_engine` passes the process-shared
    /// collector so the engine's completion metrics and the SOP tools' reports
    /// observe one set; the default per-engine collector keeps tests isolated.
    pub fn with_metrics(mut self, metrics: Arc<SopMetricsCollector>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Attach a live run-change notifier. `build_sop_engine` wires the gateway's
    /// sender here so run transitions push to the Runs WebSocket. Returns the
    /// engine unchanged when never called (tests, headless embedders).
    pub fn with_run_notifier(mut self, tx: tokio::sync::broadcast::Sender<SopRunSummary>) -> Self {
        self.run_notifier = Some(tx);
        self
    }

    /// Subscribe to the live run-change feed if a notifier is attached. Each
    /// item is a fresh [`SopRunSummary`] for the run that just transitioned.
    pub fn subscribe_run_changes(&self) -> Option<tokio::sync::broadcast::Receiver<SopRunSummary>> {
        self.run_notifier.as_ref().map(|tx| tx.subscribe())
    }

    /// Publish a run's current summary on the notifier, if attached. A send
    /// error means no live subscribers; that is not a failure, so it is
    /// dropped. Marked `active` per the caller's chokepoint.
    fn notify_run(&self, run: &SopRun, active: bool) {
        if let Some(tx) = self.run_notifier.as_ref() {
            let _ = tx.send(SopRunSummary::from_run(run, active));
        }
    }

    /// Inject a deterministic capability registry. Tests and future daemon
    /// wiring can replace the built-ins without adding another execution path.
    pub fn with_capabilities(mut self, capabilities: Arc<SopCapabilityRegistry>) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Inject the approval broker (built from `[sop.approval]` config). Defaults to
    /// a pass-through; `build_sop_engine` replaces it with the configured broker.
    pub fn with_approval_broker(mut self, broker: Arc<super::approval::ApprovalBroker>) -> Self {
        self.approval_broker = broker;
        self
    }

    /// The approval broker (membership + quorum authorization). Callers that must
    /// deliver an escalation to a policy's second route read it here.
    pub fn approval_broker(&self) -> Arc<super::approval::ApprovalBroker> {
        Arc::clone(&self.approval_broker)
    }

    /// Resolve a gate or deterministic checkpoint THROUGH the broker (membership +
    /// quorum), then its single transition owner.
    /// This is the entry point out-of-band surfaces (gateway / CLI / tools) should
    /// call so a `[sop.approval]` policy is enforced; with no policy it is exactly
    /// `resolve_gate` for a `WaitingApproval` run or the historical checkpoint
    /// resolver for a `PausedCheckpoint` run. The broker is cloned out first so it
    /// does not borrow `self` while `self` is mutated by the chokepoint.
    pub fn resolve_via_broker(
        &mut self,
        run_id: &str,
        decision: super::approval::ApprovalDecision,
        principal: super::approval::ApprovalPrincipal,
    ) -> Result<super::approval::BrokerOutcome> {
        let broker = Arc::clone(&self.approval_broker);
        if let Some(step) = self.active_runs.get(run_id).and_then(|run| {
            (run.status == SopRunStatus::PausedCheckpoint).then_some(run.current_step)
        }) {
            if let Some(outcome) =
                broker.authorize_checkpoint(self, run_id, step, &decision, &principal)?
            {
                return Ok(outcome);
            }
            if let super::approval::ApprovalDecision::Revise { guidance } = &decision {
                self.revise_checkpoint_with_principal(
                    run_id,
                    guidance,
                    decision.clone(),
                    principal,
                )?;
                return Ok(super::approval::BrokerOutcome::Resolved(
                    super::approval::ResolveOutcome::Revised,
                ));
            }
            let action = self.decide_checkpoint_with_principal(run_id, decision, principal)?;
            return Ok(super::approval::BrokerOutcome::Resolved(
                super::approval::ResolveOutcome::Resumed(Box::new(action)),
            ));
        }
        broker.resolve(self, run_id, decision, principal)
    }

    /// Load/reload SOPs from the configured directory.
    pub fn reload(&mut self, workspace_dir: &Path) {
        self.sops = load_sops(
            workspace_dir,
            self.config.sops_dir.as_deref(),
            super::parse_execution_mode(&self.config.default_execution_mode),
        );
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            &format!("SOP engine loaded {} SOPs", self.sops.len())
        );
    }

    /// Return all loaded SOP definitions.
    pub fn sops(&self) -> &[Sop] {
        &self.sops
    }

    #[cfg(test)]
    pub(crate) fn replace_sops_for_test(&mut self, sops: Vec<Sop>) {
        self.sops = sops;
    }

    /// Return all active (in-flight) runs.
    pub fn active_runs(&self) -> &HashMap<String, SopRun> {
        &self.active_runs
    }

    /// Look up a run by ID (active or finished).
    pub fn get_run(&self, run_id: &str) -> Option<&SopRun> {
        self.active_runs
            .get(run_id)
            .or_else(|| self.finished_runs.iter().find(|r| r.run_id == run_id))
    }

    /// Look up an SOP by name.
    pub fn get_sop(&self, name: &str) -> Option<&Sop> {
        self.sops.iter().find(|s| s.name == name)
    }

    // ── Trigger matching ────────────────────────────────────────

    /// Match an incoming event against all loaded SOPs and return the names of
    /// SOPs whose triggers match.
    pub fn match_trigger(&self, event: &SopEvent) -> Vec<&Sop> {
        self.sops
            .iter()
            .filter(|sop| sop.triggers.iter().any(|t| trigger_matches(t, event)))
            .collect()
    }

    /// True when any loaded SOP has a trigger of this source. Fan-in
    /// callers use this as a cheap pre-filter before building and
    /// dispatching an event.
    pub fn wants_source(&self, source: SopTriggerSource) -> bool {
        self.sops
            .iter()
            .any(|sop| sop.triggers.iter().any(|t| t.source() == source))
    }

    // ── Run lifecycle ───────────────────────────────────────────

    fn rollback_failed_start(
        &mut self,
        run_id: &str,
        claim: &ClaimToken,
        err: anyhow::Error,
    ) -> anyhow::Error {
        if err.is::<TerminalPersistenceRetained>() {
            return err;
        }
        self.active_runs.remove(run_id);
        self.release_claim_best_effort(claim);
        err
    }

    /// Undo a SUCCESSFUL `activate_reserved_run` that must be reversed because a LATER
    /// sibling in the same all-or-nothing AMQP multi-match batch failed to activate.
    /// Activation runs no irreversible side effect (deterministic execution and the LLM
    /// agent loop both run LATER, in `record_started_run` / the driver), so the run is
    /// safe to reverse. Two cases:
    /// - A still-EXECUTING sibling (`holds_exec_claim` true) never durably persisted during
    ///   activation: drop it in-memory and release its exec claim.
    /// - A sibling that PARKED at a step-1 approval/checkpoint gate DID durably persist its
    ///   parked snapshot (and already released its claim). Dropping it only in-memory would
    ///   ORPHAN that durable row: after a restart, `restore_runs` would reconstruct it,
    ///   duplicating a run whose whole delivery was deferred + requeued. Durably supersede it
    ///   with a terminal `Cancelled` (a higher revision the store's guard accepts) so restore
    ///   skips it. Best-effort: a store failure here only leaves the bounded orphan back
    ///   (logged), never a double execution — the sibling never ran.
    pub(crate) fn rollback_activated_run(&mut self, run_id: &str) {
        let Some(mut run) = self.active_runs.remove(run_id) else {
            return;
        };
        if holds_exec_claim(run.status) {
            self.release_claim_best_effort(&Self::claim_handle_for_run(&run));
            return;
        }
        // Parked sibling: its durable snapshot must not survive the rollback.
        run.status = SopRunStatus::Cancelled;
        run.completed_at = Some(now_iso8601());
        if let Err(e) = self.persist_terminal(&run) {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "run_id": run.run_id.as_str(),
                        "error": e.to_string(),
                    })),
                "SOP dispatch: could not durably cancel a rolled-back parked AMQP sibling; a stale parked row may be reconstructed on restart"
            );
        }
    }

    pub fn start_run(&mut self, sop_name: &str, event: SopEvent) -> Result<SopRunAction> {
        // A start is a two-phase operation: reserve the exec slot through the
        // authoritative store CAS (no side effect yet), then activate the reserved
        // slot into a live run and dispatch its first step. The phases are split so the
        // AMQP multi-match path can reserve the WHOLE matched batch before activating
        // any of it (see `dispatch`). A single start runs both phases back-to-back.
        let reservation = self.reserve_run_slot(sop_name)?;
        self.activate_reserved_run(reservation, event)
    }

    /// Phase 1 of a start: reserve `sop_name`'s exec slot through the authoritative
    /// store CAS WITHOUT creating an active run or dispatching any step — so no SOP
    /// side effect occurs yet. The returned `StartReservation` holds a live claim; the
    /// caller MUST either `activate_reserved_run` it or `release_reservation` it, or
    /// the slot leaks. This is the primitive behind the AMQP multi-match all-or-defer-
    /// all reservation: every matched SOP's capacity is held atomically before ANY of
    /// them produces a side effect, so a sibling engine grabbing a slot mid-batch can
    /// never leave a partial start (it makes one reservation fail → release-all +
    /// defer-all), only a safe requeue.
    pub(crate) fn reserve_run_slot(&mut self, sop_name: &str) -> Result<StartReservation> {
        self.enforce_admission(sop_name)?;

        let sop = self
            .get_sop(sop_name)
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"sop_name": sop_name})),
                    "SOP engine: sop not found"
                );
                anyhow::Error::msg(format!("SOP not found: {sop_name}"))
            })?
            .clone();

        if !self.can_start(sop_name) {
            bail!(
                "Cannot start SOP '{}': cooldown or concurrency limit reached",
                sop_name
            );
        }

        if sop.steps.is_empty() {
            bail!("SOP '{}' has no steps defined", sop_name);
        }

        let deterministic = sop.execution_mode == SopExecutionMode::Deterministic;
        self.run_counter += 1;
        let dur = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let epoch_ns = dur.as_nanos();
        let prefix = if deterministic { "det" } else { "run" };
        let run_id = format!("{prefix}-{epoch_ns}-{:04}", self.run_counter);
        let claim = self.claim_admission(&run_id, &sop)?;
        Ok(StartReservation {
            run_id,
            claim,
            sop,
            deterministic,
        })
    }

    /// Release a reservation that will NOT be activated (a batch that could not fully
    /// reserve), freeing its exec slot for admission. Best-effort + logged, exactly
    /// like a park release: a swallowed failure only lets the reaper collect the claim
    /// later — no run was ever created, so there is no side effect to unwind.
    pub(crate) fn release_reservation(&self, reservation: StartReservation) {
        self.release_claim_best_effort(&reservation.claim);
    }

    /// Phase 2 of a start: convert a held reservation into a live run — build the run
    /// record, insert it, and dispatch its first step, rolling the reservation back
    /// (release the claim, drop the run) if that dispatch fails.
    pub(crate) fn activate_reserved_run(
        &mut self,
        reservation: StartReservation,
        event: SopEvent,
    ) -> Result<SopRunAction> {
        let StartReservation {
            run_id,
            claim,
            sop,
            deterministic,
        } = reservation;

        let run = SopRun {
            run_id: run_id.clone(),
            sop_name: sop.name.clone(),
            trigger_event: event,
            frame_marker_id: new_marker_id(),
            status: SopRunStatus::Running,
            current_step: 1,
            total_steps: u32::try_from(sop.steps.len()).unwrap_or(u32::MAX),
            started_at: now_iso8601(),
            completed_at: None,
            step_results: Vec::new(),
            waiting_since: None,
            llm_calls_saved: 0,
            revision: 0,
            revision_base: 0,
        };
        let first_input = step_input_value(&run, 1);
        self.active_runs.insert(run_id.clone(), run);

        if deterministic {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                &format!(
                    "Deterministic SOP run {} started for '{}'",
                    run_id, sop.name
                )
            );
            match self.dispatch_deterministic_step(&run_id, &sop, 1, first_input) {
                Ok(action) => Ok(action),
                Err(e) => Err(self.rollback_failed_start(&run_id, &claim, e)),
            }
        } else {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                &format!("SOP run {} started for '{}'", run_id, sop.name)
            );
            match self.dispatch_llm_step(&run_id, &sop, 1, None) {
                Ok(action) => Ok(action),
                Err(e) => Err(self.rollback_failed_start(&run_id, &claim, e)),
            }
        }
    }

    pub fn advance_step(&mut self, run_id: &str, result: SopStepResult) -> Result<SopRunAction> {
        let (sop_name, current_step_number) = {
            let run = self.active_runs.get(run_id).ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"run_id": run_id})),
                    "SOP engine: active run not found"
                );
                anyhow::Error::msg(format!("Active run not found: {run_id}"))
            })?;
            if matches!(
                run.status,
                SopRunStatus::WaitingApproval | SopRunStatus::PausedCheckpoint
            ) {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "run_id": run_id,
                            "status": run.status.to_string(),
                            "step": run.current_step,
                        })),
                    "SOP engine: advance_step rejected — run is paused at a gate"
                );
                bail!(
                    "Run {run_id} is paused at a {} gate; resolve the gate through \
                     `resolve_gate` (WaitingApproval) or `approve_step` (PausedCheckpoint) \
                     before advancing with sop_advance",
                    run.status
                );
            }
            (run.sop_name.clone(), run.current_step)
        };

        let sop = self
            .sops
            .iter()
            .find(|s| s.name == sop_name)
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"sop_name": sop_name})),
                    "SOP engine: sop no longer loaded (definition removed mid-run)"
                );
                anyhow::Error::msg(format!("SOP '{sop_name}' no longer loaded"))
            })?
            .clone();

        let current_step = sop
            .steps
            .get((current_step_number.saturating_sub(1)) as usize)
            .cloned()
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(
                            ::serde_json::json!({"sop_name": sop_name, "step": current_step_number})
                        ),
                    "SOP engine: step no longer exists (definition changed mid-run)"
                );
                anyhow::Error::msg(format!(
                    "SOP '{sop_name}' step {current_step_number} no longer exists (definition changed mid-run)"
                ))
            })?;

        if self
            .active_runs
            .get(run_id)
            .is_some_and(|run| run.status == SopRunStatus::Pending)
            && pending_step_blocks_direct_advance(&sop, &current_step)
        {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "run_id": run_id,
                        "step": current_step.number,
                        "step_kind": current_step.kind.to_string(),
                    })),
                "SOP engine: advance_step rejected - pending run is blocked at a human gate"
            );
            bail!(
                "Run {run_id} is pending at gated step {}; wait for pending approval/checkpoint \
                 capacity and resolve the gate before advancing with sop_advance",
                current_step.number
            );
        }

        // Deterministic runs are driven through the dedicated piping path so the
        // same `sop_advance` tool advances every execution mode.
        if sop.execution_mode == SopExecutionMode::Deterministic {
            if result.status == SopStepStatus::Failed {
                self.record_step_result(run_id, result.clone())?;
                return self.route_recorded_step(
                    run_id,
                    &sop,
                    &current_step,
                    SopStepStatus::Failed,
                    true,
                    Some(retry_input_value(
                        self.active_runs.get(run_id).ok_or_else(|| {
                            anyhow::Error::msg(format!("Active run not found: {run_id}"))
                        })?,
                        current_step.number,
                    )),
                    Some(step_result_value(&result)),
                );
            }
            let piped = step_result_value(&result);
            return self.advance_deterministic_step(
                run_id,
                piped,
                Some((result.started_at.clone(), result.completed_at.clone())),
            );
        }

        let mut recorded = result.clone();
        if result.status == SopStepStatus::Completed {
            let output = step_result_value(&result);
            if let Err(reason) = self.validate_step_output(&current_step, &output) {
                let full_reason = format!(
                    "Step {} output schema validation failed: {reason}",
                    current_step.number
                );
                self.record_transition_event(
                    run_id,
                    "step_schema_reject",
                    Some(full_reason.clone()),
                    ::serde_json::json!({
                        "step": current_step.number,
                        "phase": "output",
                    }),
                );
                recorded.status = SopStepStatus::Failed;
                recorded.output = full_reason;
            }
        }

        let retry_input = if recorded.status == SopStepStatus::Failed {
            Some(retry_input_value(
                self.active_runs
                    .get(run_id)
                    .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?,
                current_step.number,
            ))
        } else {
            None
        };

        self.record_step_result(run_id, recorded.clone())?;
        self.route_recorded_step(
            run_id,
            &sop,
            &current_step,
            recorded.status,
            false,
            retry_input,
            None,
        )
    }

    fn schema_input_failure_action(
        &mut self,
        run_id: &str,
        step: &SopStep,
        input: &Value,
    ) -> Result<Option<SopRunAction>> {
        self.schema_input_failure_reason(step, input)
            .map(|reason| self.fail_step_schema_validation(run_id, step.number, "input", reason))
            .transpose()
    }

    fn schema_input_failure_reason(&self, step: &SopStep, input: &Value) -> Option<String> {
        self.validate_step_input(step, input).err()
    }

    fn validate_step_input(&self, step: &SopStep, input: &Value) -> Result<(), String> {
        if !self.config.step_schema_enforce {
            return Ok(());
        }
        let Some(schema) = step
            .schema
            .as_ref()
            .and_then(|schema| schema.input.as_ref())
        else {
            return Ok(());
        };
        schema::validate_value(schema, input).map_err(|e| e.to_string())
    }

    fn validate_step_output(&self, step: &SopStep, output: &Value) -> Result<(), String> {
        if !self.config.step_schema_enforce {
            return Ok(());
        }
        let Some(schema) = step
            .schema
            .as_ref()
            .and_then(|schema| schema.output.as_ref())
        else {
            return Ok(());
        };
        schema::validate_value(schema, output).map_err(|e| e.to_string())
    }

    fn fail_step_schema_validation(
        &mut self,
        run_id: &str,
        step_number: u32,
        phase: &str,
        reason: String,
    ) -> Result<SopRunAction> {
        let reason = format!("Step {step_number} {phase} schema validation failed: {reason}");
        self.record_transition_event(
            run_id,
            "step_schema_reject",
            Some(reason.clone()),
            ::serde_json::json!({
                "step": step_number,
                "phase": phase,
            }),
        );
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({
                    "run_id": run_id,
                    "step": step_number,
                    "phase": phase,
                    "reason": reason,
                })),
            "SOP step schema validation failed"
        );
        self.finish_run(run_id, SopRunStatus::Failed, Some(reason))
    }

    fn gate_schema_failure_transition(
        &self,
        run_id: &str,
        step_number: u32,
        phase: &'static str,
        reason: String,
    ) -> Result<GateClearTransition> {
        self.active_runs
            .get(run_id)
            .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
        let reason = format!("Step {step_number} {phase} schema validation failed: {reason}");
        Ok(GateClearTransition::Terminal {
            status: SopRunStatus::Failed,
            reason: Some(reason.clone()),
            follow_up: Some(GateResolutionFollowUp::StepSchemaReject {
                step: step_number,
                phase,
                reason,
            }),
        })
    }

    fn record_step_result(&mut self, run_id: &str, result: SopStepResult) -> Result<()> {
        let run = self.active_runs.get_mut(run_id).ok_or_else(|| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"run_id": run_id})),
                "SOP engine: active run not found"
            );
            anyhow::Error::msg(format!("Active run not found: {run_id}"))
        })?;
        run.step_results.push(result);
        Ok(())
    }

    fn route_recorded_step(
        &mut self,
        run_id: &str,
        sop: &Sop,
        current_step: &SopStep,
        last_status: SopStepStatus,
        deterministic: bool,
        retry_input: Option<Value>,
        routed_input: Option<Value>,
    ) -> Result<SopRunAction> {
        let decision =
            self.route_decision_after_recorded_step(run_id, sop, current_step, last_status)?;
        self.apply_route_decision(
            run_id,
            sop,
            current_step.number,
            decision,
            deterministic,
            retry_input,
            routed_input,
        )
    }

    fn route_decision_after_recorded_step(
        &self,
        run_id: &str,
        sop: &Sop,
        current_step: &SopStep,
        last_status: SopStepStatus,
    ) -> Result<NextStep> {
        let run = self
            .active_runs
            .get(run_id)
            .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;

        if last_status == SopStepStatus::Failed {
            let failed_executions = run
                .step_results
                .iter()
                .filter(|result| {
                    result.step_number == current_step.number
                        && result.status == SopStepStatus::Failed
                })
                .count()
                .try_into()
                .unwrap_or(u32::MAX);
            let retries_consumed = failed_executions.saturating_sub(1);
            let decision = route::failure::route_failure(
                &current_step.on_failure,
                retries_consumed,
                self.config.max_step_retries,
            );
            return Ok(match decision {
                NextStep::Fail(reason) if reason == "step failed" => {
                    let detail = run
                        .step_results
                        .iter()
                        .rev()
                        .find(|result| {
                            result.step_number == current_step.number
                                && result.status == SopStepStatus::Failed
                        })
                        .map(|result| result.output.as_str())
                        .unwrap_or("step failed");
                    NextStep::Fail(format!("Step {} failed: {detail}", current_step.number))
                }
                other => other,
            });
        }

        let run_data = RunData::from_step_results(&run.step_results);
        Ok(route::resolve_next(&RouteCtx {
            sop,
            run,
            run_data: &run_data,
            last_status,
            max_step_visits: self.config.max_step_visits,
        }))
    }

    fn apply_route_decision(
        &mut self,
        run_id: &str,
        sop: &Sop,
        current_step_number: u32,
        decision: NextStep,
        deterministic: bool,
        retry_input: Option<Value>,
        routed_input: Option<Value>,
    ) -> Result<SopRunAction> {
        match decision {
            NextStep::Step(step_number) => {
                if let Some(action) = self.visit_bound_failure(run_id, step_number)? {
                    return Ok(action);
                }
                self.record_transition_event(
                    run_id,
                    "step_promoted",
                    None,
                    ::serde_json::json!({
                        "from_step": current_step_number,
                        "to_step": step_number,
                    }),
                );
                if deterministic {
                    let input = routed_input.unwrap_or_default();
                    self.dispatch_deterministic_step(run_id, sop, step_number, input)
                } else {
                    self.dispatch_llm_step(run_id, sop, step_number, None)
                }
            }
            NextStep::Retry => {
                if let Some(action) = self.visit_bound_failure(run_id, current_step_number)? {
                    return Ok(action);
                }
                self.record_transition_event(
                    run_id,
                    "step_retry",
                    None,
                    ::serde_json::json!({
                        "step": current_step_number,
                    }),
                );
                if deterministic {
                    self.dispatch_deterministic_step(
                        run_id,
                        sop,
                        current_step_number,
                        retry_input.unwrap_or_default(),
                    )
                } else {
                    self.dispatch_llm_step(run_id, sop, current_step_number, retry_input)
                }
            }
            NextStep::Complete => {
                if deterministic {
                    self.finish_deterministic_run(run_id)
                } else {
                    ::zeroclaw_log::record!(
                        INFO,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_attrs(::serde_json::json!({"run_id": run_id})),
                        "SOP run completed successfully"
                    );
                    self.finish_run(run_id, SopRunStatus::Completed, None)
                }
            }
            NextStep::Fail(reason) => self.finish_run(run_id, SopRunStatus::Failed, Some(reason)),
            NextStep::Wait(step_number) => Ok(self.mark_step_pending(
                run_id,
                sop,
                step_number,
                format!("step {step_number} dependencies not satisfied"),
            )),
        }
    }

    fn visit_bound_failure(
        &mut self,
        run_id: &str,
        step_number: u32,
    ) -> Result<Option<SopRunAction>> {
        let run = self
            .active_runs
            .get(run_id)
            .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
        if route::guard::within_visit_bound(run, step_number, self.config.max_step_visits) {
            return Ok(None);
        }

        Ok(Some(self.finish_run(
            run_id,
            SopRunStatus::Failed,
            Some(format!("step {step_number} visit limit reached")),
        )?))
    }

    fn dispatch_llm_step(
        &mut self,
        run_id: &str,
        sop: &Sop,
        step_number: u32,
        input_override: Option<Value>,
    ) -> Result<SopRunAction> {
        let step = self.resolve_sop_step(sop, step_number)?;
        if let Some(action) = self.visit_bound_failure(run_id, step_number)? {
            return Ok(action);
        }

        if let Some(run) = self.active_runs.get_mut(run_id) {
            run.current_step = step_number;
            run.status = SopRunStatus::Running;
            run.waiting_since = None;
        }

        let run_data = {
            let run = self
                .active_runs
                .get(run_id)
                .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
            RunData::from_step_results(&run.step_results)
        };
        if !route::eligible(&step, &run_data) {
            return Ok(self.mark_step_pending(
                run_id,
                sop,
                step.number,
                format!("step {} dependencies not satisfied", step.number),
            ));
        }

        let input = match input_override {
            Some(input) => input,
            None => {
                let run = self
                    .active_runs
                    .get(run_id)
                    .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
                step_input_value(run, step.number)
            }
        };
        if let Some(action) = self.schema_input_failure_action(run_id, &step, &input)? {
            return Ok(action);
        }

        let context = {
            let run = self
                .active_runs
                .get(run_id)
                .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
            format_step_context(sop, run, &step, &self.config)
        };
        // Upstream's resolve_step_action now forces approval whenever the
        // SOP-level mode needs it (strictly stronger than the old
        // approval_mode-conditional escalation), so the mode param is gone.
        let action = resolve_step_action(sop, &step, run_id.to_string(), context);
        let parked_for_approval = matches!(action, SopRunAction::WaitApproval { .. });
        let has_prior_gate_presentation = parked_for_approval
            && self.run_events(run_id).is_ok_and(|events| {
                events.iter().any(|event| {
                    matches!(
                        event.kind.as_str(),
                        "gate_vote" | "gate_resolved" | "gate_escalated" | "gate_timed_out"
                    )
                })
            });

        // A1: free the exec slot while the run waits on a human - but only AFTER
        // the parked snapshot is durably persisted (else keep the claim, fail
        // closed).
        if parked_for_approval {
            if let Some(reason) = self.pending_pool_full_reason(sop) {
                Self::log_pending_capacity_full(run_id, &reason);
                return Ok(self.mark_step_pending(run_id, sop, step.number, reason));
            }
            if let Some(run) = self.active_runs.get_mut(run_id) {
                run.status = SopRunStatus::WaitingApproval;
                run.waiting_since = Some(now_iso8601());
                if run.revision > 0 || has_prior_gate_presentation {
                    run.revision += 1;
                }
            }
            match self.persist_parked_snapshot_then_release_claim(run_id) {
                // Deliver only after the parked snapshot is durable. A failed persist
                // keeps the claim and the maintenance retry issues the notice later.
                ParkPersistOutcome::Released => self.notify_park_request(run_id),
                ParkPersistOutcome::CapacityFull => {
                    let reason = self.pending_pool_capacity_raced_reason(sop);
                    Self::log_pending_capacity_full(run_id, &reason);
                    return Ok(self.mark_step_pending(run_id, sop, step.number, reason));
                }
                ParkPersistOutcome::PersistFailed => {
                    let reason =
                        format!("SOP '{}' park snapshot not yet durably persisted", sop.name);
                    return Ok(SopRunAction::Pending {
                        run_id: run_id.to_string(),
                        sop_name: sop.name.clone(),
                        step: step.number,
                        reason,
                    });
                }
            }
        } else {
            self.persist_active(run_id);
        }
        Ok(action)
    }

    /// Deliver the initial approval-request notice for a run that just parked at a
    /// policied gate, if that policy names a `request_route`. Best-effort: a run
    /// with no policy, a policy with no request route, or a delivery error all leave
    /// the (already-parked, already-durable) gate untouched.
    fn notify_park_request(&self, run_id: &str) {
        let Some(run) = self.get_run(run_id) else {
            return;
        };
        let (sop_name, step, revision) = (run.sop_name.clone(), run.current_step, run.revision);
        // Edit/Revise resolve ONLY through the deterministic-checkpoint path
        // (`resolve_checkpoint`); a broker-owned approval gate refuses them
        // fail-closed. Offering the choices on a non-checkpoint park would
        // render buttons whose submissions are always rejected — the operator's
        // typed text silently lost behind a success-looking ack.
        let is_checkpoint = run.status == SopRunStatus::PausedCheckpoint;
        // The notice carries WHAT is being approved: the parked step's piped
        // input (trigger payload at step 1, previous step's output later) plus
        // the step's authored `- prompt:` template when it has one.
        let context = step_input_value(run, step);
        let step_def = self
            .resolve_active_run_sop(run_id)
            .ok()
            .and_then(|(_, sop)| self.resolve_sop_step(&sop, step).ok());
        let gate_prompt = step_def.as_ref().and_then(|s| s.gate_prompt.clone());
        // Input-bearing choices: Edit needs the step's `- edit:` declaration;
        // Revise needs an llm.generate predecessor and headroom under the cap.
        let edit_field = step_def
            .as_ref()
            .filter(|_| is_checkpoint)
            .and_then(|s| s.edit.as_deref())
            .map(str::trim)
            .filter(|f| !f.is_empty())
            .map(str::to_string);
        let can_revise = is_checkpoint
            && revision.saturating_sub(run.revision_base) < MAX_GATE_REVISIONS
            && self.revisable_predecessor(run_id).is_some();
        let Some(policy_name) = self.current_step_policy_name(run_id) else {
            return;
        };
        let broker = self.approval_broker();
        if let Some(route) = broker.request_route(self.approval_config(), &policy_name) {
            broker.deliver_request(
                &route,
                &super::approval::GateNotice {
                    run_id,
                    sop_name: &sop_name,
                    step,
                    context: &context,
                    gate_prompt: gate_prompt.as_deref(),
                    revision,
                    edit_field: edit_field.as_deref(),
                    can_revise,
                },
            );
        }
    }

    fn resolve_sop_step(&self, sop: &Sop, step_number: u32) -> Result<SopStep> {
        sop.steps
            .iter()
            .find(|step| step.number == step_number)
            .cloned()
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(
                            ::serde_json::json!({"sop_name": sop.name, "step": step_number})
                        ),
                    "SOP engine: step no longer exists (definition changed mid-run)"
                );
                anyhow::Error::msg(format!(
                    "SOP '{}' step {step_number} no longer exists (definition changed mid-run)",
                    sop.name
                ))
            })
    }

    fn mark_step_pending(
        &mut self,
        run_id: &str,
        sop: &Sop,
        step_number: u32,
        reason: String,
    ) -> SopRunAction {
        self.mark_step_pending_with_persist(run_id, sop, step_number, reason, true)
    }

    fn mark_step_pending_with_persist(
        &mut self,
        run_id: &str,
        sop: &Sop,
        step_number: u32,
        reason: String,
        persist: bool,
    ) -> SopRunAction {
        let now = now_iso8601();
        if let Some(run) = self.active_runs.get_mut(run_id) {
            run.current_step = step_number;
            run.status = SopRunStatus::Pending;
            run.waiting_since = Some(now.clone());
            let last_is_same_skip = run.step_results.last().is_some_and(|result| {
                result.step_number == step_number && result.status == SopStepStatus::Skipped
            });
            if !last_is_same_skip {
                run.step_results.push(SopStepResult {
                    step_number,
                    status: SopStepStatus::Skipped,
                    output: reason.clone(),
                    started_at: now.clone(),
                    completed_at: Some(now.clone()),
                    effective_agent: None,
                    tool_calls: Vec::new(),
                });
            }
        }
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({
                    "run_id": run_id,
                    "sop_name": sop.name,
                    "step": step_number,
                    "reason": reason,
                })),
            "SOP run pending on step dependencies"
        );
        self.record_transition_event(
            run_id,
            "step_skipped",
            Some(reason.clone()),
            ::serde_json::json!({
                "step": step_number,
                "status": "pending",
            }),
        );
        if persist {
            self.persist_active(run_id);
        }
        SopRunAction::Pending {
            run_id: run_id.to_string(),
            sop_name: sop.name.clone(),
            step: step_number,
            reason,
        }
    }

    fn gate_step_pending_transition(
        &mut self,
        run_id: &str,
        sop: &Sop,
        step_number: u32,
        reason: String,
    ) -> Result<GateClearTransition> {
        let now = now_iso8601();
        let run = self
            .active_runs
            .get_mut(run_id)
            .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
        run.current_step = step_number;
        run.status = SopRunStatus::Pending;
        run.waiting_since = Some(now.clone());
        let last_is_same_skip = run.step_results.last().is_some_and(|result| {
            result.step_number == step_number && result.status == SopStepStatus::Skipped
        });
        if !last_is_same_skip {
            run.step_results.push(SopStepResult {
                step_number,
                status: SopStepStatus::Skipped,
                output: reason.clone(),
                started_at: now.clone(),
                completed_at: Some(now),
                effective_agent: None,
                tool_calls: Vec::new(),
            });
        }

        Ok(GateClearTransition::Active {
            action: Box::new(SopRunAction::Pending {
                run_id: run_id.to_string(),
                sop_name: sop.name.clone(),
                step: step_number,
                reason: reason.clone(),
            }),
            follow_up: Some(GateResolutionFollowUp::StepSkipped {
                sop_name: sop.name.clone(),
                step: step_number,
                reason,
            }),
        })
    }

    fn record_gate_resolution_follow_up(&self, run_id: &str, follow_up: GateResolutionFollowUp) {
        match follow_up {
            GateResolutionFollowUp::StepSchemaReject {
                step,
                phase,
                reason,
            } => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "run_id": run_id,
                            "step": step,
                            "phase": phase,
                            "reason": reason.as_str(),
                        })),
                    "SOP step schema validation failed"
                );
                self.record_transition_event(
                    run_id,
                    "step_schema_reject",
                    Some(reason),
                    ::serde_json::json!({
                        "step": step,
                        "phase": phase,
                    }),
                );
            }
            GateResolutionFollowUp::StepSkipped {
                sop_name,
                step,
                reason,
            } => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "run_id": run_id,
                            "sop_name": sop_name,
                            "step": step,
                            "reason": reason.as_str(),
                        })),
                    "SOP run pending on step dependencies"
                );
                self.record_transition_event(
                    run_id,
                    "step_skipped",
                    Some(reason),
                    ::serde_json::json!({
                        "step": step,
                        "status": "pending",
                    }),
                );
            }
        }
    }

    /// Cancel an active run.
    pub fn cancel_run(&mut self, run_id: &str) -> Result<()> {
        if !self.active_runs.contains_key(run_id) {
            bail!("Active run not found: {run_id}");
        }
        self.finish_run(run_id, SopRunStatus::Cancelled, None)?;
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"run_id": run_id})),
            "SOP run  cancelled"
        );
        Ok(())
    }

    pub fn approve_step(&mut self, run_id: &str) -> Result<SopRunAction> {
        self.resume_checkpoint(run_id, None)
    }

    /// Resume a run paused at a deterministic checkpoint, optionally amending one
    /// field of the piped value first (`amend = (field, text)`, the operator-edited
    /// draft). The amended value becomes the checkpoint's recorded output, so the
    /// human-approved text flows downstream while the predecessor step keeps the
    /// model's original.
    fn resume_checkpoint(
        &mut self,
        run_id: &str,
        amend: Option<(String, String)>,
    ) -> Result<SopRunAction> {
        self.resume_checkpoint_inner(run_id, amend, false)
    }

    fn resume_checkpoint_inner(
        &mut self,
        run_id: &str,
        amend: Option<(String, String)>,
        claim_already_reacquired: bool,
    ) -> Result<SopRunAction> {
        let status = self
            .active_runs
            .get(run_id)
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"run_id": run_id})),
                    "SOP engine: active run not found"
                );
                anyhow::Error::msg(format!("Active run not found: {run_id}"))
            })?
            .status;

        if status != SopRunStatus::PausedCheckpoint {
            bail!("Run {run_id} is not paused at a checkpoint (status: {status})");
        }

        // Refuse to resume while the checkpoint's parked snapshot has not yet
        // been durably persisted (see `is_park_persist_pending`'s doc): the kept
        // claim predates this attempt, and reacquiring on top of it would give a
        // later rollback or a maintenance retry no way to distinguish "freshly
        // reacquired" from "pre-existing, must survive."
        if self.is_park_persist_pending(run_id) {
            bail!(
                "Run {run_id} cannot resume: its parked checkpoint snapshot is not yet durably persisted (retrying)"
            );
        }

        // Pre-flight the same SOP/step lookups `advance_deterministic_step` performs
        // BEFORE reacquiring the claim or mutating the run: a definition removed or
        // shrunk while parked must fail closed with the run left at
        // `PausedCheckpoint` (re-resolvable), not stranded in `Running` holding a
        // claim it can never advance.
        self.can_advance_deterministic_step(run_id)?;

        // A1: fail-closed - re-acquire the exec claim released when this run parked
        // BEFORE flipping it to Running; if it cannot, abort and leave the run paused
        // (re-resolvable) rather than execute uncounted.
        if !claim_already_reacquired {
            self.reacquire_claim_on_resume(run_id)?;
        }
        // A deterministic run paused at a checkpoint resumes through the
        // deterministic piping path: the checkpoint step is recorded as
        // completed and its input (the previous step's output — or, for a
        // checkpoint parked at step 1, the trigger payload) is piped forward.
        // Same step-1 mapping as `step_input_value`; `.last()` alone starved an
        // intake-gate pipeline (checkpoint BEFORE the first work step) of its
        // trigger payload.
        let run = self
            .active_runs
            .get_mut(run_id)
            .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
        let mut piped = step_input_value(run, run.current_step);
        // Operator amendment: replace the declared editable field BEFORE any run
        // mutation, so a non-amendable input (pre-flighted by
        // `can_amend_checkpoint`, so defensive here) leaves the run parked.
        if let Some((field, text)) = amend {
            match piped.as_object_mut() {
                Some(map) => {
                    map.insert(field, serde_json::Value::String(text));
                }
                None => {
                    self.release_claim_on_park(run_id);
                    bail!(
                        "Run {run_id} checkpoint input is not a JSON object; \
                         cannot amend field '{field}'"
                    );
                }
            }
        }
        let run = self
            .active_runs
            .get_mut(run_id)
            .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
        let prior_waiting_since = run.waiting_since.clone();
        run.status = SopRunStatus::Running;
        run.waiting_since = None;
        match self.advance_deterministic_step(run_id, piped, None) {
            Ok(action) => Ok(action),
            Err(e) => {
                // Defensive: the pre-flight above validated the same lookups under
                // this lock, so this is unreachable in practice. If the advance
                // still fails, roll the run back to `PausedCheckpoint` and release
                // the just-reacquired claim so a run that made no progress does not
                // get stuck in `Running` holding a leaked exec slot.
                if let Some(run) = self.active_runs.get_mut(run_id) {
                    run.status = SopRunStatus::PausedCheckpoint;
                    run.waiting_since = prior_waiting_since;
                }
                self.release_claim_on_park(run_id);
                Err(e)
            }
        }
    }

    /// The `- edit:` field the run's current checkpoint step declares, or why an
    /// amend cannot apply. Resolved under the engine lock at resolution time, so
    /// the field an operator edits is always the step's live declaration.
    fn checkpoint_edit_field(&self, run_id: &str) -> Result<String> {
        let (_, sop) = self.resolve_active_run_sop(run_id)?;
        let current_step = self
            .active_runs
            .get(run_id)
            .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?
            .current_step;
        let step = self.resolve_sop_step(&sop, current_step)?;
        step.edit
            .as_deref()
            .map(str::trim)
            .filter(|f| !f.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                anyhow::Error::msg(format!(
                    "SOP '{}' step {current_step} does not declare an editable field \
                     (`- edit:`); an amend cannot apply",
                    sop.name
                ))
            })
    }

    /// Pre-flight an `Amend` WITHOUT mutating anything: the step must declare an
    /// editable field and the checkpoint's piped value must be a JSON object the
    /// field can replace into.
    fn can_amend_checkpoint(&self, run_id: &str) -> Result<()> {
        self.checkpoint_edit_field(run_id)?;
        let run = self
            .active_runs
            .get(run_id)
            .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
        if !step_input_value(run, run.current_step).is_object() {
            bail!(
                "Run {run_id} checkpoint input is not a JSON object; \
                 there is no field an amend could replace"
            );
        }
        Ok(())
    }

    /// The step a `Revise` would re-run: the last COMPLETED step before the
    /// checkpoint, but only when it is an `llm.generate` capability (the only
    /// step kind a re-draft is meaningful for). `None` = this gate is not
    /// revisable.
    fn revisable_predecessor(&self, run_id: &str) -> Option<u32> {
        let run = self.get_run(run_id)?;
        let pred = run
            .step_results
            .iter()
            .rev()
            .find(|r| r.status == SopStepStatus::Completed && r.step_number < run.current_step)?
            .step_number;
        let (_, sop) = self.resolve_active_run_sop(run_id).ok()?;
        let step = self.resolve_sop_step(&sop, pred).ok()?;
        (step.kind == SopStepKind::Capability && step.capability_id() == Some("llm.generate"))
            .then_some(pred)
    }

    /// Pre-flight a `Revise` WITHOUT mutating anything: the revision cap has not
    /// been reached and the gate has an `llm.generate` predecessor to re-run.
    fn can_revise_checkpoint(&self, run_id: &str) -> Result<()> {
        let run = self
            .get_run(run_id)
            .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
        // Per-GATE budget: presentations spent at THIS gate, not run-wide
        // (`revision` also advances when a later gate first parks).
        if run.revision.saturating_sub(run.revision_base) >= MAX_GATE_REVISIONS {
            bail!(
                "Run {run_id} has reached this gate's revision limit ({MAX_GATE_REVISIONS}); \
                 approve, edit, or deny the current draft"
            );
        }
        if self.revisable_predecessor(run_id).is_none() {
            bail!(
                "Run {run_id} has no llm.generate predecessor step to re-run; \
                 this gate is not revisable"
            );
        }
        Ok(())
    }

    /// Re-run the checkpoint's predecessor `llm.generate` step with the operator's
    /// guidance framed as reviewer feedback, replace the recorded draft, bump the
    /// gate revision, and re-present the gate. The run never leaves
    /// `PausedCheckpoint`: a failed re-draft keeps the OLD draft parked and
    /// answerable. The caller commits the new snapshot and ledger event together.
    /// The model call blocks under the engine lock — the same tradeoff as a normal
    /// `llm.generate` step.
    fn revise_checkpoint_draft(&mut self, run_id: &str, guidance: &str) -> Result<()> {
        let (_, sop) = self.resolve_active_run_sop(run_id)?;
        let pred_number = self.revisable_predecessor(run_id).ok_or_else(|| {
            anyhow::Error::msg(format!(
                "Run {run_id} has no llm.generate predecessor step to re-run"
            ))
        })?;
        let pred_step = self.resolve_sop_step(&sop, pred_number)?;
        let piped = {
            let run = self
                .get_run(run_id)
                .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
            replay_input_for_step(run, pred_number)
        };

        // The guidance rides in the step's STATIC config plane (alongside the
        // authored instruction), NOT the untrusted payload frame: it comes from
        // an authenticated approver, and it must be able to steer the redraft.
        let mut step = pred_step.clone();
        let mut configured = step
            .capability_input
            .take()
            .unwrap_or_else(|| serde_json::json!({}));
        if let Some(object) = configured.as_object_mut() {
            object.insert(
                "revision_feedback".to_string(),
                serde_json::Value::String(guidance.to_string()),
            );
        }
        step.capability_input = Some(configured);

        // The re-draft is real work: hold an exec slot for its duration (the run
        // released its slot when it parked).
        self.reacquire_claim_on_resume(run_id)?;
        let ctx = super::capability::CapabilityContext {
            run_id: run_id.to_string(),
            sop_name: sop.name.clone(),
            step_number: pred_number,
            sop_location: sop.location.clone(),
        };
        let result = self.capabilities.execute_step(ctx, &step, piped);
        self.metrics.record_capability_executed(&sop.name);

        let output = match result {
            Ok(r) if r.success => match self.validate_step_output(&pred_step, &r.output) {
                Ok(()) => r.output,
                Err(reason) => {
                    self.release_claim_on_park(run_id);
                    bail!(
                        "Run {run_id} revised draft failed step {pred_number}'s output \
                         schema (previous draft kept): {reason}"
                    );
                }
            },
            Ok(r) => {
                self.release_claim_on_park(run_id);
                bail!(
                    "Run {run_id} re-draft failed (previous draft kept): {}",
                    r.error
                        .unwrap_or_else(|| "capability returned failure".to_string())
                );
            }
            Err(e) => {
                self.release_claim_on_park(run_id);
                bail!("Run {run_id} re-draft failed (previous draft kept): {e}");
            }
        };

        {
            let run = self
                .active_runs
                .get_mut(run_id)
                .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
            if let Some(recorded) = run
                .step_results
                .iter_mut()
                .rev()
                .find(|r| r.step_number == pred_number && r.status == SopStepStatus::Completed)
            {
                recorded.output = output.to_string();
                recorded.completed_at = Some(now_iso8601());
            }
            run.revision += 1;
            run.waiting_since = Some(now_iso8601());
        }

        Ok(())
    }

    /// Apply a revise decision while preserving the current store contract: the
    /// new parked draft and its gate-resolution event commit together, or the
    /// in-memory run rolls back to the previous answerable draft.
    fn revise_checkpoint_with_principal(
        &mut self,
        run_id: &str,
        guidance: &str,
        decision: super::approval::ApprovalDecision,
        principal: super::approval::ApprovalPrincipal,
    ) -> Result<()> {
        let prior_run = self
            .active_runs
            .get(run_id)
            .cloned()
            .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
        if prior_run.status != SopRunStatus::PausedCheckpoint {
            bail!(
                "Run {run_id} is not paused at a checkpoint (status: {})",
                prior_run.status
            );
        }
        if self.is_park_persist_pending(run_id) {
            bail!(
                "Run {run_id} cannot re-draft: its parked checkpoint snapshot is not yet \
                 durably persisted (retrying)"
            );
        }
        self.can_revise_checkpoint(run_id)?;
        let (_, sop) = self.resolve_active_run_sop(run_id)?;
        self.revise_checkpoint_draft(run_id, guidance)?;

        let event = super::approval::GateLedgerEntry {
            run_id: run_id.to_string(),
            step: prior_run.current_step,
            gate_revision: Some(prior_run.revision),
            checkpoint_revision: Some(prior_run.revision),
            decision_identity: super::approval::broker::checkpoint_decision_identity(&decision)
                .map(|(_, identity)| identity),
            kind: super::approval::GateEventKind::Resolved,
            decision: Some(decision),
            principal,
            ts: now_iso8601(),
        }
        .into_event_record();
        if let Err(e) = self.persist_active_with_gate_event(run_id, &event) {
            self.active_runs.insert(run_id.to_string(), prior_run);
            self.release_claim_on_park(run_id);
            return Err(e);
        }

        // The run store is authoritative. Refresh the rehydration artifact after
        // the atomic store write, then release the temporary execution claim and
        // present the versioned replacement prompt.
        if let Err(e) = self.persist_deterministic_state(run_id, &sop, true) {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"run_id": run_id, "error": e.to_string()})),
                "SOP engine: revised state-file refresh failed (run store remains authoritative)"
            );
        }
        self.release_claim_on_park(run_id);
        self.notify_park_request(run_id);
        Ok(())
    }

    /// Pre-flight ONLY the fallible lookups that `clear_waiting_gate` performs
    /// (the SOP is still loaded and the waiting step still resolves by number),
    /// WITHOUT reacquiring a claim, mutating the run, or persisting anything.
    ///
    /// `resolve_gate` calls this BEFORE it reacquires the exec claim and appends
    /// the immutable `gate_resolved` ledger row, so a run whose SOP was removed or
    /// shrunk while it sat parked fails closed here - with no claim reacquired and
    /// no false "resolved" audit row - instead of after the ledger append, which
    /// would otherwise leave a durable `gate_resolved` row for a still-waiting gate
    /// AND leak the reacquired exec slot. Runs under the engine mutex, so the
    /// lookups it validates cannot change before `clear_waiting_gate` re-runs them.
    pub(crate) fn can_clear_waiting_gate(&self, run_id: &str) -> Result<()> {
        let (sop_name, current_step) = {
            let run = self
                .active_runs
                .get(run_id)
                .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
            (run.sop_name.clone(), run.current_step)
        };
        let sop = self
            .sops
            .iter()
            .find(|s| s.name == sop_name)
            .ok_or_else(|| anyhow::Error::msg(format!("SOP '{sop_name}' no longer loaded")))?;
        self.resolve_sop_step(sop, current_step)?;
        Ok(())
    }

    /// Resolve a checkpoint decision (`PausedCheckpoint`). `Approve` resumes the
    /// success path (records the checkpoint `Completed`, pipes forward down
    /// `routing.next`); `Deny` takes the failure path (records the checkpoint
    /// `Failed` and routes through the step's `on_failure`, exactly like a step
    /// that failed execution). This is the single entry point for both outcomes;
    /// callers never branch on status. `approve_step` is the `Approve`-only alias.
    pub fn decide_checkpoint(
        &mut self,
        run_id: &str,
        decision: super::approval::ApprovalDecision,
    ) -> Result<SopRunAction> {
        match decision {
            super::approval::ApprovalDecision::Approve => self.approve_step(run_id),
            super::approval::ApprovalDecision::Deny { reason } => {
                self.deny_checkpoint(run_id, reason)
            }
            super::approval::ApprovalDecision::Amend { .. }
            | super::approval::ApprovalDecision::Revise { .. } => {
                bail!(
                    "checkpoint edit and revise decisions must resolve through the approval broker"
                )
            }
        }
    }

    /// Apply a broker-authorized checkpoint decision and persist the resulting run
    /// state together with the approver audit row. The run store is the durable
    /// source of truth for both surfaces, so a failed combined write leaves the
    /// checkpoint parked with no false resolution event.
    fn decide_checkpoint_with_principal(
        &mut self,
        run_id: &str,
        decision: super::approval::ApprovalDecision,
        principal: super::approval::ApprovalPrincipal,
    ) -> Result<SopRunAction> {
        let prior_run = self
            .active_runs
            .get(run_id)
            .cloned()
            .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
        if prior_run.status != SopRunStatus::PausedCheckpoint {
            bail!(
                "Run {run_id} is not paused at a checkpoint (status: {})",
                prior_run.status
            );
        }
        if self.is_park_persist_pending(run_id) {
            bail!(
                "Run {run_id} cannot resolve: its parked checkpoint snapshot is not yet durably persisted (retrying)"
            );
        }

        if matches!(decision, super::approval::ApprovalDecision::Revise { .. }) {
            bail!("checkpoint revise decisions use the revision persistence path")
        }
        if matches!(decision, super::approval::ApprovalDecision::Amend { .. }) {
            self.can_amend_checkpoint(run_id)?;
        }

        let (_, sop) = self.resolve_active_run_sop(run_id)?;
        let current_step = self.resolve_sop_step(&sop, prior_run.current_step)?;
        let mut piped = step_input_value(&prior_run, current_step.number);
        if let super::approval::ApprovalDecision::Amend { text } = &decision {
            let field = self.checkpoint_edit_field(run_id)?;
            let Some(object) = piped.as_object_mut() else {
                bail!(
                    "Run {run_id} checkpoint input is not a JSON object; cannot amend field '{field}'"
                );
            };
            object.insert(field, serde_json::Value::String(text.clone()));
        }
        let (status, recorded_output, routed_output, started_at, completed_at) = match &decision {
            super::approval::ApprovalDecision::Approve
            | super::approval::ApprovalDecision::Amend { .. } => (
                SopStepStatus::Completed,
                piped.to_string(),
                piped,
                prior_run.started_at.clone(),
                Some(now_iso8601()),
            ),
            super::approval::ApprovalDecision::Deny { reason } => {
                if let super::step_contract::StepFailure::Goto { step } = &current_step.on_failure {
                    self.resolve_sop_step(&sop, *step)?;
                }
                let detail = reason
                    .clone()
                    .unwrap_or_else(|| "checkpoint denied by operator".to_string());
                let now = now_iso8601();
                (
                    SopStepStatus::Failed,
                    detail.clone(),
                    serde_json::Value::String(detail),
                    now.clone(),
                    Some(now),
                )
            }
            super::approval::ApprovalDecision::Revise { .. } => {
                bail!("checkpoint revise decisions use the revision persistence path")
            }
        };

        let retries_consumed = prior_run
            .step_results
            .iter()
            .filter(|result| {
                result.step_number == current_step.number && result.status == SopStepStatus::Failed
            })
            .count()
            .try_into()
            .unwrap_or(u32::MAX);
        let denial_terminates = matches!(decision, super::approval::ApprovalDecision::Deny { .. })
            && matches!(
                route::failure::route_failure(
                    &current_step.on_failure,
                    retries_consumed,
                    self.config.max_step_retries,
                ),
                NextStep::Fail(_)
            );
        if denial_terminates {
            self.reacquire_claim_uncapped(run_id)?;
            if let Err(e) = self
                .store
                .mark_claim_retained_after_terminal_rollback(run_id)
            {
                self.release_claim_on_park(run_id);
                return Err(anyhow::Error::msg(format!(
                    "failed to persist terminal-rollback claim marker for run {run_id}: {e}"
                )));
            }
        } else {
            self.reacquire_claim_on_resume(run_id)?;
        }

        if let Some(run) = self.active_runs.get_mut(run_id) {
            run.status = SopRunStatus::Running;
            run.waiting_since = None;
            run.step_results.push(SopStepResult {
                step_number: current_step.number,
                status,
                output: recorded_output,
                started_at,
                completed_at,
                effective_agent: None,
                tool_calls: Vec::new(),
            });
        }

        let mut routed_status = status;
        if status == SopStepStatus::Completed {
            if let Err(reason) = self.validate_step_output(&current_step, &routed_output) {
                routed_status = SopStepStatus::Failed;
                let full_reason = format!(
                    "Step {} output schema validation failed: {reason}",
                    current_step.number
                );
                if let Some(recorded) = self
                    .active_runs
                    .get_mut(run_id)
                    .and_then(|run| run.step_results.last_mut())
                {
                    recorded.status = SopStepStatus::Failed;
                    recorded.output = full_reason;
                }
            } else if let Some(run) = self.active_runs.get_mut(run_id) {
                run.llm_calls_saved += 1;
            }
        }

        let route = match self.route_decision_after_recorded_step(
            run_id,
            &sop,
            &current_step,
            routed_status,
        ) {
            Ok(route) => route,
            Err(e) => {
                self.active_runs.insert(run_id.to_string(), prior_run);
                if !denial_terminates {
                    self.release_claim_on_park(run_id);
                }
                return Err(e);
            }
        };
        let event = super::approval::GateLedgerEntry {
            run_id: run_id.to_string(),
            step: current_step.number,
            gate_revision: Some(prior_run.revision),
            checkpoint_revision: Some(prior_run.revision),
            decision_identity: super::approval::broker::checkpoint_decision_identity(&decision)
                .map(|(_, identity)| identity),
            kind: super::approval::GateEventKind::Resolved,
            decision: Some(decision),
            principal,
            ts: now_iso8601(),
        }
        .into_event_record();

        match route {
            NextStep::Complete => {
                let saved = self
                    .active_runs
                    .get(run_id)
                    .map(|run| run.llm_calls_saved)
                    .unwrap_or(0);
                match self.finish_run_with_gate_event(run_id, SopRunStatus::Completed, None, &event)
                {
                    Ok(action) => {
                        self.deterministic_savings.total_llm_calls_saved += saved;
                        self.deterministic_savings.total_runs += 1;
                        Ok(action)
                    }
                    Err(e) => {
                        self.active_runs.insert(run_id.to_string(), prior_run);
                        if !denial_terminates {
                            self.release_claim_on_park(run_id);
                        }
                        Err(e)
                    }
                }
            }
            NextStep::Fail(reason) => match self.finish_run_with_gate_event(
                run_id,
                SopRunStatus::Failed,
                Some(reason),
                &event,
            ) {
                Ok(action) => Ok(action),
                Err(e) => {
                    self.active_runs.insert(run_id.to_string(), prior_run);
                    if !denial_terminates {
                        self.release_claim_on_park(run_id);
                    }
                    Err(e)
                }
            },
            next => {
                if let Err(e) = self.persist_active_with_gate_event(run_id, &event) {
                    self.active_runs.insert(run_id.to_string(), prior_run);
                    self.release_claim_on_park(run_id);
                    return Err(e);
                }
                self.apply_route_decision(
                    run_id,
                    &sop,
                    current_step.number,
                    next,
                    true,
                    Some(retry_input_value(&prior_run, current_step.number)),
                    Some(routed_output),
                )
            }
        }
    }

    /// Failure path for a denied checkpoint: record the checkpoint step `Failed`
    /// and route through its `on_failure` policy via the shared deterministic
    /// record-and-route chokepoint. `Goto` reaches the authored failure step;
    /// the default `Fail` terminates the run `Failed`. Mirrors `approve_step`'s
    /// guard so a wrong-status or missing run fails closed with the gate intact.
    fn deny_checkpoint(&mut self, run_id: &str, reason: Option<String>) -> Result<SopRunAction> {
        let status = self
            .active_runs
            .get(run_id)
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"run_id": run_id})),
                    "SOP engine: active run not found"
                );
                anyhow::Error::msg(format!("Active run not found: {run_id}"))
            })?
            .status;

        if status != SopRunStatus::PausedCheckpoint {
            bail!("Run {run_id} is not paused at a checkpoint (status: {status})");
        }

        if self.is_park_persist_pending(run_id) {
            bail!(
                "Run {run_id} cannot resolve: its parked checkpoint snapshot is not yet durably persisted (retrying)"
            );
        }

        let (_, sop) = self.resolve_active_run_sop(run_id)?;
        let current_step_number = self
            .active_runs
            .get(run_id)
            .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?
            .current_step;
        let current_step = self.resolve_sop_step(&sop, current_step_number)?;

        // Resolve a failure-route target before mutating the parked run. A stale
        // `Goto` must leave the checkpoint untouched and re-resolvable.
        if let super::step_contract::StepFailure::Goto { step } = &current_step.on_failure {
            self.resolve_sop_step(&sop, *step)?;
        }

        let prior_run = self
            .active_runs
            .get(run_id)
            .cloned()
            .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
        // Classify the denial's routing outcome BEFORE any mutation, using the
        // AUTHORITATIVE failure router (not a second copy of its logic). A denial
        // records the checkpoint step `Failed`; the router computes `retries_consumed`
        // as (Failed count - 1) after that record, so before it the current Failed
        // count for this step is exactly that value.
        let retries_consumed = self
            .active_runs
            .get(run_id)
            .map(|run| {
                run.step_results
                    .iter()
                    .filter(|r| {
                        r.step_number == current_step.number && r.status == SopStepStatus::Failed
                    })
                    .count() as u32
            })
            .unwrap_or(0);
        let terminates = matches!(
            route::failure::route_failure(
                &current_step.on_failure,
                retries_consumed,
                self.config.max_step_retries,
            ),
            NextStep::Fail(_)
        );
        if terminates {
            // TERMINAL denial (default `Fail`, or a `Retry` whose budget is spent):
            // it must reacquire to complete atomically even under saturation - gating
            // a run that is ENDING on a free slot would strand it. This is the
            // terminal-rollback atomicity path; it stays UNCAPPED by design.
            self.reacquire_claim_uncapped(run_id)?;
        } else {
            // CONTINUING denial (`Goto`, or a `Retry` with budget remaining): it
            // resumes execution, so it must pass the SAME capped store CAS every other
            // resume-to-continue path uses, honoring the per-SOP and global limits. At
            // capacity this returns `ResumeAtCapacity`; the `?` early-returns with the
            // checkpoint still parked and re-resolvable (no mutation, no retention
            // marker yet) - typed backpressure, never an over-cap execution.
            self.reacquire_claim_on_resume(run_id)?;
        }
        if let Err(marker_err) = self
            .store
            .mark_claim_retained_after_terminal_rollback(run_id)
        {
            self.active_runs.insert(run_id.to_string(), prior_run);
            self.release_claim_on_park(run_id);
            return Err(anyhow::Error::msg(format!(
                "failed to persist terminal-rollback claim retention marker for run {run_id}: {marker_err}"
            )));
        }
        self.claims_retained_after_terminal_rollback
            .insert(run_id.to_string());

        let detail = reason.unwrap_or_else(|| "checkpoint denied by operator".to_string());
        let now = now_iso8601();

        if let Some(run) = self.active_runs.get_mut(run_id) {
            run.status = SopRunStatus::Running;
            run.waiting_since = None;
        }
        match self.record_deterministic_step_result(
            run_id,
            &sop,
            &current_step,
            SopStepStatus::Failed,
            detail.clone(),
            serde_json::Value::String(detail.clone()),
            now.clone(),
            Some(now),
        ) {
            Ok(action) => {
                if !self.persist_active_checked(run_id) {
                    self.active_runs.insert(run_id.to_string(), prior_run);
                    self.claims_pending_persist.remove(run_id);
                    self.claims_retained_after_terminal_rollback.remove(run_id);
                    self.release_claim_on_park(run_id);
                    return Err(anyhow::Error::msg(format!(
                        "failed to persist checkpoint denial transition for run {run_id}"
                    )));
                }
                if self.active_runs.get(run_id).is_some_and(|run| {
                    matches!(
                        run.status,
                        SopRunStatus::WaitingApproval | SopRunStatus::PausedCheckpoint
                    )
                }) {
                    // The denial ROUTED to another gate and the new parked snapshot
                    // is durably persisted, so this run continued — it did NOT terminal-
                    // rollback. The reacquired claim still carries the durable terminal-
                    // rollback retention marker, which is now stale. Clear it with a
                    // CHECKED release: a swallowed failure would leave a live durable
                    // marker on a continued run, which `restore_runs` would then renew
                    // forever (the slot leak this PR exists to prevent). If the release
                    // fails we must NOT report success with a live marker — roll back to
                    // the pre-decision park, drop the in-memory retention/pending
                    // tracking (so the stale claim is not heartbeated and the lease
                    // reaper frees it), and surface the error so the caller retries.
                    if let Err(e) = self.release_claim_checked(run_id) {
                        self.active_runs.insert(run_id.to_string(), prior_run);
                        self.claims_pending_persist.remove(run_id);
                        self.claims_retained_after_terminal_rollback.remove(run_id);
                        return Err(anyhow::Error::msg(format!(
                            "failed to release exec claim after routing checkpoint denial for run {run_id}: {e}"
                        )));
                    }
                    self.claims_pending_persist.remove(run_id);
                }
                self.claims_retained_after_terminal_rollback.remove(run_id);
                self.record_transition_event(
                    run_id,
                    "checkpoint_denied",
                    Some(detail),
                    ::serde_json::json!({
                        "step": current_step.number,
                        "kind": current_step.kind.to_string(),
                    }),
                );
                Ok(action)
            }
            Err(e) => {
                self.active_runs.insert(run_id.to_string(), prior_run);
                // The terminal write was rejected, so the durable store may still
                // restore this parked run. Keep the claim acquired for this decision
                // attempt to prevent another trigger from taking its execution slot.
                Err(e)
            }
        }
    }

    /// Prepare a `WaitingApproval` gate clear: mutate the in-memory run to the
    /// target state and describe how the wrapper must commit it with the gate
    /// ledger row. The wrapper owns persistence and post-commit secondary events.
    ///
    /// All-or-nothing: the SOP definition and current step are resolved (and
    /// bounds-checked) BEFORE any in-memory mutation, so a definition removed or
    /// shrunk mid-run returns `Err` with the gate left untouched (still
    /// `WaitingApproval`, re-resolvable) rather than half-transitioned or panicking
    /// on an out-of-range step index (which would poison the engine mutex). The
    /// pure prefix of these lookups is exposed as `can_clear_waiting_gate` so
    /// `resolve_gate` can fail closed before it touches the claim or the ledger.
    fn clear_waiting_gate(&mut self, run_id: &str) -> Result<GateClearTransition> {
        let (sop_name, current_step) = {
            let run = self
                .active_runs
                .get(run_id)
                .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
            (run.sop_name.clone(), run.current_step)
        };

        let sop = self
            .sops
            .iter()
            .find(|s| s.name == sop_name)
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"sop_name": sop_name})),
                    "SOP engine: sop no longer loaded (definition removed mid-run)"
                );
                anyhow::Error::msg(format!("SOP '{sop_name}' no longer loaded"))
            })?
            .clone();

        // Resolve the waiting step by its NUMBER (not vec position): a routed SOP with
        // non-contiguous step numbers (e.g. 1, 5) means position != number, and a
        // positional lookup would resume the wrong step - and, worse, only AFTER
        // resolve_gate already reacquired the claim and wrote the gate_resolved row.
        let step = self.resolve_sop_step(&sop, current_step)?;

        let run_data = {
            let run = self
                .active_runs
                .get(run_id)
                .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
            RunData::from_step_results(&run.step_results)
        };
        if !route::eligible(&step, &run_data) {
            return self.gate_step_pending_transition(
                run_id,
                &sop,
                step.number,
                format!("step {} dependencies not satisfied", step.number),
            );
        }

        let input = {
            let run = self
                .active_runs
                .get(run_id)
                .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
            step_input_value(run, step.number)
        };
        if let Some(reason) = self.schema_input_failure_reason(&step, &input) {
            return self.gate_schema_failure_transition(run_id, step.number, "input", reason);
        }

        // The exec claim was already re-acquired by resolve_gate BEFORE the audit row
        // (so a claim failure never writes a false gate_resolved row, and the run
        // holds its claim before EITHER the Pending or the Running transition here).

        // The lookups succeeded; commit the transition.
        let run = self
            .active_runs
            .get_mut(run_id)
            .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
        run.status = SopRunStatus::Running;
        run.waiting_since = None;
        let context = format_step_context(&sop, run, &step, &self.config);

        let mut step = step;
        step.agent = step
            .effective_agent(sop.agent.as_deref())
            .map(str::to_string);

        Ok(GateClearTransition::Active {
            action: Box::new(SopRunAction::ExecuteStep {
                run_id: run_id.to_string(),
                step,
                context,
            }),
            follow_up: None,
        })
    }

    /// List finished runs, optionally filtered by SOP name.
    pub fn finished_runs(&self, sop_name: Option<&str>) -> Vec<&SopRun> {
        self.finished_runs
            .iter()
            .filter(|r| sop_name.is_none_or(|name| r.sop_name == name))
            .collect()
    }

    /// Summaries of every run the engine currently holds: live runs from the
    /// active set plus retained terminal runs, newest first by start time.
    /// This is the enumeration the Runs surface polls; it never touches the
    /// durable store directly, so it reflects exactly what the running engine
    /// knows (active set + `max_finished_runs` retention window).
    pub fn run_summaries(&self, sop_name: Option<&str>) -> Vec<SopRunSummary> {
        let mut out: Vec<SopRunSummary> = self
            .active_runs
            .values()
            .filter(|r| sop_name.is_none_or(|name| r.sop_name == name))
            .map(|r| SopRunSummary::from_run(r, true))
            .chain(
                self.finished_runs
                    .iter()
                    .filter(|r| sop_name.is_none_or(|name| r.sop_name == name))
                    .map(|r| SopRunSummary::from_run(r, false)),
            )
            .collect();
        out.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        out
    }

    /// Return cumulative deterministic execution savings.
    pub fn deterministic_savings(&self) -> &DeterministicSavings {
        &self.deterministic_savings
    }

    /// Save a procedural-memory proposal into the shared SOP store. This is the
    /// production-facing engine surface EPIC F consumes for approval/write-back.
    pub fn save_proposal(&self, proposal: &ProposalRecord) -> Result<(), StoreError> {
        self.store.save_proposal(proposal)
    }

    /// Load a procedural-memory proposal by id from the shared SOP store.
    pub fn load_proposal(&self, id: &str) -> Result<Option<ProposalRecord>, StoreError> {
        self.store.load_proposal(id)
    }

    /// List procedural-memory proposals, optionally filtered by lifecycle status.
    pub fn list_proposals(
        &self,
        status: Option<ProposalStatus>,
    ) -> Result<Vec<ProposalRecord>, StoreError> {
        self.store.list_proposals(status)
    }

    // ── Approval timeout ──────────────────────────────────────────

    pub fn check_approval_timeouts(&mut self) -> Vec<SopRunAction> {
        let action_cfg = self.config.approval_timeout_action;
        let mut actions = Vec::new();
        for run_id in self.overdue_waiting_run_ids() {
            if let Some(a) =
                super::approval::timeout::apply_timeout_action(self, &run_id, action_cfg)
            {
                actions.push(a);
            }
        }
        actions
    }

    fn overdue_waiting_run_ids(&self) -> Vec<String> {
        let timeout_secs = self.config.approval_timeout_secs;
        if timeout_secs == 0 {
            return Vec::new();
        }
        // cooldown_elapsed(ts, secs) returns true when (now - ts) >= secs.
        self.active_runs
            .values()
            .filter(|r| r.status == SopRunStatus::WaitingApproval)
            .filter(|r| !self.is_park_persist_pending(&r.run_id))
            .filter(|r| {
                r.waiting_since
                    .as_deref()
                    .is_some_and(|ts| cooldown_elapsed(ts, timeout_secs))
            })
            .map(|r| r.run_id.clone())
            .collect()
    }

    pub fn run_maintenance_tick(&mut self) -> MaintenanceSummary {
        // Count overdue gates BEFORE applying the action: the fail-closed Escalate
        // default re-stamps in place and produces no action, so counting actions
        // alone would under-report the escalations.
        let timed_out = self.overdue_waiting_run_ids().len();
        let timeout_actions = self.check_approval_timeouts();
        self.retry_pending_park_persists();
        self.retry_capacity_blocked_gated_pends();
        self.heartbeat_active_claims();
        let reaped_claims = self.reap_expired_claims();
        let pruned_runs = self.prune_terminal_runs();
        MaintenanceSummary {
            timed_out,
            reaped_claims,
            pruned_runs,
            timeout_actions,
        }
    }

    /// Reclaim concurrency-claim leases past their expiry (the holder died without
    /// releasing). Best-effort: a store error is logged and the pass continues.
    /// Returns the number reclaimed.
    fn reap_expired_claims(&self) -> usize {
        let now = now_iso8601();
        let expired = match self.store.expired_claims(&now) {
            Ok(claims) => claims,
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": e.to_string()})),
                    "SOP maintenance: failed to read expired claims"
                );
                return 0;
            }
        };
        let mut reclaimed = 0;
        for token in &expired {
            match self.store.release_claim(token) {
                Ok(()) => reclaimed += 1,
                Err(e) => ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": e.to_string()})),
                    "SOP maintenance: failed to release expired claim"
                ),
            }
        }
        reclaimed
    }

    /// Drop terminal runs beyond the retention policy (`max_finished_runs`).
    /// Best-effort; returns the number pruned.
    fn prune_terminal_runs(&self) -> usize {
        let policy = RetentionPolicy {
            max_terminal: self.config.max_finished_runs,
            keep_secs: None,
        };
        match self.store.prune(&policy) {
            Ok(n) => n,
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": e.to_string()})),
                    "SOP maintenance: failed to prune terminal runs"
                );
                0
            }
        }
    }

    /// Re-stamp a run's `waiting_since` to now (timeout escalation: the gate stays
    /// open but the clock resets so it re-surfaces, not self-approves).
    pub(crate) fn restamp_waiting_with_gate_event(
        &mut self,
        run_id: &str,
        event: &SopEventRecord,
    ) -> Result<()> {
        let previous = match self.active_runs.get_mut(run_id) {
            Some(run) => {
                let previous = run.waiting_since.clone();
                run.waiting_since = Some(now_iso8601());
                previous
            }
            None => return Ok(()),
        };
        // Persist the re-stamped clock with the escalation event as one durable
        // outcome; otherwise history could say the gate escalated while the
        // timeout clock still points at the old overdue instant.
        if let Err(e) = self.persist_active_with_gate_event(run_id, event) {
            if let Some(run) = self.active_runs.get_mut(run_id) {
                run.waiting_since = previous;
            }
            return Err(e);
        }
        Ok(())
    }

    /// The current step number of an active run (0 if absent). For ledger rows.
    pub(crate) fn run_current_step(&self, run_id: &str) -> u32 {
        self.active_runs
            .get(run_id)
            .map(|r| r.current_step)
            .unwrap_or(0)
    }

    // ── Test helpers ──────────────────────────────────────────────

    /// Replace loaded SOPs (for testing from other modules).
    // Available for cross-crate testing
    pub fn set_sops_for_test(&mut self, sops: Vec<Sop>) {
        self.sops = sops;
    }

    /// Replace the live `[sop.approval]` config (for testing a mid-flight reload from
    /// other modules) - so a test can revoke a group membership while a quorum gate is
    /// parked and assert the earlier voter stops counting.
    #[cfg(test)]
    pub(crate) fn set_approval_config_for_test(
        &mut self,
        approval: zeroclaw_config::schema::SopApprovalConfig,
    ) {
        self.config.approval = approval;
    }

    // ── Internal helpers ────────────────────────────────────────

    pub fn last_finished_run(&self, sop_name: &str) -> Option<&SopRun> {
        self.finished_runs
            .iter()
            .rev()
            .find(|r| r.sop_name == sop_name)
    }

    pub fn finish_run(
        &mut self,
        run_id: &str,
        status: SopRunStatus,
        reason: Option<String>,
    ) -> Result<SopRunAction> {
        let mut run = self
            .active_runs
            .get(run_id)
            .cloned()
            .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
        run.status = status;
        run.completed_at = Some(now_iso8601());
        let sop_name = run.sop_name.clone();
        let run_id_owned = run.run_id.clone();
        self.persist_terminal(&run)?;
        self.claims_pending_persist.remove(run_id);
        self.claims_retained_after_terminal_rollback.remove(run_id);
        self.active_runs.remove(run_id);
        self.metrics.record_run_complete(&run);
        // The park snapshot is purely a rehydration artifact: a terminal run must
        // not leave one behind claiming `paused_at_checkpoint`. Decisions and the
        // final status live in the run store / approval ledger, not the snapshot.
        self.remove_deterministic_state_file(&run);
        self.finished_runs.push(run);

        // Evict oldest finished runs when over capacity
        let max = self.config.max_finished_runs;
        if max > 0 && self.finished_runs.len() > max {
            let excess = self.finished_runs.len() - max;
            self.finished_runs.drain(..excess);
        }

        Ok(match status {
            SopRunStatus::Failed => SopRunAction::Failed {
                run_id: run_id_owned,
                sop_name,
                reason: reason.unwrap_or_default(),
            },
            _ => SopRunAction::Completed {
                run_id: run_id_owned,
                sop_name,
            },
        })
    }

    pub(crate) fn finish_run_with_gate_event(
        &mut self,
        run_id: &str,
        status: SopRunStatus,
        reason: Option<String>,
        event: &SopEventRecord,
    ) -> Result<SopRunAction> {
        let mut run = self
            .active_runs
            .get(run_id)
            .cloned()
            .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
        run.status = status;
        run.completed_at = Some(now_iso8601());
        let sop_name = run.sop_name.clone();
        let run_id_owned = run.run_id.clone();
        self.persist_terminal_with_gate_event(&run, event)?;
        self.claims_pending_persist.remove(run_id);
        self.claims_retained_after_terminal_rollback.remove(run_id);
        self.active_runs.remove(run_id);
        self.metrics.record_run_complete(&run);
        self.remove_deterministic_state_file(&run);
        self.finished_runs.push(run);

        let max = self.config.max_finished_runs;
        if max > 0 && self.finished_runs.len() > max {
            let excess = self.finished_runs.len() - max;
            self.finished_runs.drain(..excess);
        }

        Ok(match status {
            SopRunStatus::Failed => SopRunAction::Failed {
                run_id: run_id_owned,
                sop_name,
                reason: reason.unwrap_or_default(),
            },
            _ => SopRunAction::Completed {
                run_id: run_id_owned,
                sop_name,
            },
        })
    }

    pub(crate) fn clear_waiting_gate_with_event(
        &mut self,
        run_id: &str,
        event: &SopEventRecord,
    ) -> Result<SopRunAction> {
        let prior_run = self
            .active_runs
            .get(run_id)
            .cloned()
            .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
        let action = match self.clear_waiting_gate(run_id) {
            Ok(transition) => match transition {
                GateClearTransition::Active { action, follow_up } => {
                    if let Err(e) = self.persist_active_with_gate_event(run_id, event) {
                        self.active_runs.insert(run_id.to_string(), prior_run);
                        self.release_claim_on_park(run_id);
                        return Err(e);
                    }
                    if let Some(follow_up) = follow_up {
                        self.record_gate_resolution_follow_up(run_id, follow_up);
                    }
                    *action
                }
                GateClearTransition::Terminal {
                    status,
                    reason,
                    follow_up,
                } => {
                    let action =
                        match self.finish_run_with_gate_event(run_id, status, reason, event) {
                            Ok(action) => action,
                            Err(e) => {
                                self.active_runs.insert(run_id.to_string(), prior_run);
                                self.release_claim_on_park(run_id);
                                return Err(e);
                            }
                        };
                    if let Some(follow_up) = follow_up {
                        self.record_gate_resolution_follow_up(run_id, follow_up);
                    }
                    action
                }
            },
            Err(e) => {
                self.active_runs.insert(run_id.to_string(), prior_run);
                self.release_claim_on_park(run_id);
                return Err(e);
            }
        };
        Ok(action)
    }

    // ── EPIC C: out-of-band approval plane ──────────────────────────

    /// Read-only config access for the approval resolver.
    pub fn config(&self) -> &SopConfig {
        &self.config
    }

    /// The live `[sop.approval]` config - the single source of truth for approval
    /// groups and policies. The broker resolves membership/policy from this at
    /// use-time rather than holding a cloned copy that could drift on reload.
    pub fn approval_config(&self) -> &zeroclaw_config::schema::SopApprovalConfig {
        &self.config.approval
    }

    /// Fallible lookup for the approval policy that applies to the run's current
    /// parked step. `Ok(None)` means the step is intentionally unpoliced; `Err`
    /// means the live run/SOP/step state is unavailable and callers must fail
    /// closed rather than treating it as unpoliced.
    pub(crate) fn current_step_policy_lookup(&self, run_id: &str) -> Result<Option<String>> {
        let run = self
            .get_run(run_id)
            .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
        let sop = self.get_sop(&run.sop_name).ok_or_else(|| {
            anyhow::Error::msg(format!("SOP '{}' no longer loaded", run.sop_name))
        })?;
        // Match the step by its `number`, NOT by vec position: routed / non-contiguous
        // step numbers mean position != number, and a positional lookup would read the
        // wrong step's policy (silently unpolicing a policied gate, or vice versa).
        let step = sop
            .steps
            .iter()
            .find(|s| s.number == run.current_step)
            .ok_or_else(|| {
                anyhow::Error::msg(format!(
                    "SOP '{}' no longer contains step {}",
                    run.sop_name, run.current_step
                ))
            })?;
        let Some(name) = step.policy.as_deref() else {
            return Ok(None);
        };
        let name = name.trim();
        // An empty/whitespace name means "no policy", same as the Markdown parser's
        // `policy:` bullet (mod.rs). Without this, a TOML `policy = ""` step would
        // deserialize as `Some("")` and the broker would treat it as a NAMED-but-absent
        // policy (fail closed, gate stuck waiting forever) instead of unpoliced -
        // diverging from the equivalent Markdown SOP, which normalizes to `None`.
        Ok((!name.is_empty()).then(|| name.to_string()))
    }

    /// The name of the approval policy that applies to the run's current step, if
    /// that step names one. Read surfaces collapse unavailable live state to
    /// `None`; the broker uses the fallible lookup above to fail closed.
    pub fn current_step_policy_name(&self, run_id: &str) -> Option<String> {
        self.current_step_policy_lookup(run_id).ok().flatten()
    }

    /// Classify a run's approval gate for `resolve_gate` (idempotency + typed
    /// not-found). `Running` (already approved) and terminal runs are
    /// `AlreadyResolved`; an unknown run or a non-`WaitingApproval` active status
    /// (e.g. a deterministic `PausedCheckpoint`, which `approve_step` owns) is
    /// `NotApplicable`.
    pub(crate) fn gate_state(&self, run_id: &str) -> GateState {
        if let Some(run) = self.active_runs.get(run_id) {
            match run.status {
                SopRunStatus::WaitingApproval => GateState::Waiting {
                    step: run.current_step,
                },
                SopRunStatus::Running => GateState::AlreadyResolved,
                _ => GateState::NotApplicable,
            }
        } else if self.finished_runs.iter().any(|r| r.run_id == run_id) {
            GateState::AlreadyResolved
        } else {
            GateState::NotApplicable
        }
    }

    /// Ordered event/ledger history for a run (from the durable store).
    pub fn run_events(&self, run_id: &str) -> Result<Vec<SopEventRecord>, StoreError> {
        self.store.list_events(run_id)
    }

    /// EPIC G (broker quorum): record an approver's vote on a still-waiting gate as
    /// an append-only ledger row (kind `gate_vote`, actor = the principal). Quorum is
    /// counted from these rows so votes are durable and survive a restart. Distinct
    /// from `gate_resolved`, which is appended only once the gate actually clears.
    ///
    /// IDEMPOTENT per `(run, step, policy, voter_key)`: a repeat vote by the same voter
    /// under the same policy is a no-op, so retries (e.g. an approver clicking twice
    /// while the gate is still pending quorum) do not grow the append-only log with
    /// duplicate rows. The count already dedups by `voter_key`, so this changes storage
    /// footprint, not the tally. A read failure is surfaced (fail-closed) rather than
    /// risking a duplicate append.
    pub(crate) fn record_gate_vote(
        &self,
        run_id: &str,
        step: u32,
        policy: &str,
        gate_revision: u32,
        principal: &super::approval::ApprovalPrincipal,
    ) -> Result<(), StoreError> {
        self.record_gate_vote_scoped(
            run_id,
            step,
            policy,
            Some(gate_revision),
            None,
            None,
            principal,
        )
    }

    /// Record a quorum vote for a deterministic checkpoint presentation. Checkpoint
    /// votes must be scoped tighter than approval-gate votes because the same step
    /// can be answered with materially different public-mutation decisions.
    pub(crate) fn record_checkpoint_gate_vote(
        &self,
        run_id: &str,
        step: u32,
        policy: &str,
        checkpoint_revision: u32,
        decision_label: &str,
        decision_identity: &str,
        principal: &super::approval::ApprovalPrincipal,
    ) -> Result<(), StoreError> {
        self.record_gate_vote_scoped(
            run_id,
            step,
            policy,
            Some(checkpoint_revision),
            Some(decision_label),
            Some(decision_identity),
            principal,
        )
    }

    fn record_gate_vote_scoped(
        &self,
        run_id: &str,
        step: u32,
        policy: &str,
        gate_revision: Option<u32>,
        decision_label: Option<&str>,
        decision_identity: Option<&str>,
        principal: &super::approval::ApprovalPrincipal,
    ) -> Result<(), StoreError> {
        let voter_key = principal.voter_key();
        if self.gate_votes_for_step(run_id, step)?.iter().any(|vote| {
            vote.voter_key == voter_key
                && vote.policy.as_deref() == Some(policy)
                && vote.gate_revision == gate_revision
                && vote.decision_identity.as_deref() == decision_identity
        }) {
            return Ok(());
        }
        let mut payload = serde_json::json!({
            "step": step,
            "source": principal.source_label(),
            "policy": policy,
            "identity": principal.identity,
        });
        if let Some(object) = payload.as_object_mut() {
            if let Some(revision) = gate_revision {
                object.insert(
                    "gate_revision".to_string(),
                    serde_json::Value::Number(revision.into()),
                );
                if decision_identity.is_some() {
                    object.insert(
                        "checkpoint_revision".to_string(),
                        serde_json::Value::Number(revision.into()),
                    );
                }
            }
            if let Some(label) = decision_label {
                object.insert(
                    "decision".to_string(),
                    serde_json::Value::String(label.to_string()),
                );
            }
            if let Some(identity) = decision_identity {
                object.insert(
                    "decision_identity".to_string(),
                    serde_json::Value::String(identity.to_string()),
                );
            }
        }
        let ev = SopEventRecord {
            run_id: run_id.to_string(),
            seq: 0,
            ts: now_iso8601(),
            kind: "gate_vote".to_string(),
            // `voter_key()` deliberately collapses `Http`/`Ws` to one canonical
            // `gateway:<id>` voter (same paired token, two transports = one voter),
            // while the agent/CLI sources stay distinct. See `ApprovalPrincipal::
            // voter_key`'s own doc for the full canonicalization rationale.
            actor: Some(voter_key),
            reason: None,
            // `policy` scopes the vote to the policy in effect when it was cast, and
            // `source`/`identity` capture enough to REVALIDATE the voter against the
            // current required group at count time - so a mid-flight policy or group
            // change cannot let a stale vote count toward the new quorum.
            payload,
        };
        self.store.append_event(&ev).map(|_| ())
    }

    /// EPIC G (broker quorum): the recorded approval votes on `run_id` AT `step`, read
    /// from the append-only `gate_vote` ledger rows. Each row carries the canonical
    /// `voter_key` (source-qualified, `Http`/`Ws` collapsed - see
    /// [`super::approval::ApprovalPrincipal::voter_key`]) plus the `policy` in effect
    /// when the vote was cast and the `source`/`identity` needed to REVALIDATE the
    /// voter against the current required group. The broker owns the tally (scope to
    /// the current policy, revalidate membership, then dedup by `voter_key`) because
    /// the policy/group/resolver live there; the engine only surfaces the durable rows.
    ///
    /// A read failure is SURFACED, never collapsed to an empty tally: an unreadable
    /// ledger must fail the resolve closed (gate stays waiting for a retry), not report
    /// a bogus zero quorum after a vote was durably appended.
    pub(crate) fn gate_votes_for_step(
        &self,
        run_id: &str,
        step: u32,
    ) -> Result<Vec<GateVote>, StoreError> {
        let events = self.store.list_events(run_id).map_err(|e| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "run_id": run_id,
                        "step": step,
                        "error": e.to_string(),
                    })),
                "SOP engine: quorum voter count could not read the gate ledger (fail-closed, gate stays waiting)"
            );
            e
        })?;
        let mut votes: Vec<GateVote> = Vec::new();
        for ev in events {
            if ev.kind == "gate_vote"
                && ev.payload.get("step").and_then(|s| s.as_u64()) == Some(u64::from(step))
                && let Some(voter_key) = ev.actor
            {
                let str_field = |k: &str| {
                    ev.payload
                        .get(k)
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                };
                votes.push(GateVote {
                    voter_key,
                    policy: str_field("policy"),
                    source: str_field("source"),
                    identity: str_field("identity"),
                    gate_revision: ev
                        .payload
                        .get("gate_revision")
                        .or_else(|| ev.payload.get("checkpoint_revision"))
                        .and_then(|value| value.as_u64())
                        .and_then(|value| u32::try_from(value).ok()),
                    checkpoint_revision: ev
                        .payload
                        .get("checkpoint_revision")
                        .and_then(|value| value.as_u64())
                        .and_then(|value| u32::try_from(value).ok()),
                    decision_identity: str_field("decision_identity"),
                });
            }
        }
        Ok(votes)
    }

    /// Record the approval completion metric at the gate-clearing chokepoint, so
    /// every principal (agent tool, CLI, gateway, WS, timeout) meters identically
    /// and the live counters agree with `SopMetricsCollector::rebuild_from_persistence`.
    /// `is_system` (the timeout principal) is metered as a timeout auto-approval;
    /// any other principal is a human approval. No-op if the run is gone.
    pub(crate) fn record_approval_metric(&self, run_id: &str, is_system: bool) {
        let Some(run) = self.get_run(run_id) else {
            return;
        };
        if is_system {
            self.metrics
                .record_timeout_auto_approve(&run.sop_name, &run.run_id);
        } else {
            self.metrics.record_approval(&run.sop_name, &run.run_id);
        }
    }

    pub fn resolve_gate(
        &mut self,
        run_id: &str,
        decision: super::approval::ApprovalDecision,
        principal: super::approval::ApprovalPrincipal,
    ) -> Result<super::approval::ResolveOutcome> {
        super::approval::resolve::resolve_gate(self, run_id, decision, principal)
    }
}

/// A recorded approval vote on a waiting gate (one `gate_vote` ledger row), as
/// surfaced by [`SopEngine::gate_votes_for_step`]. The broker scopes the tally to
/// the current `policy`, revalidates each voter (`source` + `identity`) against the
/// current required group, then dedups by `voter_key`.
pub(crate) struct GateVote {
    /// Canonical quorum-distinctness key (`Http`/`Ws` collapsed to `gateway`).
    pub voter_key: String,
    /// The `[sop.approval].policies.<name>` in effect when the vote was cast, or
    /// `None` for a vote recorded before this field existed (never counts toward a
    /// named current policy).
    pub policy: Option<String>,
    /// The voter's transport source label (`http`/`ws`/`cli`/`agent`), for membership
    /// revalidation.
    pub source: Option<String>,
    /// The voter's recorded identity (paired-token subject / agent alias / OS user),
    /// for membership revalidation. Recorded, not trusted.
    pub identity: Option<String>,
    /// Gate presentation revision used to prevent stale votes from a prior visit.
    pub gate_revision: Option<u32>,
    /// Checkpoint presentation revision, absent for ordinary approval-gate votes.
    pub checkpoint_revision: Option<u32>,
    /// Canonical hash identifying the exact positive checkpoint decision payload.
    pub decision_identity: Option<String>,
}

/// Classification of a run's approval-gate state (EPIC C `resolve_gate`).
pub(crate) enum GateState {
    /// Waiting on approval at this step number (resolvable).
    Waiting { step: u32 },
    /// Already resolved (running after approve, or terminal) - idempotent no-op.
    AlreadyResolved,
    /// Not a waiting-approval gate (unknown run, or a non-WaitingApproval status
    /// such as a deterministic `PausedCheckpoint`, which `approve_step` owns).
    NotApplicable,
}

// ── Trigger matching ────────────────────────────────────────────

/// Check whether a single trigger definition matches an incoming event.
///
/// Source class is the cheap gate: a trigger can only match an event from its
/// own source. Past that, matching is the trigger's own responsibility via its
/// `TriggerBehavior`, so there is no per-source logic to drift here.
fn trigger_matches(trigger: &SopTrigger, event: &SopEvent) -> bool {
    trigger.source() == event.source && trigger.behavior().matches(event)
}

// Trigger topic/path matchers live in `super::triggers` (crate-private);
// `trigger_source` imports them directly.

// ── Execution mode resolution ───────────────────────────────────
fn execution_mode_needs_approval(mode: SopExecutionMode, sop: &Sop, step: &SopStep) -> bool {
    match mode {
        // Deterministic mode is handled via start_deterministic_run;
        // if we reach here via the standard path, treat as Auto.
        SopExecutionMode::Auto | SopExecutionMode::Deterministic => false,
        SopExecutionMode::Supervised => {
            // Supervised: approval only before the first step
            step.number == 1
        }
        SopExecutionMode::StepByStep => true,
        SopExecutionMode::PriorityBased => match sop.priority {
            // [SEC-FLIP] Critical/High are the MOST dangerous runs, so they MUST
            // gate (was `=> false`, an inversion that auto-ran the riskiest SOPs).
            SopPriority::Critical | SopPriority::High => true,
            SopPriority::Normal | SopPriority::Low => {
                // Supervised behavior for normal/low
                step.number == 1
            }
        },
    }
}

fn step_requires_approval_gate(sop: &Sop, step: &SopStep) -> bool {
    if step.requires_confirmation {
        return true;
    }

    let effective_mode = step.mode.unwrap_or(sop.execution_mode);
    execution_mode_needs_approval(sop.execution_mode, sop, step)
        || execution_mode_needs_approval(effective_mode, sop, step)
}

pub(super) fn pending_step_blocks_direct_advance(sop: &Sop, step: &SopStep) -> bool {
    step.kind == SopStepKind::Checkpoint || step_requires_approval_gate(sop, step)
}

/// Determine the action for a step based on the effective execution mode.
fn resolve_step_action(sop: &Sop, step: &SopStep, run_id: String, context: String) -> SopRunAction {
    let mut step = step.clone();
    step.agent = step
        .effective_agent(sop.agent.as_deref())
        .map(str::to_string);
    let step = &step;

    if step_requires_approval_gate(sop, step) {
        SopRunAction::WaitApproval {
            run_id,
            step: step.clone(),
            context,
        }
    } else {
        SopRunAction::ExecuteStep {
            run_id,
            step: step.clone(),
            context,
        }
    }
}

// ── Step context formatting ─────────────────────────────────────

/// Build the structured context message that gets injected into the agent.
fn format_step_context(sop: &Sop, run: &SopRun, step: &SopStep, config: &SopConfig) -> String {
    let mut ctx = format!(
        "[SOP: {} (run {}) — Step {} of {}]\n\n",
        sop.name, run.run_id, step.number, run.total_steps
    );

    let marker_id = if run.frame_marker_id.is_empty() {
        run.run_id.as_str()
    } else {
        run.frame_marker_id.as_str()
    };
    ctx.push_str(&ContentSafety::from_sop_config(config).frame_for_context(
        run.trigger_event.payload.as_deref(),
        run.trigger_event.topic.as_deref(),
        run.trigger_event.source,
        marker_id,
    ));

    // Previous step summary
    if let Some(prev) = run.step_results.last() {
        let _ = writeln!(
            ctx,
            "Previous: Step {} {} — {}",
            prev.step_number, prev.status, prev.output
        );
    }

    let _ = write!(ctx, "\nCurrent step: **{}**\n{}\n", step.title, step.body);

    if !step.suggested_tools.is_empty() {
        let _ = write!(
            ctx,
            "\nSuggested tools: {}\n",
            step.suggested_tools.join(", ")
        );
    }

    ctx.push_str("\nWhen done, report your result.\n");

    ctx
}

pub(crate) fn step_input_value(run: &SopRun, step_number: u32) -> Value {
    if step_number <= 1 {
        return run
            .trigger_event
            .payload
            .as_deref()
            .map(jsonish_value)
            .unwrap_or(Value::Null);
    }

    run.step_results
        .last()
        .map(step_result_value)
        .unwrap_or(Value::Null)
}

/// Gate re-presentations per checkpoint a `Revise` may spend before the gate
/// insists on approve / edit / deny. Bounds operator-driven model spend.
pub(crate) const MAX_GATE_REVISIONS: u32 = 3;

/// The input that fed `step_number` when it originally ran: the output of the
/// step completed immediately BEFORE it in EXECUTION order (`step_results` is
/// append-only, so vec order IS execution order — numeric order would lie under
/// `Goto` routing), or the trigger payload when nothing ran before it. Used to
/// replay a step (a gate `Revise` re-draft) with exactly what it saw the first
/// time.
pub(crate) fn replay_input_for_step(run: &SopRun, step_number: u32) -> Value {
    let executed_at = run
        .step_results
        .iter()
        .rposition(|r| r.step_number == step_number && r.status == SopStepStatus::Completed);
    executed_at
        .and_then(|idx| {
            run.step_results[..idx]
                .iter()
                .rev()
                .find(|r| r.status == SopStepStatus::Completed)
                .map(step_result_value)
        })
        .unwrap_or_else(|| {
            run.trigger_event
                .payload
                .as_deref()
                .map(jsonish_value)
                .unwrap_or(Value::Null)
        })
}

pub(super) fn retry_input_value(run: &SopRun, step_number: u32) -> Value {
    if step_number <= 1 {
        return run
            .trigger_event
            .payload
            .as_deref()
            .map(jsonish_value)
            .unwrap_or(Value::Null);
    }

    run.step_results
        .iter()
        .rev()
        .find(|result| {
            result.status == SopStepStatus::Completed && result.step_number != step_number
        })
        .map(step_result_value)
        .unwrap_or(Value::Null)
}

pub(super) fn step_result_value(result: &SopStepResult) -> Value {
    jsonish_value(&result.output)
}

fn jsonish_value(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.into()))
}

// ── Utilities ───────────────────────────────────────────────────
// Timestamp helpers live in `super::time` (`now_iso8601` re-exported above).

/// A1: whether a run in `active_runs` currently occupies an execution slot (holds
/// a store CAS claim). A run parked at a HITL approval / deterministic checkpoint
/// releases its claim on park, so it does NOT hold a slot; every other non-terminal
/// status does. Keeps the in-memory admission fallback aligned with the store's
/// `claim_counts`, which counts only live (executing) claims.
fn holds_exec_claim(status: SopRunStatus) -> bool {
    !matches!(
        status,
        SopRunStatus::WaitingApproval | SopRunStatus::PausedCheckpoint
    )
}

#[cfg(test)]
mod tests;

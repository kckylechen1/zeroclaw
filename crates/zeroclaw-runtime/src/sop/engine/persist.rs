//! Durable run persistence, restore, and park-snapshot retry for [`SopEngine`].
//!
//! Extracted from `engine/mod.rs` so store write/retry chokepoints sit beside
//! each other rather than between claim helpers and the LLM advance path.
//! Claim admit/release stays in [`super::claims`]; this module only persists
//! state and coordinates park-snapshot → release sequencing.

use anyhow::Result;

use super::SopEngine;
use super::holds_exec_claim;
use super::now_iso8601;
use super::pending_step_blocks_direct_advance;
use crate::sop::store::{PersistedRun, SopEventRecord, StoreError};
use crate::sop::types::{Sop, SopRun, SopRunStatus, SopStepKind};

#[derive(Debug)]
pub(crate) struct TerminalPersistenceRetained {
    run_id: String,
    source: StoreError,
}

impl std::fmt::Display for TerminalPersistenceRetained {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "terminal persistence failed for run {}; active run and admission claim remain retained: {}",
            self.run_id, self.source
        )
    }
}

impl std::error::Error for TerminalPersistenceRetained {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

pub(super) enum ActivePersistOutcome {
    Saved,
    CapacityFull,
    Failed,
}

pub(crate) enum ParkPersistOutcome {
    Released,
    CapacityFull,
    PersistFailed,
}

impl SopEngine {
    /// Reconstruct in-flight runs from the store at startup (durable backends).
    /// No-op for the in-memory default. Does not overwrite already-present runs.
    pub fn restore_runs(&mut self) {
        match self.store.load_active_runs() {
            Ok(runs) => {
                let mut restored = 0usize;
                // Parking is durable before its out-of-band notice is attempted. A
                // daemon can therefore exit in the interval between those two
                // operations; replay the existing request seam after restore so a
                // parked gate cannot become invisible forever. Delivery is
                // intentionally at-least-once and keeps the canonical gate
                // reference, allowing adapters to de-duplicate it if needed.
                let mut replay_parked_requests = Vec::new();
                for pr in runs {
                    // A1: a run persisted while parked at a HITL approval / paused at
                    // a deterministic checkpoint normally holds NO exec claim - it
                    // released its slot on park. Restore it WITHOUT re-establishing a
                    // claim unless the live claim is explicitly marked as retained
                    // after a failed terminal checkpoint decision.
                    //
                    // An executing (Running/Pending) run DID hold a claim, so
                    // re-establish it WITHOUT admission caps: it was already admitted
                    // before the restart, so reconstruction is not new admission. This
                    // keeps `active_runs` and the live-claim count aligned 1:1 even for
                    // an over-cap restored set (the old capped `try_claim_run` silently
                    // dropped the claim over cap, leaving a locally active run with no
                    // store claim). On a renew error the run is left out of
                    // `active_runs` rather than cached orphaned, and the failure is
                    // logged loudly.
                    let parked = matches!(
                        pr.run.status,
                        SopRunStatus::WaitingApproval | SopRunStatus::PausedCheckpoint
                    );
                    if parked {
                        let retained = match self
                            .store
                            .has_retained_terminal_rollback_claim(&pr.run.run_id)
                        {
                            Ok(retained) => retained,
                            Err(e) => {
                                ::zeroclaw_log::record!(
                                    WARN,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Note
                                    )
                                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                    .with_attrs(
                                        ::serde_json::json!({
                                            "run_id": pr.run.run_id.as_str(),
                                            "error": e.to_string(),
                                        })
                                    ),
                                    "SOP engine: failed to inspect parked claim retention marker; failing closed (assuming retained)"
                                );
                                // FAIL CLOSED: a transient inspection read error must NOT
                                // discard a claim the terminal-rollback marker may exist to
                                // preserve (mapping it to `false` here would route into the
                                // release branch and drop that claim). Assume retained: the
                                // run keeps its claim. `heartbeat_claim` is an UPDATE-only
                                // no-op when the claim row is in fact already gone, so this
                                // cannot resurrect a released claim; the lease reaper reclaims
                                // a genuine orphan later. Erring toward keeping is the safe
                                // direction - releasing here could strand a run a real failed
                                // terminal write left restorable.
                                true
                            }
                        };
                        if retained && Self::terminal_rollback_marker_is_stale(&pr.run) {
                            // Crash-window reconcile: a terminal-rollback retention
                            // marker is legitimate ONLY when a genuine TERMINAL write
                            // failed and left the run restorable in its PRE-terminal
                            // parked state — i.e. still awaiting the (retried) decision
                            // at its current checkpoint, with NO recorded result for that
                            // step. A marker on a run that ALREADY recorded a terminal
                            // result for its current step reached this parked gate through
                            // a COMPLETED failure-route continuation (e.g. a denied
                            // checkpoint that Retried and re-parked). Its marker is stale —
                            // release it now rather than renew it forever.
                            ::zeroclaw_log::record!(
                                INFO,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Note
                                )
                                .with_attrs(::serde_json::json!({
                                    "run_id": pr.run.run_id.as_str(),
                                    "current_step": pr.run.current_step,
                                })),
                                "SOP engine: releasing stale terminal-rollback claim on a continued parked run"
                            );
                            self.release_claim_best_effort(&Self::claim_handle_for_run(&pr.run));
                        } else if retained {
                            self.claims_retained_after_terminal_rollback
                                .insert(pr.run.run_id.clone());
                            self.heartbeat_claim_for_run(&pr.run);
                        } else {
                            // A parked run normally holds no exec slot. A durable store
                            // written by OLD behavior can carry a stale `sop_claims` row
                            // for this run; RELEASE it now so the restored parked run is
                            // genuinely claim-less and does not block admission.
                            self.release_claim_best_effort(&Self::claim_handle_for_run(&pr.run));
                        }
                    } else if let Err(e) = self
                        .store
                        .renew_claim_for_restore(&pr.run.run_id, &pr.run.sop_name)
                    {
                        let span = ::zeroclaw_log::attribution_span!(&pr.run);
                        let _guard = span.enter();
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "run_id": pr.run.run_id.as_str(),
                                "sop_name": pr.run.sop_name.as_str(),
                                "error": e.to_string(),
                            })),
                            "SOP engine: dropping restored run, could not re-establish its store claim"
                        );
                        continue;
                    }
                    let run_id = pr.run.run_id.clone();
                    if self.active_runs.insert(run_id.clone(), pr.run).is_none() {
                        restored += 1;
                        if parked {
                            replay_parked_requests.push(run_id);
                        }
                    }
                }
                // Reuse the same policy resolution and request construction used
                // by a newly parked run. Restored runs already released any claim,
                // so this is delivery recovery only, not another park transition.
                for run_id in replay_parked_requests {
                    self.notify_park_request(&run_id);
                }
                if restored > 0 {
                    let span = ::zeroclaw_log::info_span!(
                        target: "zeroclaw_log_internal_scope",
                        "zeroclaw_scope",
                        sop_name = "*",
                    );
                    let _guard = span.enter();
                    ::zeroclaw_log::record!(
                        INFO,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_attrs(::serde_json::json!({"restored": restored})),
                        &format!("SOP engine restored {restored} run(s) from store")
                    );
                }
            }
            Err(e) => {
                let span = ::zeroclaw_log::info_span!(
                    target: "zeroclaw_log_internal_scope",
                    "zeroclaw_scope",
                    sop_name = "*",
                );
                let _guard = span.enter();
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": e.to_string()})),
                    "SOP engine: failed to restore runs from store"
                );
            }
        }
        self.restore_finished_runs();
    }
    /// Seed the display retention window (`finished_runs`) from the store's
    /// terminal records at boot, newest-first and capped at `max_finished_runs`.
    /// Terminal runs are durable but not part of the active-run rehydrate set, so
    /// without this the Runs surface drops all completed/failed/cancelled runs
    /// across a restart even though they remain on disk.
    pub(super) fn restore_finished_runs(&mut self) {
        let limit = self.config.max_finished_runs;
        match self.store.load_terminal_runs(limit) {
            Ok(runs) => {
                let mut seeded = 0usize;
                for pr in runs {
                    let span = ::zeroclaw_log::attribution_span!(&pr.run);
                    let _guard = span.enter();
                    ::zeroclaw_log::record!(
                        DEBUG,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Success)
                            .with_attrs(::serde_json::json!({
                                "run_id": pr.run.run_id.as_str(),
                                "sop_name": pr.run.sop_name.as_str(),
                            })),
                        "SOP engine: seeded terminal run into the retention window"
                    );
                    self.finished_runs.push(pr.run);
                    seeded += 1;
                }
                self.finished_runs
                    .sort_by(|a, b| a.started_at.cmp(&b.started_at));
                if seeded > 0 {
                    let span = ::zeroclaw_log::info_span!(
                        target: "zeroclaw_log_internal_scope",
                        "zeroclaw_scope",
                        sop_name = "*",
                    );
                    let _guard = span.enter();
                    ::zeroclaw_log::record!(
                        INFO,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_attrs(::serde_json::json!({"seeded": seeded})),
                        &format!(
                            "SOP engine seeded {seeded} terminal run(s) into the retention window"
                        )
                    );
                }
            }
            Err(e) => {
                let span = ::zeroclaw_log::info_span!(
                    target: "zeroclaw_log_internal_scope",
                    "zeroclaw_scope",
                    sop_name = "*",
                );
                let _guard = span.enter();
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": e.to_string()})),
                    "SOP engine: failed to seed terminal runs from store"
                );
            }
        }
    }
    /// Next monotonic revision for a run: one past whatever the store currently
    /// holds (0 if absent). Keeps every persist strictly newer so the store's
    /// revision guard accepts it; a cheap indexed lookup on either backend.
    pub(super) fn next_run_revision(&self, run_id: &str) -> u64 {
        match self.store.load_run(run_id) {
            Ok(Some(existing)) => existing.revision.saturating_add(1),
            _ => 0,
        }
    }
    /// Persist a still-active run (best-effort; logs on failure). Cheap no-op
    /// effect for the in-memory default.
    pub(super) fn persist_active(&self, run_id: &str) {
        let _ = self.persist_active_checked(run_id);
    }
    /// Persist a still-active run and REPORT whether the durable write succeeded.
    /// Returns `true` if there is no such active run (nothing to persist) or the
    /// snapshot was saved; `false` only if `save_run` errored. The park paths use
    /// this so they release the exec claim ONLY after the parked snapshot is
    /// durably written: a run parked in memory but NOT persisted must keep its
    /// slot, or a crash would leave the approval/checkpoint lost while newer
    /// triggers had already admitted into the "freed" slot.
    pub(super) fn persist_active_checked(&self, run_id: &str) -> bool {
        matches!(
            self.persist_active_checked_with_capacity(run_id, None),
            ActivePersistOutcome::Saved
        )
    }
    pub(super) fn persist_active_checked_with_capacity(
        &self,
        run_id: &str,
        max_pending: Option<usize>,
    ) -> ActivePersistOutcome {
        let Some(run) = self.active_runs.get(run_id) else {
            return ActivePersistOutcome::Saved;
        };
        self.heartbeat_claim_for_run(run);
        let mut pr = PersistedRun::new(run.clone(), now_iso8601(), run.trigger_event.source);
        // Each persist is a new state revision; the store rejects a
        // same-revision divergent write, so advance past what is stored.
        pr.revision = self.next_run_revision(run_id);
        let outcome = match max_pending {
            Some(max_pending) => {
                match self.store.save_run_with_pending_capacity(&pr, max_pending) {
                    Ok(true) => ActivePersistOutcome::Saved,
                    Ok(false) => ActivePersistOutcome::CapacityFull,
                    Err(e) => {
                        Self::log_persist_failure(run_id, e);
                        ActivePersistOutcome::Failed
                    }
                }
            }
            None => match self.store.save_run(&pr) {
                Ok(()) => ActivePersistOutcome::Saved,
                Err(e) => {
                    Self::log_persist_failure(run_id, e);
                    ActivePersistOutcome::Failed
                }
            },
        };
        if !matches!(outcome, ActivePersistOutcome::CapacityFull) {
            self.notify_run(run, true);
        }
        outcome
    }
    pub(super) fn log_persist_failure(run_id: &str, e: crate::sop::store::StoreError) {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"run_id": run_id, "error": e.to_string()})),
            "SOP engine: failed to persist run"
        );
    }
    pub(super) fn pending_capacity_limit_for_run(&self, run_id: &str) -> Option<usize> {
        let run = self.active_runs.get(run_id)?;
        let sop = self.sops.iter().find(|sop| sop.name == run.sop_name)?;
        (sop.max_pending_approvals > 0).then_some(sop.max_pending_approvals as usize)
    }
    pub(super) fn pending_pool_full_reason(&self, sop: &Sop) -> Option<String> {
        if sop.max_pending_approvals == 0 {
            return None;
        }
        let pending = self.pending_count_for_sop(&sop.name);
        if pending >= sop.max_pending_approvals as usize {
            Some(format!(
                "SOP '{}' pending-approval pool full ({pending}/{})",
                sop.name, sop.max_pending_approvals
            ))
        } else {
            None
        }
    }
    pub(super) fn pending_pool_capacity_raced_reason(&self, sop: &Sop) -> String {
        let pending = self.pending_count_for_sop(&sop.name);
        format!(
            "SOP '{}' pending-approval pool full ({pending}/{})",
            sop.name, sop.max_pending_approvals
        )
    }
    pub(super) fn log_pending_capacity_full(run_id: &str, reason: &str) {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"run_id": run_id, "reason": reason})),
            "SOP engine: pending-approval pool full at park transition; KEEPING the exec claim"
        );
    }
    pub(super) fn persisted_active_snapshot(&self, run_id: &str) -> Result<(PersistedRun, SopRun)> {
        let run = self
            .active_runs
            .get(run_id)
            .cloned()
            .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
        self.heartbeat_claim_for_run(&run);
        let mut persisted = PersistedRun::new(run.clone(), now_iso8601(), run.trigger_event.source);
        persisted.revision = self.next_run_revision(run_id);
        Ok((persisted, run))
    }
    /// Persist an active run transition and append its gate event as one store
    /// outcome. Used by `resolve_gate` so the durable gate ledger cannot get ahead
    /// of the run state transition it authorizes.
    pub(crate) fn persist_active_with_gate_event(
        &self,
        run_id: &str,
        event: &SopEventRecord,
    ) -> Result<()> {
        let (persisted, run) = self.persisted_active_snapshot(run_id)?;
        self.store.save_run_with_event(&persisted, event).map_err(|e| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(
                        ::serde_json::json!({"run_id": run_id, "error": e.to_string()})
                    ),
                "SOP engine: gate resolution persistence failed; run transition and ledger remain uncommitted"
            );
            anyhow::Error::new(e)
        })?;
        self.notify_run(&run, true);
        Ok(())
    }
    /// Park a run (WaitingApproval / PausedCheckpoint) and free its exec slot, but
    /// ONLY after the parked snapshot is durably persisted. If the persist fails,
    /// the claim is KEPT (fail closed): the run stays correctly counted against
    /// capacity, so it is never both claimless AND un-persisted (which a crash
    /// would turn into a lost park while newer triggers had already admitted into
    /// the "freed" slot). The slot is held until a later persist succeeds,
    /// trading a little concurrency for no lost park.
    pub(super) fn persist_parked_snapshot_then_release_claim(
        &mut self,
        run_id: &str,
    ) -> ParkPersistOutcome {
        let max_pending = self.pending_capacity_limit_for_run(run_id);
        match self.persist_active_checked_with_capacity(run_id, max_pending) {
            ActivePersistOutcome::Saved => {
                self.claims_pending_persist.remove(run_id);
                self.release_claim_on_park(run_id);
                ParkPersistOutcome::Released
            }
            ActivePersistOutcome::CapacityFull => ParkPersistOutcome::CapacityFull,
            ActivePersistOutcome::Failed => {
                // Track this run so `heartbeat_active_claims` keeps renewing its KEPT
                // claim despite the park status (see `claims_pending_persist`'s doc):
                // otherwise the claim's lease goes un-renewed and the maintenance
                // reaper reclaims it once it expires, silently undoing the fail-closed
                // keep and over-admitting a newer trigger.
                self.claims_pending_persist.insert(run_id.to_string());
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"run_id": run_id})),
                    "SOP engine: parked snapshot not persisted; KEEPING the exec claim (fail closed) so the park is not lost"
                );
                ParkPersistOutcome::PersistFailed
            }
        }
    }
    /// Retry the durable persist for every run in `claims_pending_persist`. A
    /// retry that now succeeds completes the deferred park transition (releases
    /// the claim). A retry that still fails, or that now finds the pending pool
    /// full, leaves the run tracked - but the persist helper heartbeats the claim
    /// BEFORE attempting the write, unconditionally, so even an unsaved retry still
    /// renews the kept claim's lease. This is what keeps `reap_expired_claims`
    /// from reclaiming it: called every maintenance tick, a park that never
    /// manages to persist still gets its claim renewed once per tick for as long
    /// as it stays parked.
    pub(super) fn retry_pending_park_persists(&mut self) {
        let pending: Vec<String> = self.claims_pending_persist.iter().cloned().collect();
        for run_id in pending {
            let Some(status) = self.active_runs.get(&run_id).map(|run| run.status) else {
                // The run left active_runs some other way (finished/evicted);
                // nothing left to retry or release.
                self.claims_pending_persist.remove(&run_id);
                continue;
            };
            let max_pending = self.pending_capacity_limit_for_run(&run_id);
            match self.persist_active_checked_with_capacity(&run_id, max_pending) {
                ActivePersistOutcome::Saved => {
                    self.claims_pending_persist.remove(&run_id);
                    // Only release the claim if the run is STILL parked. The entry
                    // guards in `resolve_gate`/`approve_step`/`resume_deterministic_run`
                    // (`is_park_persist_pending`) already refuse to resume a run while
                    // it is tracked here, so this should be unreachable in practice -
                    // but if a run somehow left the parked state without going through
                    // one of those guarded paths, its claim is now legitimately held
                    // by that transition and must NOT be released out from under it.
                    if !holds_exec_claim(status) {
                        self.release_claim_on_park(&run_id);
                        // The initial park deliberately withheld its route notice while
                        // the snapshot was not durable. This successful retry is the
                        // single point that makes the parked gate recoverable, so emit
                        // the deferred request now. Removing the pending marker first
                        // prevents later maintenance ticks from sending it again.
                        self.notify_park_request(&run_id);
                    }
                }
                ActivePersistOutcome::CapacityFull | ActivePersistOutcome::Failed => {}
            }
        }
    }
    pub(super) fn retry_capacity_blocked_gated_pends(&mut self) {
        let candidates: Vec<String> = self
            .active_runs
            .values()
            .filter(|run| run.status == SopRunStatus::Pending)
            .map(|run| run.run_id.clone())
            .collect();

        for run_id in candidates {
            let Some((sop, step)) = self.active_runs.get(&run_id).and_then(|run| {
                let sop = self.sops.iter().find(|sop| sop.name == run.sop_name)?;
                // Resolve the gated step by NUMBER, not vector index: step numbers
                // are not required to be contiguous/1-based, so an index lookup
                // strands a non-contiguous pending step (it never re-promotes and
                // leaks its exec claim).
                let step = sop
                    .steps
                    .iter()
                    .find(|step| step.number == run.current_step)?;
                pending_step_blocks_direct_advance(sop, step).then(|| (sop.clone(), step.clone()))
            }) else {
                continue;
            };

            if self.pending_pool_full_reason(&sop).is_some() {
                continue;
            }

            if step.kind == SopStepKind::Checkpoint {
                if let Err(e) = self.persist_deterministic_state(&run_id, &sop, true) {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "run_id": run_id,
                                "error": e.to_string(),
                            })),
                        "SOP maintenance: checkpoint pending-cap retry could not persist state"
                    );
                    continue;
                }
                if let Some(run) = self.active_runs.get_mut(&run_id) {
                    run.status = SopRunStatus::PausedCheckpoint;
                    run.waiting_since = Some(now_iso8601());
                }
            } else if let Some(run) = self.active_runs.get_mut(&run_id) {
                run.status = SopRunStatus::WaitingApproval;
                run.waiting_since = Some(now_iso8601());
            }

            match self.persist_parked_snapshot_then_release_claim(&run_id) {
                // The park is now durable: deliver the deferred approval-request
                // notice withheld while the initial persist was failing. This is a
                // no-op when the step has no policy request route.
                ParkPersistOutcome::Released => self.notify_park_request(&run_id),
                ParkPersistOutcome::PersistFailed => {}
                ParkPersistOutcome::CapacityFull => {
                    let reason = self.pending_pool_capacity_raced_reason(&sop);
                    Self::log_pending_capacity_full(&run_id, &reason);
                    self.mark_step_pending(&run_id, &sop, step.number, reason);
                }
            }
        }
    }
    /// True if `run_id`'s exec claim is being kept pending a retried park persist
    /// (`claims_pending_persist`): its most recent park snapshot has not yet been
    /// durably written. The three resume paths (`resolve_gate` via
    /// `clear_waiting_gate`, `approve_step`, `resume_deterministic_run`) must
    /// refuse to proceed while this is true - the kept claim predates the resume
    /// attempt, so a later rollback (on a ledger/audit failure) or a maintenance
    /// retry's release would either drop a claim that must survive, or release a
    /// claim out from under a run that has since started executing. Fail closed
    /// here instead: the gate/checkpoint stays parked, re-resolvable once a
    /// maintenance tick's retry durably persists the park.
    pub(crate) fn is_park_persist_pending(&self, run_id: &str) -> bool {
        self.claims_pending_persist.contains(run_id)
    }
    /// A prompt becomes stale only after a replacement presentation is durable.
    /// A gate can update its in-memory revision before its parked snapshot saves;
    /// finalizing the old prompt in that window would leave operators without a
    /// recoverable replacement after a crash.
    pub fn is_gate_reference_superseded(&self, run_id: &str, reference_revision: u32) -> bool {
        self.active_runs.get(run_id).is_some_and(|run| {
            run.revision != reference_revision && !self.is_park_persist_pending(run_id)
        })
    }
    /// Persist a run that has reached a terminal state and release its claim atomically.
    pub(super) fn persist_terminal(&self, run: &SopRun) -> Result<()> {
        let mut pr = PersistedRun::new(run.clone(), now_iso8601(), run.trigger_event.source);
        // The terminal write is the run's final revision; advance past the last
        // active snapshot so the store's revision guard accepts it.
        pr.revision = self.next_run_revision(&run.run_id);
        self.store.finish_run(&run.run_id, &pr).map_err(|e| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(
                        ::serde_json::json!({"run_id": run.run_id, "error": e.to_string()})
                    ),
                "SOP engine: terminal persistence failed; run and admission claim remain active"
            );
            anyhow::Error::new(TerminalPersistenceRetained {
                run_id: run.run_id.clone(),
                source: e,
            })
        })?;
        self.notify_run(run, false);
        Ok(())
    }
    /// Terminal counterpart to `persist_active_with_gate_event`: persist the
    /// terminal run, release its claim, and append the gate-resolution ledger row
    /// in one store transaction.
    pub(super) fn persist_terminal_with_gate_event(
        &self,
        run: &SopRun,
        event: &SopEventRecord,
    ) -> Result<()> {
        let mut pr = PersistedRun::new(run.clone(), now_iso8601(), run.trigger_event.source);
        pr.revision = self.next_run_revision(&run.run_id);
        self.store
            .finish_run_with_event(&run.run_id, &pr, event)
            .map_err(|e| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(
                            ::serde_json::json!({"run_id": run.run_id, "error": e.to_string()})
                        ),
                    "SOP engine: terminal gate resolution persistence failed; run and ledger remain uncommitted"
                );
                anyhow::Error::new(e)
            })?;
        self.notify_run(run, false);
        Ok(())
    }
    pub(super) fn record_transition_event(
        &self,
        run_id: &str,
        kind: &str,
        reason: Option<String>,
        payload: serde_json::Value,
    ) {
        let ev = SopEventRecord {
            run_id: run_id.to_string(),
            seq: 0,
            ts: now_iso8601(),
            kind: kind.to_string(),
            actor: None,
            reason,
            payload,
        };
        if let Err(e) = self.store.append_event(&ev) {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(
                        ::serde_json::json!({"run_id": run_id, "kind": kind, "error": e.to_string()})
                    ),
                "SOP engine: failed to append transition event"
            );
        }
    }
}

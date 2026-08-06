//! Exec-slot claim lifecycle for [`SopEngine`]: admit, heartbeat, park-release, resume.
//!
//! Extracted from `engine/mod.rs` so lease/CAS chokepoints are not interleaved with
//! persist and run-advance logic. The durable store remains the concurrency source of
//! truth; `active_runs` is only the execution cache.

use anyhow::{Result, bail};

use super::SopEngine;
use super::holds_exec_claim;
use crate::sop::store::ClaimToken;
use crate::sop::types::{Sop, SopRun};

/// Typed marker: a resume could not re-acquire an exec slot because the SOP's
/// per-SOP `max_concurrent` or the global `max_concurrent_total` is already
/// saturated. This is routine BACKPRESSURE, not a fault - kept distinct from a
/// store error so callers surface it as "at capacity, retry" (leaving the run
/// parked and re-resolvable) instead of logging it as a failure. It is the
/// signal that enforces the documented concurrency caps on the resume path: a
/// resume that would exceed them is refused rather than oversubscribed.
#[derive(Debug)]
pub(super) struct ResumeAtCapacity {
    run_id: String,
    sop_name: String,
}

impl std::fmt::Display for ResumeAtCapacity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "run {} ({}) cannot resume yet: execution slots are full; it stays parked and re-resolvable once a slot frees",
            self.run_id, self.sop_name
        )
    }
}

impl std::error::Error for ResumeAtCapacity {}

/// True when `err` is the typed `ResumeAtCapacity` backpressure marker (an
/// over-cap resume was refused), as opposed to a store fault. Lets a caller in
/// another module or crate (e.g. `resolve_gate`, or the gateway resume endpoint)
/// render it as backpressure (HTTP 503) rather than a fault without depending on
/// the private struct.
pub fn err_is_resume_at_capacity(err: &anyhow::Error) -> bool {
    err.is::<ResumeAtCapacity>()
}

impl SopEngine {
    /// Admit a run through the store CAS claim before it becomes locally active.
    /// The durable store is the concurrency source of truth; `active_runs` is the
    /// execution cache/status surface.
    pub(super) fn claim_admission(&self, run_id: &str, sop: &Sop) -> Result<ClaimToken> {
        match self.store.try_claim_run(
            run_id,
            &sop.name,
            sop.max_concurrent as usize,
            self.config.max_concurrent_total,
        ) {
            Ok(Some(token)) => Ok(token),
            Ok(None) => {
                bail!(
                    "Cannot start SOP '{}': cooldown or concurrency limit reached",
                    sop.name
                );
            }
            Err(e) => Err(anyhow::Error::new(e)),
        }
    }
    pub(super) fn release_claim_best_effort(&self, token: &ClaimToken) {
        if let Err(e) = self.store.release_claim(token) {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({
                        "run_id": token.run_id.as_str(),
                        "error": e.to_string(),
                    })),
                "SOP engine: failed to release run admission claim"
            );
        }
    }
    pub(super) fn claim_handle_for_run(run: &SopRun) -> ClaimToken {
        ClaimToken {
            run_id: run.run_id.clone(),
            sop_name: run.sop_name.clone(),
            claimed_at: String::new(),
            lease_expires: String::new(),
            holder: "engine".to_string(),
        }
    }
    pub(super) fn heartbeat_claim_for_run(&self, run: &SopRun) {
        let token = Self::claim_handle_for_run(run);
        if let Err(e) = self.store.heartbeat_claim(&token) {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({
                        "run_id": run.run_id.as_str(),
                        "error": e.to_string(),
                    })),
                "SOP engine: failed to heartbeat run admission claim"
            );
        }
    }
    pub(super) fn heartbeat_active_claims(&self) {
        // Only EXECUTING runs hold a claim; a parked run released its claim on park,
        // so heartbeating it would (on a durable store carrying a stale row from the
        // old behavior) extend a claim that should be gone. Skip parked runs. A run
        // in `claims_pending_persist` (a park whose snapshot failed to persist,
        // KEEPING its claim) is renewed by `retry_pending_park_persists` instead -
        // called just before this each tick - so its kept claim's lease never goes
        // un-renewed even while parked.
        for run in self.active_runs.values() {
            if holds_exec_claim(run.status) {
                self.heartbeat_claim_for_run(run);
            }
        }
        for run_id in &self.claims_retained_after_terminal_rollback {
            if let Some(run) = self.active_runs.get(run_id)
                && !holds_exec_claim(run.status)
            {
                self.heartbeat_claim_for_run(run);
            }
        }
    }
    /// A1: release a parked run's exec claim so its concurrency slot frees for
    /// other triggers. A run waiting on a human approval (or paused at a
    /// deterministic checkpoint) is not executing, so it must not hold an
    /// execution slot. The run stays in `active_runs` - every reader (gate_state,
    /// overdue_waiting_run_ids, resolve_gate, resume) and `finish_run` rely on it
    /// still being there; only the store CAS claim is dropped. Best-effort +
    /// logged. Persist the parked state BEFORE calling this so a crash in the
    /// window leaves a restorable parked run rather than a freed-but-unpersisted one.
    pub(crate) fn release_claim_on_park(&self, run_id: &str) {
        if let Some(run) = self.active_runs.get(run_id) {
            self.release_claim_best_effort(&Self::claim_handle_for_run(run));
        }
    }
    /// Checked counterpart to `release_claim_on_park`: release a parked run's exec
    /// claim and REPORT a store failure instead of swallowing it. Used on the
    /// checkpoint-denial CONTINUATION path, where the reacquired claim still carries
    /// the durable terminal-rollback retention marker. If that release is swallowed
    /// and fails, the marker survives on a run that actually CONTINUED (did not
    /// terminal-rollback), and `restore_runs` would then renew that stale claim
    /// forever, leaking the slot. Returning the error lets the caller fail closed
    /// (roll back + surface it) rather than report success with a live marker.
    /// `Ok(())` when there is no such active run (nothing to release).
    pub(super) fn release_claim_checked(
        &self,
        run_id: &str,
    ) -> Result<(), crate::sop::store::StoreError> {
        match self.active_runs.get(run_id) {
            Some(run) => self.store.release_claim(&Self::claim_handle_for_run(run)),
            None => Ok(()),
        }
    }
    /// Whether a durable terminal-rollback retention marker on a restored parked
    /// run is STALE. A legitimate marker guards a run whose TERMINAL write failed and
    /// left it restorable in its pre-terminal parked state — still awaiting the
    /// retried decision at its current checkpoint, which therefore has NO recorded
    /// result yet. If the current step ALREADY has a recorded `step_result`, the run
    /// reached this parked gate through a COMPLETED failure-route continuation (e.g. a
    /// denied checkpoint that `Retry`-re-parked at the same step), so the marker is
    /// stale and must be released rather than renewed forever.
    ///
    /// This is a HEURISTIC, not an exact classifier, and it errs on the SAFE side.
    /// It has two disclosed residuals, both bounded and benign:
    /// - It does NOT catch a denial that routed via `Goto` to a DIFFERENT, fresh
    ///   checkpoint (new current step, no result yet): that durable footprint is
    ///   indistinguishable from a legitimate terminal rollback at that fresh checkpoint,
    ///   so a stale marker there survives. The checked continuation release plus the
    ///   lease reaper cover that path in the non-crash case (see `deny_checkpoint`).
    /// - Symmetrically, it CAN flag a legitimate marker: a `Retry` checkpoint denied
    ///   enough times to re-park at the same step (leaving a `Failed` result there) and
    ///   then routed to a terminal `Fail` whose terminal write fails takes
    ///   `deny_checkpoint`'s retain-and-restore branch while carrying a result for its
    ///   current step; a restart before re-resolution would release that legitimate
    ///   marker. That direction is safe: the run is still restored into `active_runs`
    ///   (never lost) and only loses its HELD slot, degrading to standard parked
    ///   semantics — it re-acquires its exec slot on its next decision, capped
    ///   (subject to `max_concurrent`/`max_concurrent_total`) via
    ///   `reacquire_claim_on_resume` for an approval or checkpoint-approve resume, or
    ///   uncapped via `reacquire_claim_uncapped` for a subsequent denial. No double
    ///   execution, no permanent leak, no hard-cap violation.
    pub(super) fn terminal_rollback_marker_is_stale(run: &SopRun) -> bool {
        run.step_results
            .iter()
            .any(|result| result.step_number == run.current_step)
    }
    /// A1: re-establish a RESUMING run's exec claim, subject to the SOP's per-SOP
    /// `max_concurrent` AND the global `max_concurrent_total`. A run parked at a HITL
    /// approval / deterministic checkpoint released its exec slot on park; resuming
    /// it must re-admit through the SAME store CAS (`try_claim_run`) a fresh start
    /// uses, so a burst of simultaneous approvals can never push executing runs past
    /// the configured caps. (That burst is the reviewed defect: many runs park,
    /// releasing their slots, then all resume at once - the uncapped restore path
    /// let them oversubscribe.) Three outcomes:
    /// - `Ok(())`                 a slot was available; the run holds its claim and may resume.
    /// - `Err(ResumeAtCapacity)`  the cap is saturated. TYPED backpressure, NOT a fault:
    ///   the caller leaves the run parked and re-resolvable (`resolve_gate` reports
    ///   `DeferredAtCapacity`; the checkpoint paths surface it to the operator), and a
    ///   later approval attempt or the timeout tick's retry resumes it once a slot frees.
    /// - `Err(_)`                 a store fault (fail-closed, as before): abort the resume,
    ///   never execute uncounted.
    ///
    /// A missing run is a no-op `Ok` (the caller already validated it exists). The
    /// checkpoint-DENIAL path uses `reacquire_claim_uncapped` instead - a denial may
    /// TERMINATE the run, and gating a terminating run on a free slot would refuse to
    /// end it under load and strand it.
    pub(crate) fn reacquire_claim_on_resume(&self, run_id: &str) -> Result<()> {
        let Some((rid, sop_name)) = self
            .active_runs
            .get(run_id)
            .map(|run| (run.run_id.clone(), run.sop_name.clone()))
        else {
            return Ok(());
        };
        // Resolve the per-SOP cap exactly as the normal admit path does. The resume
        // pre-flights (`can_clear_waiting_gate` / `can_advance_deterministic_step`)
        // already proved the SOP is still loaded before we reach here; if it somehow
        // is not, fail closed rather than resume uncounted.
        let per_sop_cap = self
            .get_sop(&sop_name)
            .map(|sop| sop.max_concurrent as usize);
        let Some(per_sop_cap) = per_sop_cap else {
            return Err(anyhow::Error::msg(format!(
                "failed to re-acquire exec claim on resume for run {rid}: SOP '{sop_name}' no longer loaded"
            )));
        };
        match self.store.try_claim_run(
            &rid,
            &sop_name,
            per_sop_cap,
            self.config.max_concurrent_total,
        ) {
            Ok(Some(_token)) => Ok(()),
            Ok(None) => Err(anyhow::Error::new(ResumeAtCapacity {
                run_id: rid,
                sop_name,
            })),
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "run_id": rid.as_str(),
                            "error": e.to_string(),
                        })),
                    "SOP engine: resume aborted, could not re-acquire the run admission claim (fail-closed)"
                );
                Err(anyhow::Error::msg(format!(
                    "failed to re-acquire exec claim on resume for run {rid}: {e}"
                )))
            }
        }
    }
    /// UNCAPPED exec-claim re-establishment, for the checkpoint-DENIAL path only
    /// (`deny_checkpoint`). A denial may TERMINATE the run - it reacquires the claim
    /// to write terminal state and the terminal-rollback retention marker atomically,
    /// so this is rollback/atomicity machinery, not new admission, and must never be
    /// blocked by the concurrency cap (refusing to terminate a run under load would
    /// strand it, since it already released its slot at park). This is the ORIGINAL
    /// uncapped restore behavior; the capped `reacquire_claim_on_resume` above governs
    /// the three resume-to-continue paths (approval approve, checkpoint approve,
    /// deterministic resume). Fail-CLOSED on a store error, as before.
    pub(crate) fn reacquire_claim_uncapped(&self, run_id: &str) -> Result<()> {
        let Some(run) = self.active_runs.get(run_id) else {
            return Ok(());
        };
        self.store
            .renew_claim_for_restore(&run.run_id, &run.sop_name)
            .map(|_| ())
            .map_err(|e| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "run_id": run.run_id.as_str(),
                            "error": e.to_string(),
                        })),
                    "SOP engine: resume aborted, could not re-acquire the run admission claim (fail-closed)"
                );
                anyhow::Error::msg(format!(
                    "failed to re-acquire exec claim on resume for run {run_id}: {e}"
                ))
            })
    }
}

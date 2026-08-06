//! Admission policy, cooldown, and per-message dispatch dedup for [`SopEngine`].
use anyhow::{Result, bail};

use super::SopEngine;
use super::holds_exec_claim;
use crate::sop::time::cooldown_elapsed;
use crate::sop::types::{Sop, SopAdmission, SopAdmissionPolicy};

/// Cap on the in-memory per-message dispatch-dedup window (`SopEngine::dispatch_dedup`).
const DISPATCH_DEDUP_CAP: usize = 512;

/// Composite dedup key: `sop_name` and the transport delivery key joined by a NUL, which
/// cannot appear in a SOP name, so distinct pairs never collide.
fn dispatch_dedup_composite(sop_name: &str, dedup_key: &str) -> String {
    format!("{sop_name}\u{0}{dedup_key}")
}

impl SopEngine {
    /// Check whether a new run can be started for the given SOP
    /// (respects cooldown and concurrency limits).
    pub fn can_start(&self, sop_name: &str) -> bool {
        let sop = match self.get_sop(sop_name) {
            Some(s) => s,
            None => return false,
        };
        let (active_for_sop, active_total) = self.exec_counts(sop_name);
        if active_for_sop >= sop.max_concurrent as usize
            || active_total >= self.config.max_concurrent_total
        {
            return false;
        }
        !self.in_cooldown(sop)
    }

    /// Live *executing* run counts `(for_sop, total)`. The store's CAS claims are
    /// the authoritative concurrency source (shared across engine holders); parked
    /// runs release their claim (A1), so they are excluded. Falls back to the
    /// in-memory view (also parked-excluded) only if the store call errors.
    pub(crate) fn exec_counts(&self, sop_name: &str) -> (usize, usize) {
        match self.store.claim_counts(sop_name) {
            Ok(counts) => counts,
            Err(_) => (
                self.active_runs
                    .values()
                    .filter(|r| holds_exec_claim(r.status) && r.sop_name == sop_name)
                    .count(),
                self.active_runs
                    .values()
                    .filter(|r| holds_exec_claim(r.status))
                    .count(),
            ),
        }
    }

    /// Whether the SOP's cooldown window is still active (blocks a new start). Read
    /// from the shared store so every engine holder observes the same completion
    /// marker; falls back to the local finished list only on a store error.
    fn in_cooldown(&self, sop: &Sop) -> bool {
        if sop.cooldown_secs == 0 {
            return false;
        }
        let last_completed = match self.store.last_terminal_completed_at(&sop.name) {
            Ok(completed) => completed,
            Err(_) => self
                .last_finished_run(&sop.name)
                .and_then(|last| last.completed_at.clone()),
        };
        matches!(last_completed, Some(ts) if !cooldown_elapsed(&ts, sop.cooldown_secs))
    }

    /// Count runs of `sop_name` currently parked at a HITL approval / checkpoint
    /// (they hold no exec slot). This is the "pending-approval pool" A2 bounds.
    pub(super) fn pending_count_for_sop(&self, sop_name: &str) -> usize {
        // Read the shared store's active-run surface so multiple engine holders see
        // one source of truth for the pending-approval pool (mirrors exec_counts,
        // which reads store claim_counts). A persisted `WaitingApproval` run parked
        // by a sibling engine is counted here, so `max_pending_approvals` is not
        // silently exceeded across processes. Fall back to this engine's local view
        // only when the store errors.
        match self.store.load_active_runs() {
            Ok(runs) => runs
                .iter()
                .filter(|pr| pr.run.sop_name == sop_name && !holds_exec_claim(pr.run.status))
                .count(),
            Err(_) => self
                .active_runs
                .values()
                .filter(|r| r.sop_name == sop_name && !holds_exec_claim(r.status))
                .count(),
        }
    }

    /// First active (executing or parked) run id for `sop_name`, if any - the
    /// `Coalesce` policy names the in-flight run a new trigger folds into. Resolved
    /// from the SHARED store's active-run surface (like exec/pending counts), so an
    /// engine whose local map is empty still finds a sibling engine's in-flight run
    /// and returns `Coalesce` rather than `Defer` (which on a durable transport would
    /// churn redeliveries instead of acknowledging the trigger as absorbed). Falls
    /// back to the local map only on a store error.
    pub(super) fn first_active_run_for_sop(&self, sop_name: &str) -> Option<String> {
        match self.store.load_active_runs() {
            Ok(runs) => runs
                .into_iter()
                .find(|pr| pr.run.sop_name == sop_name)
                .map(|pr| pr.run.run_id),
            Err(_) => self
                .active_runs
                .values()
                .find(|r| r.sop_name == sop_name)
                .map(|r| r.run_id.clone()),
        }
    }

    /// A2: decide how to admit a matched trigger for `sop_name` under its
    /// `SopAdmissionPolicy`. `Admit` still passes through the authoritative CAS in
    /// `start_run`; the other outcomes are surfaced by the dispatch layer so a
    /// non-admitted trigger is never silently lost. A cooldown or unknown SOP drops
    /// regardless of policy (a cooldown is a deliberate rate limit, not backpressure).
    ///
    /// AUTHORITY: within a SINGLE daemon this decision is authoritative - the engine
    /// `Mutex` serializes `evaluate_admission` + the CAS claim, so two triggers cannot
    /// both admit past the policy. The exec-slot cap is additionally CAS-authoritative
    /// via the shared store even ACROSS engines. The pending-approval pool
    /// (`max_pending_approvals`), however, is only ADVISORY across engines: a run
    /// parks at approval only AFTER it has executed, so its pending slot cannot be
    /// atomically pre-reserved at admission time, and two engines sharing a store can
    /// each admit a run that later parks. Making the pending cap cross-engine-
    /// authoritative requires a store-level two-phase reservation (a follow-up); the
    /// single-daemon deployment - the common case - is fully authoritative today.
    pub fn evaluate_admission(&self, sop_name: &str) -> SopAdmission {
        let sop = match self.get_sop(sop_name) {
            Some(s) => s,
            None => {
                return SopAdmission::Drop {
                    reason: format!("SOP '{sop_name}' not loaded"),
                };
            }
        };
        if self.in_cooldown(sop) {
            return SopAdmission::Drop {
                reason: format!("SOP '{sop_name}' in cooldown"),
            };
        }

        let (exec_for_sop, exec_total) = self.exec_counts(sop_name);
        let pending_for_sop = self.pending_count_for_sop(sop_name);
        let exec_slot_free = exec_for_sop < sop.max_concurrent as usize
            && exec_total < self.config.max_concurrent_total;
        let policy = sop.admission_policy;

        // Pending-approval-pool backpressure (every policy but Drop, which drops).
        if sop.max_pending_approvals > 0 && pending_for_sop >= sop.max_pending_approvals as usize {
            let reason = format!("SOP '{sop_name}' pending-approval pool full ({pending_for_sop})");
            return match policy {
                SopAdmissionPolicy::Drop => SopAdmission::Drop { reason },
                _ => SopAdmission::Defer { reason },
            };
        }

        match policy {
            SopAdmissionPolicy::Parallel => {
                if exec_slot_free {
                    SopAdmission::Admit
                } else {
                    SopAdmission::Defer {
                        reason: format!("SOP '{sop_name}' execution slots full"),
                    }
                }
            }
            SopAdmissionPolicy::Hold => {
                if exec_for_sop + pending_for_sop == 0 && exec_slot_free {
                    SopAdmission::Admit
                } else {
                    SopAdmission::Defer {
                        reason: format!("SOP '{sop_name}' held (a run is already in flight)"),
                    }
                }
            }
            SopAdmissionPolicy::Coalesce => {
                if exec_for_sop + pending_for_sop == 0 && exec_slot_free {
                    SopAdmission::Admit
                } else if let Some(existing_run_id) = self.first_active_run_for_sop(sop_name) {
                    SopAdmission::Coalesce { existing_run_id }
                } else {
                    SopAdmission::Defer {
                        reason: format!("SOP '{sop_name}' execution slots full"),
                    }
                }
            }
            SopAdmissionPolicy::Drop => {
                if exec_slot_free {
                    SopAdmission::Admit
                } else {
                    SopAdmission::Drop {
                        reason: format!("SOP '{sop_name}' execution slots full (drop policy)"),
                    }
                }
            }
        }
    }

    /// A2 per-message idempotency: the run already started for `(sop_name, dedup_key)`, if
    /// one is in the bounded window AND the key is not ambiguous. Used by dispatch to
    /// coalesce a broker redelivery of the same message. Returns `None` for an AMBIGUOUS
    /// key (empty run - one a distinct fresh delivery reused): such a key must never
    /// coalesce, so its deliveries dispatch (a duplicate at worst, never a lost trigger).
    pub(crate) fn dispatch_dedup_lookup(&self, sop_name: &str, dedup_key: &str) -> Option<String> {
        let composite = dispatch_dedup_composite(sop_name, dedup_key);
        self.dispatch_dedup
            .iter()
            .find(|(k, _)| *k == composite)
            .and_then(|(_, run_id)| (!run_id.is_empty()).then(|| run_id.clone()))
    }

    /// A2: a FRESH (non-redelivery) delivery arrived for `(sop_name, dedup_key)`. If that
    /// key is ALREADY in the window a distinct delivery is REUSING a message-id (an AMQP
    /// contract violation); mark it AMBIGUOUS (empty run) so neither it nor a later
    /// redelivery ever coalesces - the safe direction is a duplicate run, never ACKing a
    /// distinct trigger away. Called BEFORE admission, so it also covers a reused-id
    /// delivery that then defers and is broker-redelivered.
    pub(crate) fn note_fresh_dispatch_key(&mut self, sop_name: &str, dedup_key: &str) {
        let composite = dispatch_dedup_composite(sop_name, dedup_key);
        if let Some(entry) = self
            .dispatch_dedup
            .iter_mut()
            .find(|(k, _)| *k == composite)
        {
            entry.1.clear();
        }
    }

    /// Record that a run started for `(sop_name, dedup_key)` so a later redelivery of the
    /// same message coalesces. A new key records its run; an existing key that maps to a
    /// DIFFERENT run (a reused message-id) is marked AMBIGUOUS (empty run - never
    /// coalesce). Bounded FIFO so the window self-trims.
    ///
    /// BEST-EFFORT and BOUNDED, by design: the window is in-memory and capped at
    /// `DISPATCH_DEDUP_CAP`. If a redelivery arrives after the process restarted or after
    /// more than the cap of other starts have pushed this key out, the dedup MISSES and
    /// the SOP may run again - this is the SAFE failure direction (an at-least-once
    /// duplicate, never a lost message). An eviction that drops a key whose run is still
    /// active is logged so the miss is observable rather than silent.
    pub(crate) fn record_dispatch_dedup(&mut self, sop_name: &str, dedup_key: &str, run_id: &str) {
        let composite = dispatch_dedup_composite(sop_name, dedup_key);
        if let Some(entry) = self
            .dispatch_dedup
            .iter_mut()
            .find(|(k, _)| *k == composite)
        {
            // Reused message-id (different run, or already ambiguous): mark ambiguous.
            if entry.1 != run_id {
                entry.1.clear();
            }
            return;
        }
        self.dispatch_dedup
            .push_back((composite, run_id.to_string()));
        while self.dispatch_dedup.len() > DISPATCH_DEDUP_CAP {
            if let Some((_, evicted_run)) = self.dispatch_dedup.pop_front()
                && self.active_runs.contains_key(&evicted_run)
            {
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({
                            "evicted_run_id": evicted_run,
                            "cap": DISPATCH_DEDUP_CAP,
                        })),
                    "SOP dispatch: per-message dedup window evicted a still-active run's \
                     key (window full); a later redelivery of that message may re-run it"
                );
            }
        }
    }

    /// Start a new SOP run. Returns the first action to take.
    /// Deterministic SOPs are automatically routed to `start_deterministic_run`.
    /// Enforce the SOP's admission policy at a start entrypoint. `Admit` proceeds;
    /// any other outcome declines the start with a descriptive error so a trigger is
    /// never run past its policy. dispatch pre-consults `evaluate_admission` and only
    /// reaches a start path on `Admit`, so re-checking here (under the same held lock)
    /// is idempotent; a DIRECT caller (`sop_execute`, or `start_deterministic_run`)
    /// would otherwise bypass Hold / Coalesce / the `max_pending_approvals` pool.
    pub(super) fn enforce_admission(&self, sop_name: &str) -> Result<()> {
        match self.evaluate_admission(sop_name) {
            SopAdmission::Admit => Ok(()),
            SopAdmission::Coalesce { existing_run_id } => bail!(
                "SOP '{sop_name}' not started: coalesced into in-flight run {existing_run_id}"
            ),
            SopAdmission::Defer { reason } | SopAdmission::Drop { reason } => {
                bail!("SOP '{sop_name}' not started: {reason}")
            }
        }
    }
}

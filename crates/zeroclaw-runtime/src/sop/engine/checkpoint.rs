//! SOP checkpoint decide/deny/revise extracted from engine/mod.rs.

use super::*;
use anyhow::Result;

impl super::SopEngine {
    pub(crate) fn resume_checkpoint(
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
    pub(crate) fn revisable_predecessor(&self, run_id: &str) -> Option<u32> {
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
        let ctx = crate::sop::capability::CapabilityContext {
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
    pub(crate) fn revise_checkpoint_with_principal(
        &mut self,
        run_id: &str,
        guidance: &str,
        decision: crate::sop::approval::ApprovalDecision,
        principal: crate::sop::approval::ApprovalPrincipal,
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

        let event = crate::sop::approval::GateLedgerEntry {
            run_id: run_id.to_string(),
            step: prior_run.current_step,
            gate_revision: Some(prior_run.revision),
            checkpoint_revision: Some(prior_run.revision),
            decision_identity: crate::sop::approval::broker::checkpoint_decision_identity(
                &decision,
            )
            .map(|(_, identity)| identity),
            kind: crate::sop::approval::GateEventKind::Resolved,
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
        decision: crate::sop::approval::ApprovalDecision,
    ) -> Result<SopRunAction> {
        match decision {
            crate::sop::approval::ApprovalDecision::Approve => self.approve_step(run_id),
            crate::sop::approval::ApprovalDecision::Deny { reason } => {
                self.deny_checkpoint(run_id, reason)
            }
            crate::sop::approval::ApprovalDecision::Amend { .. }
            | crate::sop::approval::ApprovalDecision::Revise { .. } => {
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
    pub(crate) fn decide_checkpoint_with_principal(
        &mut self,
        run_id: &str,
        decision: crate::sop::approval::ApprovalDecision,
        principal: crate::sop::approval::ApprovalPrincipal,
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

        if matches!(
            decision,
            crate::sop::approval::ApprovalDecision::Revise { .. }
        ) {
            bail!("checkpoint revise decisions use the revision persistence path")
        }
        if matches!(
            decision,
            crate::sop::approval::ApprovalDecision::Amend { .. }
        ) {
            self.can_amend_checkpoint(run_id)?;
        }

        let (_, sop) = self.resolve_active_run_sop(run_id)?;
        let current_step = self.resolve_sop_step(&sop, prior_run.current_step)?;
        let mut piped = step_input_value(&prior_run, current_step.number);
        if let crate::sop::approval::ApprovalDecision::Amend { text } = &decision {
            let field = self.checkpoint_edit_field(run_id)?;
            let Some(object) = piped.as_object_mut() else {
                bail!(
                    "Run {run_id} checkpoint input is not a JSON object; cannot amend field '{field}'"
                );
            };
            object.insert(field, serde_json::Value::String(text.clone()));
        }
        let (status, recorded_output, routed_output, started_at, completed_at) = match &decision {
            crate::sop::approval::ApprovalDecision::Approve
            | crate::sop::approval::ApprovalDecision::Amend { .. } => (
                SopStepStatus::Completed,
                piped.to_string(),
                piped,
                prior_run.started_at.clone(),
                Some(now_iso8601()),
            ),
            crate::sop::approval::ApprovalDecision::Deny { reason } => {
                if let crate::sop::step_contract::StepFailure::Goto { step } =
                    &current_step.on_failure
                {
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
            crate::sop::approval::ApprovalDecision::Revise { .. } => {
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
        let denial_terminates = matches!(
            decision,
            crate::sop::approval::ApprovalDecision::Deny { .. }
        ) && matches!(
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
        let event = crate::sop::approval::GateLedgerEntry {
            run_id: run_id.to_string(),
            step: current_step.number,
            gate_revision: Some(prior_run.revision),
            checkpoint_revision: Some(prior_run.revision),
            decision_identity: crate::sop::approval::broker::checkpoint_decision_identity(
                &decision,
            )
            .map(|(_, identity)| identity),
            kind: crate::sop::approval::GateEventKind::Resolved,
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
        if let crate::sop::step_contract::StepFailure::Goto { step } = &current_step.on_failure {
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
}

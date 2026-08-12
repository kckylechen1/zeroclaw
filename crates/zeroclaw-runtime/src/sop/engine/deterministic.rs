//! Deterministic SOP execution, capability/forge steps, and state-file persistence.
//!
//! Extracted from `engine/mod.rs` so the headless/deterministic path can evolve
//! without sitting next to the LLM advance / checkpoint-gate chokepoints.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde_json::Value;

use super::now_iso8601;
use super::{ParkPersistOutcome, SopEngine, retry_input_value, step_result_value};
use crate::sop::capability;
use crate::sop::route;
use crate::sop::rundata::RunData;
use crate::sop::store::{SopEventRecord, StoreError};
use crate::sop::types::{
    DeterministicRunState, Sop, SopEvent, SopExecutionMode, SopRun, SopRunAction, SopRunStatus,
    SopStep, SopStepKind, SopStepResult, SopStepStatus,
};

fn forge_comment_input_matches_checkpoint_output(
    input: &Value,
    checkpoint_result: &SopStepResult,
) -> bool {
    let Ok(target) = capability::resolve_forge_comment_target(input) else {
        return false;
    };
    let approved = step_result_value(checkpoint_result);
    let Some(approved) = approved.as_object() else {
        return false;
    };
    let approved_repo = approved
        .get("repo")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|repo| !repo.is_empty());
    let approved_number = approved.get("number").and_then(Value::as_u64);
    let approved_body = approved.get("body").and_then(Value::as_str);
    let approved_channel = approved
        .get("channel")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|channel| !channel.is_empty());
    let channel_matches = match target.channel {
        Some(channel) => approved_channel == Some(channel),
        None => true,
    };

    approved_repo == Some(target.repo)
        && approved_number == Some(target.number)
        && approved_body == Some(target.body)
        && channel_matches
}

impl SopEngine {
    pub(super) fn dispatch_deterministic_step(
        &mut self,
        run_id: &str,
        sop: &Sop,
        step_number: u32,
        input: Value,
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

        self.resolve_deterministic_action(sop, run_id, &step, input)
    }
    pub(super) fn finish_deterministic_run(&mut self, run_id: &str) -> Result<SopRunAction> {
        let saved = self
            .active_runs
            .get(run_id)
            .map(|run| run.llm_calls_saved)
            .unwrap_or(0);
        let action = self.finish_run(run_id, SopRunStatus::Completed, None)?;
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            &format!("Deterministic SOP run {run_id} completed ({saved} LLM calls saved)")
        );
        self.deterministic_savings.total_llm_calls_saved += saved;
        self.deterministic_savings.total_runs += 1;
        Ok(action)
    }
    /// Pre-flight ONLY the fallible SOP/step lookups that
    /// `advance_deterministic_step` performs for `run_id`'s current step, WITHOUT
    /// reacquiring a claim, mutating the run, or persisting anything.
    ///
    /// `approve_step` calls this BEFORE it reacquires the exec claim and flips the
    /// run to `Running`, so a checkpoint resume whose SOP was removed or shrunk
    /// while parked fails closed here - with the run left untouched at
    /// `PausedCheckpoint` - instead of after the mutation, which would otherwise
    /// strand the run in `Running`, holding a claim, with no way to make progress.
    pub(crate) fn can_advance_deterministic_step(&self, run_id: &str) -> Result<()> {
        let (_, sop) = self.resolve_active_run_sop(run_id)?;
        let current_step = self
            .active_runs
            .get(run_id)
            .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?
            .current_step;
        self.resolve_sop_step(&sop, current_step)?;
        Ok(())
    }
    // ── Deterministic execution ─────────────────────────────────

    /// Start a deterministic SOP run. Steps execute sequentially without LLM
    /// round-trips. Returns the first action (DeterministicStep or CheckpointWait).
    pub fn start_deterministic_run(
        &mut self,
        sop_name: &str,
        event: SopEvent,
    ) -> Result<SopRunAction> {
        // A2: this is a PUBLIC start entrypoint, so it must enforce the admission
        // policy itself - a direct caller must not be able to bypass Hold / Coalesce
        // / the pending-approval pool that `start_run` enforces. (When reached via
        // `start_run` the re-check is idempotent under the same held lock.)
        self.enforce_admission(sop_name)?;

        let sop = self.get_sop(sop_name).ok_or_else(|| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"sop_name": sop_name})),
                "SOP engine: sop not found"
            );
            anyhow::Error::msg(format!("SOP not found: {sop_name}"))
        })?;

        // Reject a non-deterministic SOP BEFORE reserving a slot, so a wrong-mode direct
        // call cannot claim (and then have to roll back) an execution slot.
        if sop.execution_mode != SopExecutionMode::Deterministic {
            bail!(
                "SOP '{}' is not in deterministic mode (mode: {})",
                sop_name,
                sop.execution_mode
            );
        }

        // Reserve + activate through the shared two-phase start path (identical run_id
        // prefix, logging, and dispatch to the pre-refactor inline body).
        let reservation = self.reserve_run_slot(sop_name)?;
        self.activate_reserved_run(reservation, event)
    }

    pub fn drive_headless_deterministic(
        &mut self,
        run_id: &str,
        first_action: SopRunAction,
    ) -> Result<SopRunAction> {
        let mut action = first_action;
        loop {
            match action {
                SopRunAction::DeterministicStep {
                    ref step,
                    ref input,
                    ..
                } if step.kind == SopStepKind::Capability => {
                    let (sop_name, sop) = self.resolve_active_run_sop(run_id)?;
                    action = self.execute_capability_step(&sop, run_id, step, input.clone())?;
                    if self.active_runs.contains_key(run_id) {
                        let run_sop_name = self
                            .active_runs
                            .get(run_id)
                            .map(|run| run.sop_name.as_str())
                            .unwrap_or(sop_name.as_str());
                        if run_sop_name != sop.name {
                            return Ok(action);
                        }
                    }
                }
                SopRunAction::DeterministicStep {
                    ref step,
                    ref run_id,
                    ..
                } => {
                    let sop_name = self
                        .active_runs
                        .get(run_id)
                        .map(|run| run.sop_name.clone())
                        .unwrap_or_default();
                    return self.fail_headless_driverless_step(run_id, &sop_name, step);
                }
                terminal => return Ok(terminal),
            }
        }
    }

    /// Advance a deterministic run with the output of the current step.
    /// The output is piped as input to the next step.
    pub fn advance_deterministic_step(
        &mut self,
        run_id: &str,
        step_output: serde_json::Value,
        step_timestamps: Option<(String, Option<String>)>,
    ) -> Result<SopRunAction> {
        let (_, sop) = self.resolve_active_run_sop(run_id)?;
        let current_step_number = self
            .active_runs
            .get(run_id)
            .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?
            .current_step;
        let current_step = self.resolve_sop_step(&sop, current_step_number)?;
        let (started_at, completed_at) = match step_timestamps {
            Some((started, completed)) => (started, completed),
            None => {
                let run = self
                    .active_runs
                    .get(run_id)
                    .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
                (run.started_at.clone(), Some(now_iso8601()))
            }
        };

        self.record_deterministic_step_result(
            run_id,
            &sop,
            &current_step,
            SopStepStatus::Completed,
            step_output.to_string(),
            step_output,
            started_at,
            completed_at,
        )
    }

    pub(super) fn forge_comment_authorized_by_prior_checkpoint(
        &self,
        sop: &Sop,
        run_id: &str,
        step_number: u32,
        input: &serde_json::Value,
    ) -> bool {
        let Some(run) = self.active_runs.get(run_id) else {
            return false;
        };
        let checkpoint_revision = run.revision;
        let Some(checkpoint_result) = run
            .step_results
            .iter()
            .rev()
            .find(|result| result.status == SopStepStatus::Completed)
        else {
            return false;
        };
        let checkpoint_step_number = checkpoint_result.step_number;
        if !sop.steps.iter().any(|step| {
            step.number == checkpoint_step_number && step.kind == SopStepKind::Checkpoint
        }) {
            return false;
        }
        if checkpoint_step_number >= step_number {
            return false;
        }
        if !forge_comment_input_matches_checkpoint_output(input, checkpoint_result) {
            return false;
        }

        self.run_events(run_id).is_ok_and(|events| {
            events.iter().any(|event| {
                event.kind.as_str() == "gate_resolved"
                    && event.payload.get("step").and_then(|value| value.as_u64())
                        == Some(u64::from(checkpoint_step_number))
                    && event
                        .payload
                        .get("checkpoint_revision")
                        .and_then(|value| value.as_u64())
                        == Some(u64::from(checkpoint_revision))
                    && event
                        .payload
                        .get("source")
                        .and_then(|value| value.as_str())
                        .is_some_and(|source| source != "agent" && source != "system")
                    && matches!(
                        event
                            .payload
                            .get("decision")
                            .and_then(|value| value.as_str()),
                        Some("approve") | Some("amend")
                    )
            })
        })
    }

    fn forge_comment_effect_payload(
        &self,
        sop: &Sop,
        step_number: u32,
        input: &Value,
    ) -> Result<Value> {
        let target = capability::resolve_forge_comment_target(input).map_err(anyhow::Error::msg)?;
        Ok(::serde_json::json!({
            "capability": "forge.comment",
            "sop_name": sop.name,
            "step": step_number,
            "channel": target.channel,
            "repo": target.repo,
            "number": target.number,
            "body": target.body,
        }))
    }

    fn forge_comment_success_output(&self, input: &Value) -> Result<Value> {
        let target = capability::resolve_forge_comment_target(input).map_err(anyhow::Error::msg)?;
        Ok(::serde_json::json!({
            "posted": true,
            "repo": target.repo,
            "number": target.number,
        }))
    }

    fn forge_comment_effect_state(
        &self,
        run_id: &str,
        effect_payload: &Value,
    ) -> Result<(bool, bool), StoreError> {
        let mut started = false;
        let mut completed = false;
        for event in self.store.list_events(run_id)? {
            if event.payload == *effect_payload {
                match event.kind.as_str() {
                    "capability_effect_started" => started = true,
                    "capability_effect_completed" => completed = true,
                    _ => {}
                }
            }
        }
        Ok((started, completed))
    }

    fn record_forge_comment_effect_marker(
        &self,
        run_id: &str,
        kind: &str,
        effect_payload: Value,
    ) -> Result<(), StoreError> {
        self.store
            .append_event(&SopEventRecord {
                run_id: run_id.to_string(),
                seq: 0,
                ts: now_iso8601(),
                kind: kind.to_string(),
                actor: None,
                reason: None,
                payload: effect_payload,
            })
            .map(|_| ())
    }

    fn record_forge_comment_failure(
        &mut self,
        run_id: &str,
        sop: &Sop,
        step: &SopStep,
        error: String,
        started_at: String,
    ) -> Result<SopRunAction> {
        self.metrics.record_capability_executed(&sop.name);
        let completed_at = Some(now_iso8601());
        self.record_deterministic_step_result(
            run_id,
            sop,
            step,
            SopStepStatus::Failed,
            error.clone(),
            serde_json::Value::String(error),
            started_at,
            completed_at,
        )
    }

    fn execute_forge_comment_step(
        &mut self,
        sop: &Sop,
        run_id: &str,
        step: &SopStep,
        input: Value,
        capability_input: Value,
        started_at: String,
    ) -> Result<SopRunAction> {
        if !self.forge_comment_authorized_by_prior_checkpoint(
            sop,
            run_id,
            step.number,
            &capability_input,
        ) {
            return self.record_forge_comment_failure(
                run_id,
                sop,
                step,
                "forge.comment requires the immediately preceding checkpoint to approve the exact repo, number, body, and channel"
                    .to_string(),
                started_at,
            );
        }

        let effect_payload =
            match self.forge_comment_effect_payload(sop, step.number, &capability_input) {
                Ok(payload) => payload,
                Err(e) => {
                    return self.record_forge_comment_failure(
                        run_id,
                        sop,
                        step,
                        format!("forge.comment: invalid target for effect ledger: {e}"),
                        started_at,
                    );
                }
            };
        let success_output = match self.forge_comment_success_output(&capability_input) {
            Ok(output) => output,
            Err(e) => {
                return self.record_forge_comment_failure(
                    run_id,
                    sop,
                    step,
                    format!("forge.comment: invalid target for success replay: {e}"),
                    started_at,
                );
            }
        };

        match self.forge_comment_effect_state(run_id, &effect_payload) {
            Ok((_started, true)) => {
                self.metrics.record_capability_executed(&sop.name);
                let completed_at = Some(now_iso8601());
                return self.record_deterministic_step_result(
                    run_id,
                    sop,
                    step,
                    SopStepStatus::Completed,
                    success_output.to_string(),
                    success_output,
                    started_at,
                    completed_at,
                );
            }
            Ok((true, false)) => {
                return self.record_forge_comment_failure(
                    run_id,
                    sop,
                    step,
                    "forge.comment has a prior unconfirmed public-send attempt for this run/step/target; refusing to replay automatically"
                        .to_string(),
                    started_at,
                );
            }
            Ok((false, false)) => {}
            Err(e) => {
                return self.record_forge_comment_failure(
                    run_id,
                    sop,
                    step,
                    format!(
                        "forge.comment cannot inspect durable effect ledger (fail-closed): {e}"
                    ),
                    started_at,
                );
            }
        }

        if let Err(e) = self.record_forge_comment_effect_marker(
            run_id,
            "capability_effect_started",
            effect_payload.clone(),
        ) {
            return self.record_forge_comment_failure(
                run_id,
                sop,
                step,
                format!(
                    "forge.comment cannot persist public-send attempt marker (fail-closed): {e}"
                ),
                started_at,
            );
        }

        let ctx = capability::CapabilityContext {
            run_id: run_id.to_string(),
            sop_name: sop.name.clone(),
            step_number: step.number,
            sop_location: sop.location.clone(),
        };
        let result = self.capabilities.execute_step(ctx, step, input);
        self.metrics.record_capability_executed(&sop.name);
        let completed_at = Some(now_iso8601());
        match result {
            Ok(result) if result.success => {
                if let Err(e) = self.record_forge_comment_effect_marker(
                    run_id,
                    "capability_effect_completed",
                    effect_payload,
                ) {
                    let error = format!(
                        "forge.comment posted but could not persist success marker (fail-closed; refusing replay): {e}"
                    );
                    return self.record_deterministic_step_result(
                        run_id,
                        sop,
                        step,
                        SopStepStatus::Failed,
                        error.clone(),
                        serde_json::Value::String(error),
                        started_at,
                        completed_at,
                    );
                }
                self.record_deterministic_step_result(
                    run_id,
                    sop,
                    step,
                    SopStepStatus::Completed,
                    result.output.to_string(),
                    result.output,
                    started_at,
                    completed_at,
                )
            }
            Ok(result) => {
                let error = result
                    .error
                    .unwrap_or_else(|| "capability returned failure".to_string());
                self.record_deterministic_step_result(
                    run_id,
                    sop,
                    step,
                    SopStepStatus::Failed,
                    error.clone(),
                    serde_json::Value::String(error),
                    started_at,
                    completed_at,
                )
            }
            Err(e) => {
                let error = e.to_string();
                self.record_deterministic_step_result(
                    run_id,
                    sop,
                    step,
                    SopStepStatus::Failed,
                    error.clone(),
                    serde_json::Value::String(error),
                    started_at,
                    completed_at,
                )
            }
        }
    }

    pub(super) fn execute_capability_step(
        &mut self,
        sop: &Sop,
        run_id: &str,
        step: &SopStep,
        input: serde_json::Value,
    ) -> Result<SopRunAction> {
        let started_at = now_iso8601();
        let capability_input = step.capability_call_input(input.clone());
        if step.capability_id() == Some("forge.comment") {
            return self.execute_forge_comment_step(
                sop,
                run_id,
                step,
                input,
                capability_input,
                started_at,
            );
        }

        let ctx = capability::CapabilityContext {
            run_id: run_id.to_string(),
            sop_name: sop.name.clone(),
            step_number: step.number,
            sop_location: sop.location.clone(),
        };
        let result = self.capabilities.execute_step(ctx, step, input);
        self.metrics.record_capability_executed(&sop.name);
        let completed_at = Some(now_iso8601());
        match result {
            Ok(result) if result.success => self.record_deterministic_step_result(
                run_id,
                sop,
                step,
                SopStepStatus::Completed,
                result.output.to_string(),
                result.output,
                started_at,
                completed_at,
            ),
            Ok(result) => {
                let error = result
                    .error
                    .unwrap_or_else(|| "capability returned failure".to_string());
                self.record_deterministic_step_result(
                    run_id,
                    sop,
                    step,
                    SopStepStatus::Failed,
                    error.clone(),
                    serde_json::Value::String(error),
                    started_at,
                    completed_at,
                )
            }
            Err(e) => {
                let error = e.to_string();
                self.record_deterministic_step_result(
                    run_id,
                    sop,
                    step,
                    SopStepStatus::Failed,
                    error.clone(),
                    serde_json::Value::String(error),
                    started_at,
                    completed_at,
                )
            }
        }
    }

    pub(super) fn record_deterministic_step_result(
        &mut self,
        run_id: &str,
        sop: &Sop,
        current_step: &SopStep,
        status: SopStepStatus,
        recorded_output: String,
        routed_output: serde_json::Value,
        started_at: String,
        completed_at: Option<String>,
    ) -> Result<SopRunAction> {
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
        let retry_input = retry_input_value(run, current_step.number);
        run.step_results.push(SopStepResult {
            step_number: run.current_step,
            status,
            output: recorded_output,
            started_at,
            completed_at,
            effective_agent: None,
            tool_calls: Vec::new(),
        });

        let mut last_status = status;
        if status == SopStepStatus::Completed {
            if let Err(reason) = self.validate_step_output(current_step, &routed_output) {
                last_status = SopStepStatus::Failed;
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

        self.route_recorded_step(
            run_id,
            sop,
            current_step,
            last_status,
            true,
            Some(retry_input),
            Some(routed_output),
        )
    }

    pub(super) fn resolve_active_run_sop(&self, run_id: &str) -> Result<(String, Sop)> {
        let sop_name = self
            .active_runs
            .get(run_id)
            .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?
            .sop_name
            .clone();
        let sop = self
            .sops
            .iter()
            .find(|s| s.name == sop_name)
            .cloned()
            .ok_or_else(|| anyhow::Error::msg(format!("SOP '{sop_name}' no longer loaded")))?;
        Ok((sop_name, sop))
    }

    pub(super) fn fail_headless_driverless_step(
        &mut self,
        run_id: &str,
        sop_name: &str,
        step: &SopStep,
    ) -> Result<SopRunAction> {
        let reason = format!(
            "Headless deterministic SOP step {} '{}' requires an external driver; it was not executed",
            step.number, step.title
        );
        let now = now_iso8601();
        if let Some(run) = self.active_runs.get_mut(run_id) {
            run.step_results.push(SopStepResult {
                step_number: step.number,
                status: SopStepStatus::Failed,
                output: reason.clone(),
                started_at: now.clone(),
                completed_at: Some(now),
                effective_agent: None,
                tool_calls: Vec::new(),
            });
        }
        self.record_transition_event(
            run_id,
            "headless_driver_missing",
            Some(reason.clone()),
            ::serde_json::json!({
                "sop_name": sop_name,
                "step": step.number,
                "kind": step.kind.to_string(),
            }),
        );
        self.finish_run(run_id, SopRunStatus::Failed, Some(reason))
    }

    /// Resume a deterministic run from persisted state.
    pub fn resume_deterministic_run(
        &mut self,
        state: DeterministicRunState,
    ) -> Result<SopRunAction> {
        // Validate the run exists and is paused (immutable read), capturing its SOP
        // name, before any mutation - so the fail-closed reacquire can run first.
        let sop_name = match self.active_runs.get(&state.run_id) {
            Some(run) if run.status == SopRunStatus::PausedCheckpoint => run.sop_name.clone(),
            Some(run) => {
                bail!(
                    "Run {} is not paused at checkpoint (status: {})",
                    state.run_id,
                    run.status
                );
            }
            None => {
                let run_id = state.run_id.clone();
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"run_id": run_id})),
                    "SOP engine: active run not found"
                );
                bail!("Active run not found: {}", state.run_id);
            }
        };

        // Refuse to resume while the checkpoint's parked snapshot has not yet
        // been durably persisted (see `is_park_persist_pending`'s doc): the kept
        // claim predates this attempt, and reacquiring on top of it would give a
        // later rollback or a maintenance retry no way to distinguish "freshly
        // reacquired" from "pre-existing, must survive."
        if self.is_park_persist_pending(&state.run_id) {
            bail!(
                "Run {} cannot resume: its parked checkpoint snapshot is not yet durably persisted (retrying)",
                state.run_id
            );
        }

        let sop = self
            .sops
            .iter()
            .find(|s| s.name == sop_name)
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"sop_name": sop_name.as_str()})),
                    "SOP engine: sop no longer loaded (definition removed mid-run)"
                );
                anyhow::Error::msg(format!("SOP '{sop_name}' no longer loaded"))
            })?
            .clone();

        // Pre-flight the step this resume will advance to BEFORE reacquiring the
        // claim or mutating the run: a definition shrunk while parked must fail
        // closed here, with the run left untouched at `PausedCheckpoint`
        // (re-resolvable), instead of after the mutation below - which would
        // otherwise strand the run in `Running`, holding a claim, with no way to
        // make progress.
        let resume_step = if state.last_completed_step == 0 {
            1
        } else {
            state.last_completed_step
        };
        self.resolve_sop_step(&sop, resume_step)?;

        // A1: fail-closed - a restored parked run holds no exec claim; re-acquire it
        // BEFORE the transition and abort (leaving the run paused) if it fails.
        self.reacquire_claim_on_resume(&state.run_id)?;

        let run = self
            .active_runs
            .get_mut(&state.run_id)
            .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {}", state.run_id)))?;
        let prior_waiting_since = run.waiting_since.clone();
        let prior_llm_calls_saved = run.llm_calls_saved;
        let prior_current_step = run.current_step;
        run.status = SopRunStatus::Running;
        run.waiting_since = None;
        run.llm_calls_saved = state.llm_calls_saved;
        for (step_number, output) in &state.step_outputs {
            let already_recorded = run
                .step_results
                .iter()
                .any(|result| result.step_number == *step_number);
            if !already_recorded {
                run.step_results.push(SopStepResult {
                    step_number: *step_number,
                    status: SopStepStatus::Completed,
                    output: output.to_string(),
                    started_at: state.persisted_at.clone(),
                    completed_at: Some(state.persisted_at.clone()),
                    effective_agent: None,
                    tool_calls: Vec::new(),
                });
            }
        }

        let last_output = state
            .step_outputs
            .get(&state.last_completed_step)
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let run_id = state.run_id.clone();

        let outcome = if state.last_completed_step == 0 {
            self.dispatch_deterministic_step(&run_id, &sop, 1, last_output)
        } else {
            {
                let run = self.active_runs.get_mut(&run_id).unwrap();
                run.current_step = state.last_completed_step;
            }
            self.resolve_sop_step(&sop, state.last_completed_step)
                .and_then(|current_step| {
                    self.route_recorded_step(
                        &run_id,
                        &sop,
                        &current_step,
                        SopStepStatus::Completed,
                        true,
                        None,
                        Some(last_output),
                    )
                })
        };

        match outcome {
            Ok(action) => Ok(action),
            Err(e) => {
                // Defensive: the pre-flight above validated the same step lookup
                // under this lock, so this is unreachable in practice. If it still
                // fails, roll the run back to `PausedCheckpoint` and release the
                // just-reacquired claim so it doesn't get stuck in `Running`
                // holding a leaked exec slot.
                if let Some(run) = self.active_runs.get_mut(&run_id) {
                    run.status = SopRunStatus::PausedCheckpoint;
                    run.waiting_since = prior_waiting_since;
                    run.llm_calls_saved = prior_llm_calls_saved;
                    run.current_step = prior_current_step;
                }
                self.release_claim_on_park(&run_id);
                Err(e)
            }
        }
    }

    /// Resolve the action for a deterministic step (execute or checkpoint).
    pub(super) fn resolve_deterministic_action(
        &mut self,
        sop: &Sop,
        run_id: &str,
        step: &SopStep,
        input: serde_json::Value,
    ) -> Result<SopRunAction> {
        let run_data = {
            let run = self
                .active_runs
                .get(run_id)
                .ok_or_else(|| anyhow::Error::msg(format!("Active run not found: {run_id}")))?;
            RunData::from_step_results(&run.step_results)
        };
        if !route::eligible(step, &run_data) {
            return Ok(self.mark_step_pending(
                run_id,
                sop,
                step.number,
                format!("step {} dependencies not satisfied", step.number),
            ));
        }

        if let Some(action) = self.schema_input_failure_action(run_id, step, &input)? {
            return Ok(action);
        }

        match step.kind {
            SopStepKind::Checkpoint => {
                if let Some(reason) = self.pending_pool_full_reason(sop) {
                    Self::log_pending_capacity_full(run_id, &reason);
                    return Ok(self.mark_step_pending(run_id, sop, step.number, reason));
                }

                // Persist the checkpoint state before flipping the run status. If
                // the state-file write fails, the run remains Running with its
                // execution claim still heartbeat-eligible.
                let state_file = self.persist_deterministic_state(run_id, sop, true)?;

                // A prior checkpoint's recorded result (it records on resolve)
                // means this run has presented a gate before.
                let has_prior_gate = self.active_runs.get(run_id).is_some_and(|run| {
                    run.step_results.iter().any(|r| {
                        sop.steps
                            .iter()
                            .any(|s| s.number == r.step_number && s.kind == SopStepKind::Checkpoint)
                    })
                });
                // Pause at checkpoint - persist state and wait for approval
                if let Some(run) = self.active_runs.get_mut(run_id) {
                    run.status = SopRunStatus::PausedCheckpoint;
                    run.waiting_since = Some(now_iso8601());
                    // A NEW gate presentation (not a revise re-park): after the
                    // run's first-ever park, bump the presentation counter so
                    // this gate's prompt reference can never collide with an
                    // earlier gate's leftover buttons, and rebase the per-gate
                    // revise budget (`revision - revision_base`).
                    if run.revision > 0 || has_prior_gate {
                        run.revision += 1;
                    }
                    run.revision_base = run.revision;
                }

                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    &format!(
                        "Deterministic SOP run {run_id}: checkpoint at step {} '{}', state persisted to {}",
                        step.number,
                        step.title,
                        state_file.display().to_string()
                    )
                );

                // Mirror the paused checkpoint into the shared run store (alongside
                // the deterministic state file) so a restart leaves a non-terminal
                // row for restore_runs() to rehydrate. A1: free the exec slot while
                // the run waits at the checkpoint - but only AFTER the parked
                // snapshot is durably persisted (else keep the claim).
                match self.persist_parked_snapshot_then_release_claim(run_id) {
                    // A policy-gated checkpoint is the same durable approval boundary
                    // as `WaitingApproval`: send its configured request notice only
                    // after the parked snapshot is recoverable. If this write failed,
                    // the maintenance retry owns the eventual single notification.
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

                Ok(SopRunAction::CheckpointWait {
                    run_id: run_id.to_string(),
                    step: step.clone(),
                    state_file,
                })
            }
            SopStepKind::Capability => self.execute_capability_step(sop, run_id, step, input),
            SopStepKind::Execute => {
                // Persist the active (Running) deterministic run so a restart mid-run
                // leaves a non-terminal row for restore_runs() to rehydrate. This is
                // the single sink for start / advance / resume deterministic steps.
                self.persist_active(run_id);

                Ok(SopRunAction::DeterministicStep {
                    run_id: run_id.to_string(),
                    step: step.clone(),
                    input,
                })
            }
        }
    }

    /// Persist the current deterministic run state to a JSON file.
    pub(super) fn persist_deterministic_state(
        &self,
        run_id: &str,
        sop: &Sop,
        paused_at_checkpoint: bool,
    ) -> Result<PathBuf> {
        let run = self.active_runs.get(run_id).ok_or_else(|| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"run_id": run_id})),
                "SOP engine: run not found in history"
            );
            anyhow::Error::msg(format!("Run not found: {run_id}"))
        })?;

        let mut step_outputs = HashMap::new();
        let mut last_completed_step = 0;
        for result in &run.step_results {
            if result.status == SopStepStatus::Completed {
                let value = step_result_value(result);
                step_outputs.insert(result.step_number, value);
                last_completed_step = result.step_number;
            }
        }

        let state = DeterministicRunState {
            run_id: run_id.to_string(),
            sop_name: run.sop_name.clone(),
            last_completed_step,
            total_steps: run.total_steps,
            step_outputs,
            persisted_at: now_iso8601(),
            llm_calls_saved: run.llm_calls_saved,
            paused_at_checkpoint,
        };

        // Write to SOP location directory, or system temp dir
        let temp_dir = std::env::temp_dir();
        let dir = sop.location.as_deref().unwrap_or(temp_dir.as_path());
        let state_file = dir.join(format!("{run_id}.state.json"));
        let json = serde_json::to_string_pretty(&state)?;
        std::fs::write(&state_file, json)?;

        Ok(state_file)
    }

    /// Best-effort removal of a run's park-snapshot file once the run is
    /// terminal. Mirrors `persist_deterministic_state`'s path resolution; a
    /// missing file (the run never parked) is not an error.
    pub(super) fn remove_deterministic_state_file(&self, run: &SopRun) {
        let temp_dir = std::env::temp_dir();
        let dir = self
            .get_sop(&run.sop_name)
            .and_then(|sop| sop.location.clone())
            .unwrap_or(temp_dir);
        let path = dir.join(format!("{}.state.json", run.run_id));
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "run_id": run.run_id,
                            "path": path.display().to_string(),
                            "error": e.to_string(),
                        })),
                    "SOP engine: terminal run's park snapshot could not be removed"
                );
            }
        }
    }

    /// Load a persisted deterministic run state from a JSON file.
    pub fn load_deterministic_state(path: &Path) -> Result<DeterministicRunState> {
        let content = std::fs::read_to_string(path)?;
        let state: DeterministicRunState = serde_json::from_str(&content)?;
        Ok(state)
    }
}

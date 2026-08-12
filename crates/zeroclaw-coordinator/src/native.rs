//! Native coordinator adapter for [`AgentDriver`].
//!
//! Wraps [`ChannelBackend`] — the coordinator's public command surface — rather
//! than `zeroclaw-runtime`'s `NativeChildRunner`. [`ChildRunner`] stays the
//! internal lifecycle seam; this adapter is the registry-facing translation
//! layer that turns a heterogeneous [`AgentRunRequest`] into a native
//! [`ChildRequest`] and bridges inspect/cancel onto the existing actor.
//!
//! Spawn is detached: it awaits admission only and returns a handle. That is
//! the `AgentDriver::spawn` contract. It does **not** replace
//! [`ChannelBackend::spawn`] or `spawn_subagent`; those callers stay on the
//! native `ChildRequest` path until a later PR switches them over.
//!
//! Resume stays on the trait default ([`DriverError::Unsupported`]). Native
//! continuation is expressed as a new spawn with [`AgentRunRequest::resume_from`],
//! matching how [`ChildRequest::resume_from`] already works.
//!
//! [`AgentDriver`]: crate::driver::AgentDriver
//! [`AgentRunRequest`]: crate::driver::AgentRunRequest
//! [`ChildRunner`]: crate::state::ChildRunner
//! [`ChildRequest`]: crate::types::ChildRequest
//! [`ChannelBackend`]: crate::backend::ChannelBackend
//! [`ChannelBackend::spawn`]: crate::backend::ChannelBackend::spawn
//! [`DriverError::Unsupported`]: crate::driver::DriverError::Unsupported

use crate::backend::{ChannelBackend, spawn_admission_timeout};
use crate::cancel::CancelToken;
use crate::driver::{
    AgentDriver, AgentRunHandle, AgentRunRequest, AgentRunSnapshot, AgentRunStatus, DriverError,
    HarnessCapabilities, HarnessCard, HarnessId, HarnessKind,
};
use crate::types::{
    CancelOutcome, ChildInspection, ChildRequest, ChildResult, ChildStatus, CoordinatorCommand,
    SpawnAdmission, SpawnCommand,
};

/// Registry key and [`AgentDriver::id`] for the in-process native harness.
pub const NATIVE_HARNESS_ID: &str = "native";

/// [`AgentDriver`] that runs children through the coordinator actor.
pub struct NativeAgentDriver {
    backend: ChannelBackend,
}

impl NativeAgentDriver {
    #[must_use]
    pub fn new(backend: ChannelBackend) -> Self {
        Self { backend }
    }

    /// Card advertised for the native harness. `resumable` is false because
    /// [`AgentDriver::resume`] stays unsupported; continuation uses
    /// [`AgentRunRequest::resume_from`] on a new spawn.
    #[must_use]
    pub fn card() -> HarnessCard {
        HarnessCard {
            id: HarnessId::from(NATIVE_HARNESS_ID),
            kind: HarnessKind::Native,
            name: "Native".into(),
            capabilities: HarnessCapabilities {
                streaming_tools: false,
                resumable: false,
                cancellable: true,
            },
        }
    }
}

/// Translate a heterogeneous spawn into the coordinator's native request.
///
/// Native-only fields get driver defaults: detached (`run_in_background`),
/// completion still surfaced, no fork, no parent-turn binding. `parent_session_id`
/// is copied from a session-bound backend when the caller supplies one.
/// [`AgentRunRequest::parent_alias`] is passed through unchanged — it is the
/// parent session's owning agent, not a native lifecycle knob.
pub(crate) fn child_request_from(
    request: AgentRunRequest,
    parent_session_id: Option<&str>,
) -> ChildRequest {
    ChildRequest {
        child_id: request.run_id,
        prompt: request.prompt.clone(),
        description: request.prompt,
        agent_type: request.agent,
        parent_session_id: parent_session_id.unwrap_or("").to_owned(),
        parent_alias: request.parent_alias,
        parent_prompt_id: None,
        resume_from: request.resume_from,
        cwd: request.cwd.map(|path| path.to_string_lossy().into_owned()),
        overrides: Default::default(),
        run_in_background: true,
        surface_completion: true,
        await_to_completion: false,
        fork_context: false,
        cancel_token: CancelToken::new(),
    }
}

/// Map coordinator lifecycle onto the driver-boundary status vocabulary.
///
/// `Initializing` is the coordinator's "spawned, not yet executing" state, so
/// it becomes [`AgentRunStatus::Pending`]. Terminal endings keep the wrapped
/// [`crate::outcome::ChildOutcome`] — they are not collapsed.
pub(crate) fn agent_run_status(status: &ChildStatus) -> AgentRunStatus {
    match status {
        ChildStatus::Initializing => AgentRunStatus::Pending,
        ChildStatus::Running { .. } => AgentRunStatus::Running,
        ChildStatus::Finished { outcome, .. } => AgentRunStatus::Finished(*outcome),
    }
}

/// Rebuild a driver snapshot from an inspect reply.
///
/// Terminal usage comes from [`ChildStatus::Finished`], which
/// `completed_snapshot` copies out of [`crate::state::CompletedChild::result`].
/// The spawn `result_tx` receiver stays dropped: keeping it would duplicate
/// the completed map, fight the detached-spawn contract, and leave
/// `ChannelBackend::inspect` / query equally blind. ChildRunner is unchanged.
fn snapshot_from_inspection(inspection: ChildInspection) -> AgentRunSnapshot {
    let status = agent_run_status(&inspection.snapshot.status);
    let result = match &inspection.snapshot.status {
        ChildStatus::Finished {
            outcome,
            output,
            detail,
            tool_calls,
            turns,
            tokens_used,
            output_tokens_used,
            total_tokens_used,
            worktree_path,
        } => Some(ChildResult {
            outcome: *outcome,
            output: output.as_str().into(),
            detail: detail.clone(),
            child_id: inspection.snapshot.child_id.clone(),
            child_session_id: inspection.child_session_id.clone(),
            tool_calls: *tool_calls,
            turns: *turns,
            duration_ms: inspection.snapshot.duration_ms,
            tokens_used: *tokens_used,
            output_tokens_used: *output_tokens_used,
            total_tokens_used: *total_tokens_used,
            worktree_path: worktree_path.clone(),
            backgrounded: false,
        }),
        _ => None,
    };
    AgentRunSnapshot { status, result }
}

#[async_trait::async_trait]
impl AgentDriver for NativeAgentDriver {
    fn id(&self) -> &str {
        NATIVE_HARNESS_ID
    }

    fn kind(&self) -> HarnessKind {
        HarnessKind::Native
    }

    async fn spawn(&self, request: AgentRunRequest) -> Result<AgentRunHandle, DriverError> {
        let run_id = request.run_id.clone();
        let child = child_request_from(request, self.backend.bound_session());
        let (admission_tx, admission_rx) = tokio::sync::oneshot::channel();
        let (result_tx, _result_rx) = tokio::sync::oneshot::channel();
        // `_result_rx` is dropped on purpose: awaiting it would block until
        // the child ends, which is `ChannelBackend::spawn`'s contract, not
        // this one. `handle_only` is set from `run_in_background`, so the
        // coordinator does not treat the dropped receiver as an abandoned
        // foreground caller. See `spawn_subagent`'s detached path.
        self.backend
            .sender()
            .send(CoordinatorCommand::Spawn(SpawnCommand {
                request: Box::new(child),
                admission_tx,
                result_tx,
            }))
            .map_err(|_| DriverError::Internal("coordinator channel closed".into()))?;

        match tokio::time::timeout(spawn_admission_timeout(), admission_rx).await {
            Ok(Ok(SpawnAdmission::Admitted)) => Ok(AgentRunHandle {
                run_id,
                session_ref: None,
            }),
            Ok(Ok(SpawnAdmission::Refused(refusal))) => {
                Err(DriverError::Internal(format!("spawn refused: {refusal}")))
            }
            Ok(Err(_)) => Err(DriverError::Internal(
                "spawn admission channel dropped — the coordinator never decided".into(),
            )),
            Err(_) => Err(DriverError::Internal(format!(
                "spawn admission timed out after {:?}",
                spawn_admission_timeout()
            ))),
        }
    }

    async fn inspect(&self, handle: &AgentRunHandle) -> Result<AgentRunSnapshot, DriverError> {
        match self.backend.inspect(&handle.run_id).await {
            Some(inspection) => Ok(snapshot_from_inspection(inspection)),
            None => Err(DriverError::NotFound(handle.run_id.clone())),
        }
    }

    async fn cancel(&self, handle: &AgentRunHandle) -> Result<(), DriverError> {
        match self.backend.cancel(&handle.run_id).await {
            CancelOutcome::Cancelled | CancelOutcome::AlreadyFinished { .. } => Ok(()),
            CancelOutcome::NotFound => Err(DriverError::NotFound(handle.run_id.clone())),
        }
    }
}

#[cfg(test)]
#[path = "native_tests.rs"]
mod tests;

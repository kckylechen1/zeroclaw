// Derived from grok-build (Apache-2.0), revision
// 1adcd1f477870e4a97bacbd6be78c8a3bfbac46d, from
// `.../grok_build/task/backend.rs`.
// Copyright 2023-2026 SpaceXAI. Licensed under the Apache License, Version 2.0.
//
// This file was CHANGED by ZeroClaw Labs: the `SubagentBackend` trait and its
// `Arc<dyn ...>` resource wrapper were dropped (they existed so upstream's
// `Resources` bag could inject a transport; this crate is injected by
// constructor), leaving the concrete channel backend with inherent methods;
// `ToolError` was replaced by this crate's own `CoordinatorError`; the
// workflow-owner drop-cancel guard went with the workflow owner; `tracing`
// calls were dropped; the timeout env var was renamed to ZeroClaw's namespace.
// See ../LICENSE and ../NOTICE.

//! Client side of the coordinator's command channel.
//!
//! Every caller — a tool frontend, a session teardown path, a status view —
//! talks to the actor through this. It owns the reply channels so that no
//! caller has to; a caller that forgets a oneshot is a caller that hangs.

use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use crate::types::{
    CancelCommand, CancelOutcome, CancelTarget, ChildInspection, ChildRequest, ChildResult,
    ChildSnapshot, CoordinatorCommand, DescribeOutcome, DescribeTypeCommand, InspectCommand,
    ListRunningCommand, QueryCommand, RegistryCounts, RegistryCountsCommand, SpawnCommand,
    SpawnedChildRef, SpawnedRefsCommand, ValidateTypeCommand, ValidateTypeOutcome,
};

/// Why a spawn produced no result at all.
///
/// Distinct from a child that ran and failed: this is the coordinator itself
/// being unreachable or losing the reply, which no child outcome can express.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoordinatorError {
    /// The coordinator's command channel closed — the actor is gone.
    ChannelClosed,
    /// The command was accepted but the reply channel was dropped before an
    /// answer arrived.
    ResultChannelDropped,
}

impl std::fmt::Display for CoordinatorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ChannelClosed => {
                f.write_str("coordinator channel closed — cannot spawn child")
            }
            Self::ResultChannelDropped => {
                f.write_str("child result channel dropped — the child may have crashed")
            }
        }
    }
}

impl std::error::Error for CoordinatorError {}

/// In-process backend that carries commands to the coordinator actor.
///
/// Cloning is cheap and every clone talks to the same actor.
#[derive(Clone)]
pub struct ChannelBackend {
    tx: mpsc::UnboundedSender<CoordinatorCommand>,
    parent_session_id: Option<Arc<str>>,
}

impl std::fmt::Debug for ChannelBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelBackend")
            .field("parent_session_id", &self.parent_session_id)
            .finish_non_exhaustive()
    }
}

impl ChannelBackend {
    #[must_use]
    pub fn new(tx: mpsc::UnboundedSender<CoordinatorCommand>) -> Self {
        Self {
            tx,
            parent_session_id: None,
        }
    }

    /// Bind every operation to one parent session.
    ///
    /// This is the containment boundary: a bound backend cannot see, wait on,
    /// or cancel another session's children, whatever id it is handed.
    #[must_use]
    pub fn for_session(
        tx: mpsc::UnboundedSender<CoordinatorCommand>,
        parent_session_id: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            tx,
            parent_session_id: Some(parent_session_id.into()),
        }
    }

    fn parent_session_id(&self) -> Option<String> {
        self.parent_session_id.as_deref().map(str::to_owned)
    }

    #[must_use]
    pub fn sender(&self) -> mpsc::UnboundedSender<CoordinatorCommand> {
        self.tx.clone()
    }

    /// Spawn a child and await its result.
    ///
    /// A blocking caller awaits this directly; a background caller spawns a
    /// task around it and drops the receiver, which the coordinator reads as
    /// "nobody is waiting inline".
    ///
    /// # Errors
    ///
    /// [`CoordinatorError`] when the actor is unreachable or the reply was
    /// lost. A child that merely failed returns `Ok` with a failed outcome.
    pub async fn spawn(&self, mut request: ChildRequest) -> Result<ChildResult, CoordinatorError> {
        if let Some(parent_session_id) = self.parent_session_id.as_deref() {
            request.parent_session_id = parent_session_id.to_owned();
        }
        let (respond_to, response_rx) = oneshot::channel();
        self.tx
            .send(CoordinatorCommand::Spawn(SpawnCommand {
                request: Box::new(request),
                result_tx: respond_to,
            }))
            .map_err(|_| CoordinatorError::ChannelClosed)?;
        response_rx
            .await
            .map_err(|_| CoordinatorError::ResultChannelDropped)
    }

    /// Look up a child's state, optionally waiting for it to finish.
    pub async fn query(
        &self,
        id: &str,
        block: bool,
        timeout_ms: Option<u64>,
    ) -> Option<ChildSnapshot> {
        let (respond_to, response_rx) = oneshot::channel();
        let sent = self.tx.send(CoordinatorCommand::Query(QueryCommand {
            child_id: id.to_owned(),
            parent_session_id: self.parent_session_id(),
            block,
            timeout_ms,
            respond_to,
        }));
        if sent.is_err() {
            return None;
        }
        response_rx.await.ok().flatten()
    }

    /// Ask for one child to stop.
    pub async fn cancel(&self, id: &str) -> CancelOutcome {
        let (respond_to, response_rx) = oneshot::channel();
        let sent = self.tx.send(CoordinatorCommand::Cancel(CancelCommand {
            parent_session_id: self.parent_session_id(),
            target: CancelTarget::ChildId(id.to_owned()),
            respond_to,
        }));
        if sent.is_err() {
            return CancelOutcome::NotFound;
        }
        response_rx.await.unwrap_or(CancelOutcome::NotFound)
    }

    /// Cancel every child spawned by one parent turn.
    pub async fn cancel_parent_prompt(&self, parent_prompt_id: &str) -> CancelOutcome {
        let (respond_to, response_rx) = oneshot::channel();
        if self
            .tx
            .send(CoordinatorCommand::Cancel(CancelCommand {
                parent_session_id: self.parent_session_id(),
                target: CancelTarget::ParentPromptId(parent_prompt_id.to_owned()),
                respond_to,
            }))
            .is_err()
        {
            return CancelOutcome::NotFound;
        }
        response_rx.await.unwrap_or(CancelOutcome::NotFound)
    }

    pub async fn inspect(&self, id: &str) -> Option<ChildInspection> {
        let (respond_to, response_rx) = oneshot::channel();
        self.tx
            .send(CoordinatorCommand::Inspect(InspectCommand {
                child_id: id.to_owned(),
                parent_session_id: self.parent_session_id(),
                respond_to,
            }))
            .ok()?;
        response_rx.await.ok().flatten()
    }

    pub async fn list_running(&self, parent_session_id: &str) -> Vec<ChildInspection> {
        let (respond_to, response_rx) = oneshot::channel();
        if self
            .tx
            .send(CoordinatorCommand::ListRunning(ListRunningCommand {
                parent_session_id: parent_session_id.to_owned(),
                respond_to,
            }))
            .is_err()
        {
            return Vec::new();
        }
        response_rx.await.unwrap_or_default()
    }

    pub async fn spawned_refs_for_prompt(
        &self,
        parent_session_id: &str,
        prompt_id: &str,
    ) -> Vec<SpawnedChildRef> {
        let (respond_to, response_rx) = oneshot::channel();
        if self
            .tx
            .send(CoordinatorCommand::SpawnedRefs(SpawnedRefsCommand {
                parent_session_id: self
                    .parent_session_id
                    .as_deref()
                    .unwrap_or(parent_session_id)
                    .to_owned(),
                prompt_id: prompt_id.to_owned(),
                respond_to,
            }))
            .is_err()
        {
            return Vec::new();
        }
        response_rx.await.unwrap_or_default()
    }

    pub async fn registry_counts(&self) -> RegistryCounts {
        let (respond_to, response_rx) = oneshot::channel();
        if self
            .tx
            .send(CoordinatorCommand::RegistryCounts(RegistryCountsCommand {
                respond_to,
            }))
            .is_err()
        {
            return RegistryCounts::default();
        }
        response_rx.await.unwrap_or_default()
    }

    /// Check an agent type before spawning.
    ///
    /// Every failure to *reach* the coordinator answers
    /// [`ValidateTypeOutcome::ValidationUnavailable`], never `Unknown`: a
    /// caller must not be told a type does not exist because a channel was
    /// busy.
    pub async fn validate_type(
        &self,
        agent_type: &str,
        parent_session_id: &str,
    ) -> ValidateTypeOutcome {
        let parent_session_id = self
            .parent_session_id
            .as_deref()
            .unwrap_or(parent_session_id);
        let (respond_to, response_rx) = oneshot::channel();
        if self
            .tx
            .send(CoordinatorCommand::ValidateType(ValidateTypeCommand {
                agent_type: agent_type.to_owned(),
                parent_session_id: parent_session_id.to_owned(),
                respond_to,
            }))
            .is_err()
        {
            return ValidateTypeOutcome::ValidationUnavailable;
        }
        match tokio::time::timeout(validate_type_timeout(), response_rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) | Err(_) => ValidateTypeOutcome::ValidationUnavailable,
        }
    }

    /// Describe what a child of this agent type could do, without spawning it.
    ///
    /// Modelled exactly on [`Self::validate_type`]: unreachable is
    /// [`DescribeOutcome::Unavailable`], which callers treat as fail-open.
    pub async fn describe_agent_type(
        &self,
        agent_type: &str,
        harness_agent_type: Option<&str>,
        parent_session_id: &str,
    ) -> DescribeOutcome {
        let parent_session_id = self
            .parent_session_id
            .as_deref()
            .unwrap_or(parent_session_id);
        let (respond_to, response_rx) = oneshot::channel();
        if self
            .tx
            .send(CoordinatorCommand::DescribeType(DescribeTypeCommand {
                agent_type: agent_type.to_owned(),
                harness_agent_type: harness_agent_type.map(str::to_owned),
                parent_session_id: parent_session_id.to_owned(),
                respond_to,
            }))
            .is_err()
        {
            return DescribeOutcome::Unavailable;
        }
        match tokio::time::timeout(validate_type_timeout(), response_rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) | Err(_) => DescribeOutcome::Unavailable,
        }
    }
}

/// Default `validate_type` / `describe_agent_type` timeout.
pub const VALIDATE_TYPE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Env-var override for [`VALIDATE_TYPE_TIMEOUT`] (positive milliseconds).
pub const VALIDATE_TYPE_TIMEOUT_ENV_VAR: &str = "ZEROCLAW_VALIDATE_TYPE_TIMEOUT_MS";

/// Validation timeout, honouring the env-var override.
#[must_use]
pub fn validate_type_timeout() -> std::time::Duration {
    env_duration_or(VALIDATE_TYPE_TIMEOUT_ENV_VAR, VALIDATE_TYPE_TIMEOUT)
}

/// Parse a positive `u64` millisecond value; `None` for unset, invalid, or zero.
pub(crate) fn parse_timeout_ms(value: Option<&str>) -> Option<u64> {
    value?.parse::<u64>().ok().filter(|&ms| ms > 0)
}

/// Resolve a `Duration` from a positive-millisecond env override, falling back
/// to `default` when the var is unset, non-numeric, or zero.
#[must_use]
pub fn env_duration_or(env_var: &str, default: std::time::Duration) -> std::time::Duration {
    parse_timeout_ms(std::env::var(env_var).ok().as_deref())
        .map(std::time::Duration::from_millis)
        .unwrap_or(default)
}

#[cfg(test)]
#[path = "backend_tests.rs"]
mod tests;

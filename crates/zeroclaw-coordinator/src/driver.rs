//! Heterogeneous agent driver boundary.
//!
//! [`ChildRunner`] remains the coordinator's internal lifecycle seam — it is
//! generic over associated types and drives one in-process run. This module
//! is the registry-facing surface: an object-safe [`AgentDriver`] trait that
//! [`crate::DriverRegistry`] (`HashMap<HarnessId, Box<dyn AgentDriver>>`)
//! dispatches on, plus the [`HarnessCard`] config entity that names which
//! backend owns a given agent. [`HarnessKind`] is a classification only —
//! multiple drivers (e.g. claude-code and codex) may share
//! [`HarnessKind::Process`] and must not collide as registry keys.
//!
//! [`crate::NativeAgentDriver`] adapts the coordinator's [`ChannelBackend`]
//! (translating [`AgentRunRequest`] into the native [`ChildRequest`]). Live
//! spawn callers (`ChannelBackend::spawn`, `spawn_subagent`) are not switched
//! over yet; a later PR does that, then `JoinMode`, then delegate convergence.
//!
//! [`ChildRunner`]: crate::state::ChildRunner
//! [`ChildRequest`]: crate::types::ChildRequest
//! [`ChannelBackend`]: crate::backend::ChannelBackend

use std::fmt;
use std::path::PathBuf;

use crate::outcome::ChildOutcome;
use crate::types::ChildResult;

// ── HarnessCard ──────────────────────────────────────────────────────────

/// Stable registry key for one harness backend (e.g. `"native"`,
/// `"claude-code"`, `"codex"`). Distinct from the operator-visible
/// [`HarnessCard::name`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct HarnessId(pub String);

impl HarnessId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for HarnessId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for HarnessId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl fmt::Display for HarnessId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// How an agent is executed and controlled. One per harness backend.
///
/// V1 is a plain struct carried inside the coordinator crate. A later PR
/// moves this to `zeroclaw-config` as a `[harness.<alias>]` TOML surface once
/// the driver registry exists and the fields stabilize.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HarnessCard {
    /// Stable id used as the driver-registry key. Must be unique across
    /// drivers that share the same [`HarnessKind`].
    pub id: HarnessId,
    /// Which driver backend category this harness belongs to.
    pub kind: HarnessKind,
    /// Operator-visible display name (e.g. "Claude Code", "Codex").
    pub name: String,
    /// Capabilities the driver declares it can satisfy.
    pub capabilities: HarnessCapabilities,
}

/// Broad category of execution backend. Classification only — not a registry
/// key (see [`HarnessId`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessKind {
    /// ZeroClaw-native in-process agent loop.
    Native,
    /// Out-of-process CLI invoked and observed by ZeroClaw.
    Process,
    /// Language SDK linked into the ZeroClaw process.
    Sdk,
    /// Remote endpoint reached over network (A2A, RPC, etc.).
    Remote,
}

/// Feature flags a driver advertises. Callers consult these before relying on
/// a capability the backend may not have.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HarnessCapabilities {
    /// Driver emits streaming tool events.
    pub streaming_tools: bool,
    /// Driver supports resume after process restart.
    pub resumable: bool,
    /// Driver can be cancelled mid-turn.
    pub cancellable: bool,
}

// ── Handle / snapshot / request types ────────────────────────────────────

/// Opaque handle to a live or completed run, returned by [`AgentDriver::spawn`]
/// and [`AgentDriver::resume`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRunHandle {
    /// Stable id — matches [`AgentRunRequest::run_id`].
    pub run_id: String,
    /// Driver-specific session reference (e.g. external CLI session id).
    pub session_ref: Option<String>,
}

/// Point-in-time snapshot of a run, returned by [`AgentDriver::inspect`].
///
/// Invariant: [`result`](Self::result) is `Some` only when
/// [`status`](Self::status) is [`AgentRunStatus::Finished`]. Non-terminal
/// statuses carry `None`.
#[derive(Debug, Clone)]
pub struct AgentRunSnapshot {
    pub status: AgentRunStatus,
    /// Present once the run reaches [`AgentRunStatus::Finished`].
    pub result: Option<ChildResult>,
}

/// Coarse lifecycle status a driver can report.
///
/// Non-terminal states are local to the driver boundary. Terminal endings reuse
/// the coordinator's [`ChildOutcome`] vocabulary via
/// [`Finished`](Self::Finished) — the same five endings as native children
/// (`Completed` / `Failed` / `Cancelled` / `TimedOut` / `Lost`), so drivers
/// cannot invent a parallel three-state terminal set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRunStatus {
    /// Spawned but not yet executing.
    Pending,
    /// Actively running.
    Running,
    /// Ended. The wrapped [`ChildOutcome`] says how.
    Finished(ChildOutcome),
}

/// Heterogeneous spawn input — deliberately thinner than the coordinator's
/// native [`ChildRequest`].
///
/// Native lifecycle fields (`CancelToken`, `fork_context`, `surface_completion`,
/// join-mode knobs, parent-session wiring, …) stay on `ChildRequest`. A
/// `NativeAgentDriver` adapter is responsible for translating this request into
/// a `ChildRequest` when the registry routes a run to the native harness.
///
/// [`ChildRequest`]: crate::types::ChildRequest
#[derive(Debug, Clone)]
pub struct AgentRunRequest {
    /// Stable id for the run, chosen by the caller.
    pub run_id: String,
    /// Prompt / task text handed to the harness.
    pub prompt: String,
    /// Agent type or alias the harness should run as.
    pub agent: String,
    /// Agent alias that owns the parent session — same contract as
    /// [`ChildRequest::parent_alias`].
    ///
    /// This is what belongs in a persisted `TaskRecord.agent` and, downstream
    /// of that, in `Announcement.agent`. It is **not** a role/agent-type
    /// spelling like `"explore"`, and it is a different axis than
    /// `parent_session_id` (session identity). Empty means the caller has no
    /// parent alias to attribute — the adapter must not invent one.
    ///
    /// [`ChildRequest::parent_alias`]: crate::types::ChildRequest::parent_alias
    pub parent_alias: String,
    /// Explicit working directory for the harness, when the caller sets one.
    pub cwd: Option<PathBuf>,
    /// Resume from a previously completed run or external session id.
    pub resume_from: Option<String>,
}

/// Resume input: the run id to continue plus a new prompt.
#[derive(Debug, Clone)]
pub struct ResumeRequest {
    pub run_id: String,
    pub prompt: String,
}

// ── Error ────────────────────────────────────────────────────────────────

/// Driver-level error. Distinct from [`ChildOutcome`] (the run's result): a
/// `DriverError` means the driver could not perform the requested operation,
/// not that the run itself failed.
///
/// [`ChildOutcome`]: crate::outcome::ChildOutcome
#[derive(Debug)]
pub enum DriverError {
    /// The run id is unknown to this driver.
    NotFound(String),
    /// The driver backend does not implement this operation.
    Unsupported(String),
    /// The driver failed internally (spawn failure, I/O, protocol, etc.).
    Internal(String),
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "run not found: {id}"),
            Self::Unsupported(op) => write!(f, "driver does not support this operation: {op}"),
            Self::Internal(msg) => write!(f, "driver internal error: {msg}"),
        }
    }
}

impl std::error::Error for DriverError {}

// ── Trait ────────────────────────────────────────────────────────────────

/// Object-safe driver boundary for heterogeneous agent harnesses.
///
/// One implementation per harness backend. Drivers register under a stable
/// [`id`](Self::id) (see [`HarnessId`]); [`kind`](Self::kind) is only a
/// classification. The coordinator's [`ChildRunner`] stays the internal
/// lifecycle seam; this trait is the registry surface that picks which driver
/// handles a given [`HarnessCard`].
///
/// Spawn takes [`AgentRunRequest`], not the native [`ChildRequest`]: the
/// heterogeneous boundary must not pull CancelToken / fork / surface-completion
/// semantics into every driver. The native adapter translates.
///
/// Drivers that advertise [`HarnessCapabilities::resumable`] `= false` may leave
/// [`resume`](Self::resume) unimplemented — the default returns
/// [`DriverError::Unsupported`].
///
/// V1 omits `subscribe`/event-streaming: that needs a `Stream` return type
/// which complicates object-safety. Add it in a later PR once the registry
/// exists and a caller actually needs streaming observation.
///
/// [`ChildRunner`]: crate::state::ChildRunner
/// [`ChildRequest`]: crate::types::ChildRequest
#[async_trait::async_trait]
pub trait AgentDriver: Send + Sync {
    /// Stable harness id this driver handles. Registry key; object-safe.
    fn id(&self) -> &str;

    /// Which harness kind category this driver belongs to.
    fn kind(&self) -> HarnessKind;

    /// Start a child run. Returns a handle for later inspect/cancel/resume.
    async fn spawn(&self, request: AgentRunRequest) -> Result<AgentRunHandle, DriverError>;

    /// Snapshot the current state of a run (without subscribing to events).
    async fn inspect(&self, handle: &AgentRunHandle) -> Result<AgentRunSnapshot, DriverError>;

    /// Request cancellation. The run may not stop immediately; poll
    /// [`inspect`](Self::inspect) to confirm.
    async fn cancel(&self, handle: &AgentRunHandle) -> Result<(), DriverError>;

    /// Resume a previously completed run's conversation with a new prompt.
    ///
    /// Default: [`DriverError::Unsupported`]. Drivers with
    /// [`HarnessCapabilities::resumable`] `= false` need not override this.
    async fn resume(&self, request: ResumeRequest) -> Result<AgentRunHandle, DriverError> {
        let _ = request;
        Err(DriverError::Unsupported("resume".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── serde ────────────────────────────────────────────────────────────

    #[test]
    fn harness_card_round_trips_through_serde() {
        let card = HarnessCard {
            id: HarnessId::from("native"),
            kind: HarnessKind::Native,
            name: "Native".into(),
            capabilities: HarnessCapabilities {
                streaming_tools: true,
                resumable: false,
                cancellable: true,
            },
        };
        let json = serde_json::to_string(&card).unwrap();
        let back: HarnessCard = serde_json::from_str(&json).unwrap();
        assert_eq!(card, back);
    }

    #[test]
    fn harness_kind_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&HarnessKind::Native).unwrap(),
            "\"native\""
        );
        assert_eq!(
            serde_json::to_string(&HarnessKind::Process).unwrap(),
            "\"process\""
        );
        assert_eq!(serde_json::to_string(&HarnessKind::Sdk).unwrap(), "\"sdk\"");
        assert_eq!(
            serde_json::to_string(&HarnessKind::Remote).unwrap(),
            "\"remote\""
        );
    }

    #[test]
    fn harness_kind_all_variants_serialize() {
        for kind in [
            HarnessKind::Native,
            HarnessKind::Process,
            HarnessKind::Sdk,
            HarnessKind::Remote,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: HarnessKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back, "round-trip failed for {kind:?}");
        }
    }

    #[test]
    fn harness_capabilities_default_all_false() {
        let caps = HarnessCapabilities::default();
        assert!(!caps.streaming_tools);
        assert!(!caps.resumable);
        assert!(!caps.cancellable);
    }

    #[test]
    fn harness_capabilities_field_keys_are_snake_case() {
        let json = serde_json::to_value(HarnessCapabilities {
            streaming_tools: true,
            resumable: true,
            cancellable: false,
        })
        .unwrap();
        let obj = json.as_object().unwrap();
        assert!(obj.contains_key("streaming_tools"));
        assert!(obj.contains_key("resumable"));
        assert!(obj.contains_key("cancellable"));
        assert!(!obj.contains_key("streamingTools"));
    }

    // ── status vocabulary ────────────────────────────────────────────────

    #[test]
    fn agent_run_status_finished_carries_full_child_outcome_vocabulary() {
        for outcome in [
            ChildOutcome::Completed,
            ChildOutcome::Failed,
            ChildOutcome::Cancelled,
            ChildOutcome::TimedOut,
            ChildOutcome::Lost,
        ] {
            let status = AgentRunStatus::Finished(outcome);
            assert_eq!(status, AgentRunStatus::Finished(outcome));
        }
        // TimedOut / Lost must remain expressible — they are not collapsed
        // into Failed / Cancelled at this boundary.
        assert_eq!(
            AgentRunStatus::Finished(ChildOutcome::TimedOut),
            AgentRunStatus::Finished(ChildOutcome::TimedOut)
        );
        assert_eq!(
            AgentRunStatus::Finished(ChildOutcome::Lost),
            AgentRunStatus::Finished(ChildOutcome::Lost)
        );
    }

    #[test]
    fn snapshot_result_present_only_when_finished() {
        let finished = AgentRunSnapshot {
            status: AgentRunStatus::Finished(ChildOutcome::TimedOut),
            result: Some(ChildResult {
                outcome: ChildOutcome::TimedOut,
                ..ChildResult::default()
            }),
        };
        assert!(matches!(
            finished.status,
            AgentRunStatus::Finished(ChildOutcome::TimedOut)
        ));
        assert!(finished.result.is_some());

        let running = AgentRunSnapshot {
            status: AgentRunStatus::Running,
            result: None,
        };
        assert!(running.result.is_none());
    }

    // ── handle ───────────────────────────────────────────────────────────

    #[test]
    fn agent_run_handle_clones_correctly() {
        let handle = AgentRunHandle {
            run_id: "run-1".into(),
            session_ref: Some("ext-session-abc".into()),
        };
        let cloned = handle.clone();
        assert_eq!(handle, cloned);
    }

    // ── error display ────────────────────────────────────────────────────

    #[test]
    fn driver_error_displays_without_panic() {
        assert_eq!(
            DriverError::NotFound("run-1".into()).to_string(),
            "run not found: run-1"
        );
        assert_eq!(
            DriverError::Unsupported("subscribe".into()).to_string(),
            "driver does not support this operation: subscribe"
        );
        assert_eq!(
            DriverError::Internal("spawn failed".into()).to_string(),
            "driver internal error: spawn failed"
        );
    }

    // ── object safety ────────────────────────────────────────────────────

    /// A no-op driver whose every method returns `Unsupported` (resume uses
    /// the trait default). Exists solely to prove the trait is object-safe
    /// (`Box<dyn AgentDriver>` compiles) and to serve as a template for
    /// future implementations.
    struct UnsupportedDriver {
        id: String,
        kind: HarnessKind,
    }

    #[async_trait::async_trait]
    impl AgentDriver for UnsupportedDriver {
        fn id(&self) -> &str {
            &self.id
        }

        fn kind(&self) -> HarnessKind {
            self.kind
        }

        async fn spawn(&self, _request: AgentRunRequest) -> Result<AgentRunHandle, DriverError> {
            Err(DriverError::Unsupported("spawn".into()))
        }

        async fn inspect(&self, _handle: &AgentRunHandle) -> Result<AgentRunSnapshot, DriverError> {
            Err(DriverError::Unsupported("inspect".into()))
        }

        async fn cancel(&self, _handle: &AgentRunHandle) -> Result<(), DriverError> {
            Err(DriverError::Unsupported("cancel".into()))
        }
    }

    #[tokio::test]
    async fn trait_is_object_safe() {
        let driver: Box<dyn AgentDriver> = Box::new(UnsupportedDriver {
            id: "unsupported-sdk".into(),
            kind: HarnessKind::Sdk,
        });
        assert_eq!(driver.id(), "unsupported-sdk");
        assert_eq!(driver.kind(), HarnessKind::Sdk);

        let handle = AgentRunHandle {
            run_id: "test".into(),
            session_ref: None,
        };
        let request = AgentRunRequest {
            run_id: "test".into(),
            prompt: "hi".into(),
            agent: "explore".into(),
            parent_alias: "parent-alias".into(),
            cwd: None,
            resume_from: None,
        };

        assert!(matches!(
            driver.spawn(request).await,
            Err(DriverError::Unsupported(_))
        ));
        assert!(matches!(
            driver.inspect(&handle).await,
            Err(DriverError::Unsupported(_))
        ));
        assert!(matches!(
            driver.cancel(&handle).await,
            Err(DriverError::Unsupported(_))
        ));
        // Default resume impl — not overridden on UnsupportedDriver.
        assert!(matches!(
            driver
                .resume(ResumeRequest {
                    run_id: "test".into(),
                    prompt: "again".into(),
                })
                .await,
            Err(DriverError::Unsupported(_))
        ));
    }

    #[test]
    fn two_process_harnesses_do_not_share_registry_id() {
        let claude = HarnessCard {
            id: HarnessId::from("claude-code"),
            kind: HarnessKind::Process,
            name: "Claude Code".into(),
            capabilities: HarnessCapabilities::default(),
        };
        let codex = HarnessCard {
            id: HarnessId::from("codex"),
            kind: HarnessKind::Process,
            name: "Codex".into(),
            capabilities: HarnessCapabilities::default(),
        };
        assert_eq!(claude.kind, codex.kind);
        assert_ne!(claude.id, codex.id);
    }
}

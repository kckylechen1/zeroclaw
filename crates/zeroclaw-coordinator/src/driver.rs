//! Heterogeneous agent driver boundary.
//!
//! [`ChildRunner`] remains the coordinator's internal lifecycle seam — it is
//! generic over associated types and drives one in-process run. This module
//! adds the registry-facing surface: an object-safe [`AgentDriver`] trait that
//! a future driver registry (`HashMap<HarnessKind, Box<dyn AgentDriver>>`)
//! dispatches on, plus the [`HarnessCard`] config entity that names which
//! backend owns a given agent.
//!
//! Nothing in this module is wired into the live coordinator yet. A later PR
//! adapts `NativeChildRunner` to `impl AgentDriver`; after that comes the
//! registry, then `JoinMode`, then delegate convergence.
//!
//! [`ChildRunner`]: crate::state::ChildRunner

use std::fmt;

use crate::types::ChildRequest;
use crate::types::ChildResult;

// ── HarnessCard ──────────────────────────────────────────────────────────

/// How an agent is executed and controlled. One per harness backend.
///
/// V1 is a plain struct carried inside the coordinator crate. A later PR
/// moves this to `zeroclaw-config` as a `[harness.<alias>]` TOML surface once
/// the driver registry exists and the fields stabilize.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HarnessCard {
    /// Which driver backend handles this harness.
    pub kind: HarnessKind,
    /// Operator-visible name (e.g. "native", "claude-code", "codex").
    pub name: String,
    /// Capabilities the driver declares it can satisfy.
    pub capabilities: HarnessCapabilities,
}

/// Broad category of execution backend. Drivers register under exactly one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// Stable id — matches `ChildRequest::child_id`.
    pub run_id: String,
    /// Driver-specific session reference (e.g. external CLI session id).
    pub session_ref: Option<String>,
}

/// Point-in-time snapshot of a run, returned by [`AgentDriver::inspect`].
#[derive(Debug, Clone)]
pub struct AgentRunSnapshot {
    pub status: AgentRunStatus,
    /// Present once the run reaches a terminal status.
    pub result: Option<ChildResult>,
}

/// Coarse lifecycle status a driver can report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRunStatus {
    /// Spawned but not yet executing.
    Pending,
    /// Actively running.
    Running,
    /// Finished successfully.
    Completed,
    /// Ended in error.
    Failed,
    /// Cancelled by the caller.
    Cancelled,
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
/// One implementation per backend (`Native`, `Process`, `Sdk`, `Remote`).
/// The coordinator's [`ChildRunner`] stays the internal lifecycle seam; this
/// trait is the registry surface that picks which driver handles a given
/// [`HarnessCard`].
///
/// Methods reuse coordinator types ([`ChildRequest`], [`ChildResult`]) so an
/// adapter from `ChildRunner` to `AgentDriver` is a thin wrapper, not a
/// translation layer.
///
/// V1 omits `subscribe`/event-streaming: that needs a `Stream` return type
/// which complicates object-safety. Add it in a later PR once the registry
/// exists and a caller actually needs streaming observation.
///
/// [`ChildRunner`]: crate::state::ChildRunner
#[async_trait::async_trait]
pub trait AgentDriver: Send + Sync {
    /// Which harness kind this driver handles.
    fn kind(&self) -> HarnessKind;

    /// Start a child run. Returns a handle for later inspect/cancel/resume.
    async fn spawn(&self, request: ChildRequest) -> Result<AgentRunHandle, DriverError>;

    /// Snapshot the current state of a run (without subscribing to events).
    async fn inspect(&self, handle: &AgentRunHandle) -> Result<AgentRunSnapshot, DriverError>;

    /// Request cancellation. The run may not stop immediately; poll
    /// [`inspect`](Self::inspect) to confirm.
    async fn cancel(&self, handle: &AgentRunHandle) -> Result<(), DriverError>;

    /// Resume a previously completed run's conversation with a new prompt.
    async fn resume(&self, request: ResumeRequest) -> Result<AgentRunHandle, DriverError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── serde round-trips ────────────────────────────────────────────────

    #[test]
    fn harness_card_round_trips_through_serde() {
        let card = HarnessCard {
            kind: HarnessKind::Native,
            name: "native".into(),
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

    /// A no-op driver whose every method returns `Unsupported`. Exists solely
    /// to prove the trait is object-safe (`Box<dyn AgentDriver>` compiles) and
    /// to serve as a template for future implementations.
    struct UnsupportedDriver {
        kind: HarnessKind,
    }

    #[async_trait::async_trait]
    impl AgentDriver for UnsupportedDriver {
        fn kind(&self) -> HarnessKind {
            self.kind
        }

        async fn spawn(&self, _request: ChildRequest) -> Result<AgentRunHandle, DriverError> {
            Err(DriverError::Unsupported("spawn".into()))
        }

        async fn inspect(&self, _handle: &AgentRunHandle) -> Result<AgentRunSnapshot, DriverError> {
            Err(DriverError::Unsupported("inspect".into()))
        }

        async fn cancel(&self, _handle: &AgentRunHandle) -> Result<(), DriverError> {
            Err(DriverError::Unsupported("cancel".into()))
        }

        async fn resume(&self, _request: ResumeRequest) -> Result<AgentRunHandle, DriverError> {
            Err(DriverError::Unsupported("resume".into()))
        }
    }

    #[tokio::test]
    async fn trait_is_object_safe() {
        let driver: Box<dyn AgentDriver> = Box::new(UnsupportedDriver {
            kind: HarnessKind::Sdk,
        });
        assert_eq!(driver.kind(), HarnessKind::Sdk);

        // Every method returns Unsupported — confirm the dispatch works
        // through the trait object.
        let handle = AgentRunHandle {
            run_id: "test".into(),
            session_ref: None,
        };
        assert!(matches!(
            driver.cancel(&handle).await,
            Err(DriverError::Unsupported(_))
        ));
    }
}

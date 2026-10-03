//! Shared Tachi Staff receipts and error vocabulary (ADR-017).

use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

/// Why the body is handing work to Tachi instead of doing it itself. Mirrors
/// Tachi's `TachiDispatchReason`; Tachi refuses a start without one.
///
/// Use [`StaffingReason::ExplicitUserRequest`] when the owner asked for the
/// delegation. Otherwise the model picks the reason that is true.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StaffingReason {
    /// The owner explicitly asked for this work to be delegated.
    ExplicitUserRequest,
    /// The work must outlive this conversation or process.
    DurableCrossSession,
    /// The work must run on, or be reachable from, another device.
    CrossDeviceRemote,
    /// The body has no native way to do this work itself.
    NativeSubagentUnavailable,
}

impl StaffingReason {
    /// Every reason, in Tachi's order.
    pub const ALL: [Self; 4] = [
        Self::ExplicitUserRequest,
        Self::DurableCrossSession,
        Self::CrossDeviceRemote,
        Self::NativeSubagentUnavailable,
    ];

    /// The wire spelling Tachi expects.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitUserRequest => "explicit_user_request",
            Self::DurableCrossSession => "durable_cross_session",
            Self::CrossDeviceRemote => "cross_device_remote",
            Self::NativeSubagentUnavailable => "native_subagent_unavailable",
        }
    }

    /// Parse a wire spelling (for a model-chosen reason). Unknown text is
    /// `None`, never a default reason.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|reason| reason.as_str() == value.trim())
    }
}

/// Optional references bound to a start. Tachi stores them on the run; they
/// never select how the run executes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StaffRefs {
    /// GitHub issue reference, for example `owner/repo#123`.
    pub issue_ref: Option<String>,
    /// GitHub pull request reference.
    pub pr_ref: Option<String>,
    /// Tachi flow id for feature-scoped linkage.
    pub flow_id: Option<String>,
}

/// A Tachi task state (`TASK_STATE_*`). Unknown states are kept verbatim so
/// a newer Tachi never makes a read fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunState {
    /// `TASK_STATE_SUBMITTED`.
    Submitted,
    /// `TASK_STATE_WORKING`.
    Working,
    /// `TASK_STATE_INPUT_REQUIRED`: terminal only when `closure_kind` is
    /// `partial`.
    InputRequired,
    /// `TASK_STATE_COMPLETED`.
    Completed,
    /// `TASK_STATE_FAILED`.
    Failed,
    /// `TASK_STATE_CANCELED`.
    Canceled,
    /// Any other state string.
    Other(String),
}

impl RunState {
    /// Parse a Tachi state string.
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value {
            "TASK_STATE_SUBMITTED" => Self::Submitted,
            "TASK_STATE_WORKING" => Self::Working,
            "TASK_STATE_INPUT_REQUIRED" => Self::InputRequired,
            "TASK_STATE_COMPLETED" => Self::Completed,
            "TASK_STATE_FAILED" => Self::Failed,
            "TASK_STATE_CANCELED" => Self::Canceled,
            other => Self::Other(other.to_string()),
        }
    }

    /// The Tachi state string.
    #[must_use]
    pub fn as_wire(&self) -> &str {
        match self {
            Self::Submitted => "TASK_STATE_SUBMITTED",
            Self::Working => "TASK_STATE_WORKING",
            Self::InputRequired => "TASK_STATE_INPUT_REQUIRED",
            Self::Completed => "TASK_STATE_COMPLETED",
            Self::Failed => "TASK_STATE_FAILED",
            Self::Canceled => "TASK_STATE_CANCELED",
            Self::Other(other) => other,
        }
    }
}

impl<'de> Deserialize<'de> for RunState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::from_wire(&raw))
    }
}

impl fmt::Display for RunState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

/// What Tachi answers to an accepted start. Tachi guarantees these three
/// fields and a durable `status.json` before it answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaffReceipt {
    /// Canonical run id; every later call names the run by it.
    pub dispatch_id: String,
    /// Initial state, `TASK_STATE_WORKING` for a fresh run.
    pub state: RunState,
    /// Tachi's run directory (on the Tachi machine).
    pub run_dir: String,
}

/// A projection of Tachi's canonical `status.json` for one run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunStatus {
    /// Canonical run id.
    pub dispatch_id: String,
    /// Current state.
    pub state: RunState,
    /// Receipt revision; `cancel` must quote it. Historical receipts may not
    /// carry one.
    #[serde(default, rename = "status_revision")]
    pub revision: Option<u64>,
    /// `partial` closes an `INPUT_REQUIRED` run.
    #[serde(default)]
    pub closure_kind: Option<String>,
    /// Whether the run has written `result.md`.
    #[serde(default)]
    pub result_written: Option<bool>,
    /// Last update time as Tachi wrote it.
    #[serde(default)]
    pub updated_at: Option<String>,
}

impl RunStatus {
    /// True when Tachi will not move this run any further: completed,
    /// failed, canceled, or input-required with a partial closure.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        match self.state {
            RunState::Completed | RunState::Failed | RunState::Canceled => true,
            RunState::InputRequired => self.closure_kind.as_deref() == Some("partial"),
            _ => false,
        }
    }
}

/// A run's report (`result.md`), as far as Tachi serves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunResult {
    /// Canonical execution state from `run_status`, never the task read-model state.
    pub state: RunState,
    /// Canonical receipt served alongside the report.
    pub status: RunStatus,
    /// Task facade projection; it may differ from canonical run truth.
    pub task_state: RunState,
    /// Why the task facade inferred its state, when provided.
    pub state_basis: Option<String>,
    /// Tachi same-receipt management/recovery projection, when provided.
    pub managed_run: Option<serde_json::Value>,
    /// Report text; `None` when the run has not written one.
    pub body: Option<String>,
    /// True when Tachi cut the report at its response cap.
    pub truncated: bool,
}

/// How far an accepted cancellation got.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelOutcome {
    /// Recorded; the run has not stopped yet.
    Requested,
    /// The run is stopped and canceled.
    Confirmed,
    /// Tachi could not prove the process stopped; the run is failed.
    TerminationUnconfirmed,
}

/// Tachi's cancellation receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CancelReceipt {
    /// What happened.
    pub outcome: CancelOutcome,
    /// Run state the receipt reports, when it reports one.
    pub state: Option<RunState>,
}

/// Every way a `tachi_staff` call can fail.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TachiStaffError {
    /// Tachi is disabled, not configured, or not reachable. Delegation stops
    /// here; nothing is run locally instead.
    #[error("Tachi unavailable: {0}")]
    Unavailable(String),
    /// A start was sent, but no trustworthy receipt was received. It may have
    /// started; never retry this request automatically.
    #[error("Tachi submission outcome unknown: {0}")]
    SubmissionUnknown(String),
    /// Tachi answered and refused (validation, admission, unknown run, …).
    #[error("Tachi refused the request: {0}")]
    Refused(String),
    /// The harness name is not listed in `[tachi.harnesses]`.
    #[error("unknown harness `{0}`: not listed in [tachi.harnesses]")]
    UnknownHarness(String),
    /// Tachi cannot cancel this run (today: every CLI-backed run; only
    /// same-daemon managed-custom runs are cancellable).
    #[error("Tachi cannot cancel this run ({reason})")]
    CancelUnsupported { reason: String },
    /// `cancel` quoted a revision the run has moved past; read status again.
    #[error("stale status revision: expected {expected}, run is at {observed:?}")]
    StaleRevision {
        expected: u64,
        observed: Option<u64>,
    },
    /// Tachi answered with something this client cannot read.
    #[error("unreadable Tachi response: {0}")]
    Protocol(String),
}

impl Serialize for RunState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_wire())
    }
}

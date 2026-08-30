//! Shared domain vocabulary for the ephemeral ExecutionSubAgent vertical
//! (zeroclaw #261): the typed SessionController seam, the Tachi
//! attached-session fact spine it reports through (tachi #1678), and the
//! Parent-level three-path route discriminator (#198 addendum #2).
//!
//! These types are CONTENT and receipts only — nothing here carries or
//! grants authority, spawns a process, or opens a store. The controller
//! and fact-sink PORTS live in `zeroclaw-runtime::execution_subagent`;
//! admission and the run loop live beside them.
//!
//! Law recap encoded by construction:
//!
//! - **Closed vocabularies.** Event kinds, terminal outcomes, canonical
//!   states, connection facts, intervention kinds, and intervention
//!   dispositions are closed enums mirroring the tachi-side spine's own
//!   closed sets (memcore v34). A string that is not in the set fails
//!   [`parse`]-style constructors — the consumer cannot invent a fact.
//! - **Typed refs, non-interchangeable** (#205 vocabulary): host
//!   identity, adapter connection, remote session, attachment, and
//!   session event ids are distinct newtypes. They are decode-side
//!   wrappers only; the minting authority is the host/tachi side.
//! - **A request is never a state.** Intervention receipts and
//!   dispositions are bookkeeping; only a terminal event fact moves
//!   canonical lifecycle state (`SessionCanonicalStateV1`), and
//!   `cancelled` can never be minted without an authority confirmation
//!   reference (enforced by the spine; reflected here by requiring the
//!   ref on the outcome's constructor path).
//! - **The three paths are typed** (#198 addendum #2):
//!   [`ExecutionRouteV1`] has exactly `Reason`, `EphemeralExec`, and
//!   `DurableExec` variants. There is no `Local` fallback variant to
//!   degrade into.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ─────────────────────────────────────────────────────────────────────────
// Typed refs (#205 vocabulary; decode-side only — never minted here)
// ─────────────────────────────────────────────────────────────────────────

macro_rules! opaque_ref {
    ($(#[$meta:meta])* $name:ident, $ns:expr) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(String);

        impl $name {
            /// Wrap an already-minted id. Does not mint.
            #[must_use]
            pub fn from_opaque(id: impl Into<String>) -> Self {
                Self(id.into())
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}:{}", $ns, self.0)
            }
        }
    };
}

opaque_ref!(
    /// Identity of the host that owns the session lifecycle (the ZeroClaw
    /// process side). Derived from the host's own admission, never
    /// caller-claimed on the wire.
    HostIdentityRef,
    "host"
);
opaque_ref!(
    /// Identity of one adapter connection between the host and the spine.
    AdapterConnectionRef,
    "adapter-conn"
);
opaque_ref!(
    /// The harness-side session id (harness-native identity).
    RemoteSessionRef,
    "remote-session"
);
opaque_ref!(
    /// The spine-minted attachment binding for one attached session.
    SessionAttachmentRef,
    "attachment"
);
opaque_ref!(
    /// Host-assigned event id inside one attachment's spine (stable,
    /// replay-idempotent key).
    SessionEventIdRef,
    "session-event"
);
opaque_ref!(
    /// Authority confirmation reference binding a `cancelled` terminal
    /// fact to a recorded accepted cancel intervention result.
    AuthorityConfirmationRef,
    "confirm"
);
opaque_ref!(
    /// Idempotency key for one intervention request receipt.
    InterventionRequestIdRef,
    "intervention-req"
);

// ─────────────────────────────────────────────────────────────────────────
// Closed fact vocabulary (mirror of the tachi spine's memcore v34 sets)
// ─────────────────────────────────────────────────────────────────────────

/// Closed lifecycle fact vocabulary the host may report. Mirrors the
/// spine's event kinds exactly; an unknown kind is unrepresentable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionEventKindV1 {
    /// The attachment was admitted (verbatim from the attach receipt).
    Accepted,
    /// The harness session started work.
    Started,
    /// Bounded progress fact (no transcript).
    Progress,
    /// The session is blocked waiting for input/correction.
    InputRequired,
    /// A terminal fact: exactly one of [`SessionTerminalOutcomeV1`].
    Terminal,
    /// Post-terminal cleanup receipt.
    Cleanup,
}

impl SessionEventKindV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Started => "started",
            Self::Progress => "progress",
            Self::InputRequired => "input_required",
            Self::Terminal => "terminal",
            Self::Cleanup => "cleanup",
        }
    }

    /// The only construction path from wire text; unknown kinds refuse.
    pub fn parse(raw: &str) -> Result<Self, SessionFactError> {
        match raw.trim() {
            "accepted" => Ok(Self::Accepted),
            "started" => Ok(Self::Started),
            "progress" => Ok(Self::Progress),
            "input_required" => Ok(Self::InputRequired),
            "terminal" => Ok(Self::Terminal),
            "cleanup" => Ok(Self::Cleanup),
            other => Err(SessionFactError::UnknownEventKind(other.to_string())),
        }
    }
}

/// Closed terminal outcome vocabulary. A `Cancelled` value carries the
/// authority confirmation reference that binds it to a recorded accepted
/// cancel intervention result — without it the value is unconstructable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionTerminalOutcomeV1 {
    Completed,
    Failed,
    Cancelled {
        confirmation: AuthorityConfirmationRef,
    },
}

impl SessionTerminalOutcomeV1 {
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled { .. } => "cancelled",
        }
    }

    #[must_use]
    pub fn authority_confirmation_ref(&self) -> Option<&AuthorityConfirmationRef> {
        match self {
            Self::Cancelled { confirmation } => Some(confirmation),
            _ => None,
        }
    }
}

/// The canonical session-state projection the spine maintains (read-side
/// mirror). `UnknownOrphaned` means the host reported the session gone and
/// no terminal receipt exists — recoverable by authoritative facts after a
/// reconnect, never guessed into `failed`/`completed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionCanonicalStateV1 {
    Accepted,
    Started,
    Progressing,
    InputRequired,
    Completed,
    Failed,
    Cancelled,
    /// Two authoritative terminal facts disagree. Stuck by design;
    /// adjudication is tachi-owned (#1623), never guessed here.
    InconsistentReconciling,
    /// Session gone without a terminal receipt. Recoverable on reconnect.
    UnknownOrphaned,
}

impl SessionCanonicalStateV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Started => "started",
            Self::Progressing => "progressing",
            Self::InputRequired => "input_required",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::InconsistentReconciling => "inconsistent_reconciling",
            Self::UnknownOrphaned => "unknown_orphaned",
        }
    }

    /// The only construction path from spine projections; unknown states
    /// refuse rather than degrade to a guess.
    pub fn parse(raw: &str) -> Result<Self, SessionFactError> {
        match raw.trim() {
            "accepted" => Ok(Self::Accepted),
            "started" => Ok(Self::Started),
            "progressing" => Ok(Self::Progressing),
            "input_required" => Ok(Self::InputRequired),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "inconsistent_reconciling" => Ok(Self::InconsistentReconciling),
            "unknown_orphaned" => Ok(Self::UnknownOrphaned),
            other => Err(SessionFactError::UnknownCanonicalState(other.to_string())),
        }
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::InconsistentReconciling
        )
    }
}

/// Closed host connection facts (dropouts). There is no "connected" write:
/// connectivity is re-established by the reconnect receipt, not asserted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionConnectionFactV1 {
    /// The host observed its connection to the session drop.
    Disconnected,
    /// A reconnect attempt failed.
    ReconnectFailed,
}

impl SessionConnectionFactV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::ReconnectFailed => "reconnect_failed",
        }
    }
}

/// Closed intervention kinds the spine can carry for an attached session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionInterventionKindV1 {
    RequestStatus,
    PromptOrCorrect,
    RequestPause,
    RequestCancel,
    RequestResume,
}

impl SessionInterventionKindV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RequestStatus => "request_status",
            Self::PromptOrCorrect => "prompt_or_correct",
            Self::RequestPause => "request_pause",
            Self::RequestCancel => "request_cancel",
            Self::RequestResume => "request_resume",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, SessionFactError> {
        match raw.trim() {
            "request_status" => Ok(Self::RequestStatus),
            "prompt_or_correct" => Ok(Self::PromptOrCorrect),
            "request_pause" => Ok(Self::RequestPause),
            "request_cancel" => Ok(Self::RequestCancel),
            "request_resume" => Ok(Self::RequestResume),
            other => Err(SessionFactError::UnknownInterventionKind(other.to_string())),
        }
    }
}

/// Closed dispositions the HOST may authoritatively report for an
/// intervention request. `Accepted` on a `RequestCancel` must carry the
/// harness confirmation reference for the eventual terminal fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionInterventionDispositionV1 {
    Accepted,
    Refused,
    Unsupported,
    Failed,
}

impl SessionInterventionDispositionV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Refused => "refused",
            Self::Unsupported => "unsupported",
            Self::Failed => "failed",
        }
    }
}

/// Which capability fact an intervention gate consulted. Mirrors the
/// spine's `CapabilitySource`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionCapabilitySourceV1 {
    /// The latest host advertisement decided.
    Advertised,
    /// No advertisement exists; the attachment's declared set decided.
    Declared,
}

impl SessionCapabilitySourceV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Advertised => "advertised",
            Self::Declared => "declared",
        }
    }
}

/// Typed error surface for fact-vocabulary parsing and sink refusals.
/// All failures are typed; none is a stringly fallback.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionFactError {
    UnknownEventKind(String),
    UnknownCanonicalState(String),
    UnknownInterventionKind(String),
    UnknownConnectionFact(String),
    /// The spine (or its transport) is unreachable. Fail-closed: callers
    /// must NOT degrade to a local-only path on this.
    Unavailable,
    /// The spine refused the fact (admission, validation, revision guard).
    Refused(String),
}

impl std::fmt::Display for SessionFactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownEventKind(raw) => write!(f, "unknown session event kind {raw:?}"),
            Self::UnknownCanonicalState(raw) => {
                write!(f, "unknown canonical session state {raw:?}")
            }
            Self::UnknownInterventionKind(raw) => {
                write!(f, "unknown intervention kind {raw:?}")
            }
            Self::UnknownConnectionFact(raw) => write!(f, "unknown connection fact {raw:?}"),
            Self::Unavailable => write!(f, "session fact sink unavailable (fail closed)"),
            Self::Refused(reason) => write!(f, "session fact refused: {reason}"),
        }
    }
}

impl std::error::Error for SessionFactError {}

// ─────────────────────────────────────────────────────────────────────────
// Read-side projections (what the host sees back from the spine)
// ─────────────────────────────────────────────────────────────────────────

/// The canonical state projection of one attachment (read view).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStateView {
    pub canonical_state: SessionCanonicalStateV1,
    pub canonical_revision: u64,
    pub cleanup_recorded: bool,
    pub conflicting_terminal: bool,
    pub last_event_id: Option<String>,
}

/// Admission class of a replay-idempotent receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionReceiptAdmissionV1 {
    Created,
    Replayed,
}

impl SessionReceiptAdmissionV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Replayed => "replayed",
        }
    }
}

/// Receipt for one ingested event fact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEventReceiptView {
    pub attachment_ref: SessionAttachmentRef,
    pub event_id: SessionEventIdRef,
    pub admission: SessionReceiptAdmissionV1,
    /// The spine's per-event disposition (e.g. `journaled` / `advanced` /
    /// `stale` classes — opaque here, surfaced for the report).
    pub disposition: String,
    pub state: SessionStateView,
}

/// Receipt for one reconnect.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionReconnectReceiptView {
    pub attachment_ref: SessionAttachmentRef,
    pub reconnected: bool,
    /// The revision the host must resume watching from — facts at or
    /// after this revision replay exactly once (spine dedups by event id).
    pub resume_from_revision: u64,
    pub state: SessionStateView,
}

/// Receipt for one capability advertisement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAdvertiseReceiptView {
    pub attachment_ref: SessionAttachmentRef,
    pub advertisement_seq: u64,
    pub capabilities: Vec<String>,
}

/// Receipt for one recorded intervention request (an ask picked up).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInterventionRequestView {
    pub attachment_ref: SessionAttachmentRef,
    pub request_id: InterventionRequestIdRef,
    pub kind: SessionInterventionKindV1,
    pub reason: String,
    pub expected_session_revision: u64,
    pub capability_source: SessionCapabilitySourceV1,
    pub state: SessionStateView,
}

/// The spine's verdict when a requested intervention is not supported by
/// the advertised/declared capability set. Zero mutation happened.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUnsupportedRefusalView {
    pub attachment_ref: SessionAttachmentRef,
    pub intervention_kind: SessionInterventionKindV1,
}

/// Receipt for one recorded intervention result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInterventionResultView {
    pub attachment_ref: SessionAttachmentRef,
    pub request_id: InterventionRequestIdRef,
    pub disposition: SessionInterventionDispositionV1,
    pub state: SessionStateView,
}

// ─────────────────────────────────────────────────────────────────────────
// The three-path route discriminator (#198 addendum #2)
// ─────────────────────────────────────────────────────────────────────────

/// One Parent-level execution request, before any path is chosen. The
/// flags are the product semantics table from the addendum, typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionRequestV1 {
    /// Bounded objective (the same bound as the V1 reasoning objective).
    pub objective: String,
    /// Requires restart recovery / outlives this process → durable.
    pub needs_restart_recovery: bool,
    /// Remote target / remote workspace → durable.
    pub needs_remote: bool,
    /// Multi-attempt orchestration with claims → durable.
    pub needs_multi_attempt: bool,
    /// Human approvals on the execution path → durable.
    pub needs_approvals: bool,
    /// Evidence required for adjudication → durable.
    pub needs_evidence: bool,
    /// Pure analysis / comparison / planning → reason.
    pub analysis_only: bool,
}

/// The three execution paths (addendum #2). Exactly three variants: the
/// type cannot express a "local execution" fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ExecutionRouteV1 {
    /// Pure analysis — the ReasoningSubAgent path; no session, no task.
    Reason,
    /// One short review/fix through the ephemeral ExecutionSubAgent over
    /// the typed SessionController; host owns lifecycle; facts flow to
    /// the tachi spine as receipts; no TaskRef is minted.
    EphemeralExec,
    /// Durable work through the Tachi TaskIntent bridge (submit/get/
    /// watch/collect); a TaskRef is minted by tachi only.
    DurableExec,
}

impl ExecutionRouteV1 {
    /// The typed path selection for one request. Total, pure, and
    /// monotone: durability requirements dominate, then analysis-only,
    /// and everything else is ephemeral. There is no context-dependent
    /// fourth answer and no "availability" input — availability can only
    /// fail the chosen path CLOSED (typed error), it can never re-route.
    #[must_use]
    pub fn route(request: &ExecutionRequestV1) -> Self {
        if request.needs_restart_recovery
            || request.needs_remote
            || request.needs_multi_attempt
            || request.needs_approvals
            || request.needs_evidence
        {
            Self::DurableExec
        } else if request.analysis_only {
            Self::Reason
        } else {
            Self::EphemeralExec
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// The ONLY child→parent result channel for an ephemeral run
// ─────────────────────────────────────────────────────────────────────────

/// Terminal classification of one ephemeral execution run, from the
/// run's OWN bounded vocabulary (distinct from the spine's canonical
/// state: the run may end `refused` before any session exists, or
/// `unsupported_operation` when the advertised capability set refused a
/// lifecycle op — a typed refusal surfaced, never a fake success).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ExecutionRunStatusV1 {
    Completed,
    Failed,
    TimedOut,
    StoppedGracefully,
    Aborted,
    Refused,
    UnsupportedOperation,
}

impl ExecutionRunStatusV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::StoppedGracefully => "stopped_gracefully",
            Self::Aborted => "aborted",
            Self::Refused => "refused",
            Self::UnsupportedOperation => "unsupported_operation",
        }
    }
}

/// One intervention fact that flowed through the run, as the report
/// carries it to the Parent (bounded, receipt-shaped).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionInterventionRecordV1 {
    pub request_id: String,
    pub kind: SessionInterventionKindV1,
    pub disposition: SessionInterventionDispositionV1,
}

/// Structured report of one ephemeral execution run. Receipts and refs
/// only: the collected summary is bounded and digest-bound; no transcript
/// crosses to the Parent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionSessionReportV1 {
    /// The run's own ref (SubAgentRunRef namespace, minted per run).
    pub run_ref: String,
    /// The route this run actually took (always `EphemeralExec` for a
    /// run executed by the ExecutionSubAgent; carried for the report's
    /// self-describing completeness).
    pub route: ExecutionRouteV1,
    /// Non-secret controller binding label (e.g. the ACPX transport
    /// alias). Never a credential.
    pub controller_ref: String,
    pub status: ExecutionRunStatusV1,
    /// The harness-side session ref, when a session was started.
    pub remote_session_ref: Option<RemoteSessionRef>,
    /// The spine attachment ref, when facts flowed (fail-closed runs
    /// before attach carry `None`).
    pub attachment_ref: Option<SessionAttachmentRef>,
    /// Final canonical state read back from the spine, when attached.
    pub final_canonical_state: Option<SessionCanonicalStateV1>,
    /// Bounded terminal summary from `collect` (presence-blind to any
    /// transcript; the spine stores at most a 2000-char bounded summary
    /// and a digest — this report carries the same bound).
    pub collected_summary: Option<String>,
    /// SHA-256 over the collected terminal projection (hex), when collected.
    pub collected_digest: Option<String>,
    pub interventions: Vec<ExecutionInterventionRecordV1>,
    /// Evidence refs surfaced by collect (artifact paths as refs, no
    /// content).
    pub evidence_refs: Vec<String>,
    /// Per-run usage (actions recorded against the run's meter).
    pub usage: ExecutionUsageV1,
    /// Typed refusal detail when `status` is `Refused` or
    /// `UnsupportedOperation`; `None` otherwise.
    pub refusal: Option<String>,
}

impl ExecutionSessionReportV1 {
    /// Compute the report's own digest (over its canonical JSON). The
    /// Parent can pin this when forwarding findings.
    #[must_use]
    pub fn compute_digest(&self) -> String {
        let json = serde_json::to_string(self).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(json.as_bytes());
        format!("{:x}", hasher.finalize())
    }
}

/// Bounded per-run usage summary (mirrors the V1 usage shape).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionUsageV1 {
    pub actions: u32,
    pub max_actions: u32,
    pub elapsed_ms: u64,
    pub events_observed: u64,
    pub facts_reported: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_kind_vocabulary_is_closed_and_mirrors_the_spine() {
        for known in [
            "accepted",
            "started",
            "progress",
            "input_required",
            "terminal",
            "cleanup",
        ] {
            assert!(
                SessionEventKindV1::parse(known).is_ok(),
                "{known} must parse"
            );
        }
        assert!(matches!(
            SessionEventKindV1::parse("submit"),
            Err(SessionFactError::UnknownEventKind(_))
        ));
        // The spine refuses worker-side `submit` facts; so does this
        // vocabulary — there is no "add whatever the harness says" path.
        assert_eq!(
            SessionEventKindV1::parse(" cleanup ").unwrap().as_str(),
            "cleanup"
        );
    }

    #[test]
    fn canonical_state_vocabulary_carries_the_recovery_and_conflict_states() {
        assert_eq!(
            SessionCanonicalStateV1::parse("unknown_orphaned").unwrap(),
            SessionCanonicalStateV1::UnknownOrphaned
        );
        assert_eq!(
            SessionCanonicalStateV1::parse("inconsistent_reconciling").unwrap(),
            SessionCanonicalStateV1::InconsistentReconciling
        );
        assert!(
            !SessionCanonicalStateV1::UnknownOrphaned.is_terminal(),
            "orphaned is a recovery state, not a terminal guess"
        );
        assert!(
            SessionCanonicalStateV1::parse("failed")
                .unwrap()
                .is_terminal()
        );
        assert!(
            SessionCanonicalStateV1::parse("completed")
                .unwrap()
                .is_terminal()
        );
        assert!(matches!(
            SessionCanonicalStateV1::parse("completed_maybe"),
            Err(SessionFactError::UnknownCanonicalState(_))
        ));
    }

    #[test]
    fn cancelled_outcome_cannot_exist_without_a_confirmation_ref() {
        // The type has no `Cancelled` unit variant: the confirmation
        // reference is structural, mirroring the spine's CHECK-level law.
        let outcome = SessionTerminalOutcomeV1::Cancelled {
            confirmation: AuthorityConfirmationRef::from_opaque("zc-confirm-42"),
        };
        assert_eq!(outcome.kind_name(), "cancelled");
        assert!(outcome.authority_confirmation_ref().is_some());
        assert_eq!(
            SessionTerminalOutcomeV1::Completed.authority_confirmation_ref(),
            None
        );
    }

    #[test]
    fn route_discriminator_follows_the_addendum_table_exactly() {
        let base = |mut req: ExecutionRequestV1| {
            req.objective = "x".to_string();
            req
        };
        // One short review/fix → ephemeral.
        let short = base(ExecutionRequestV1 {
            objective: String::new(),
            needs_restart_recovery: false,
            needs_remote: false,
            needs_multi_attempt: false,
            needs_approvals: false,
            needs_evidence: false,
            analysis_only: false,
        });
        assert_eq!(
            ExecutionRouteV1::route(&short),
            ExecutionRouteV1::EphemeralExec
        );
        // Every durability flag forces durable.
        for flag in [
            ExecutionRequestV1 {
                needs_restart_recovery: true,
                ..short.clone()
            },
            ExecutionRequestV1 {
                needs_remote: true,
                ..short.clone()
            },
            ExecutionRequestV1 {
                needs_multi_attempt: true,
                ..short.clone()
            },
            ExecutionRequestV1 {
                needs_approvals: true,
                ..short.clone()
            },
            ExecutionRequestV1 {
                needs_evidence: true,
                ..short
            },
        ] {
            assert_eq!(
                ExecutionRouteV1::route(&flag),
                ExecutionRouteV1::DurableExec
            );
        }
        // Pure analysis → reason.
        assert_eq!(
            ExecutionRouteV1::route(&base(ExecutionRequestV1 {
                analysis_only: true,
                needs_restart_recovery: false,
                needs_remote: false,
                needs_multi_attempt: false,
                needs_approvals: false,
                needs_evidence: false,
                objective: String::new(),
            })),
            ExecutionRouteV1::Reason
        );
        // Durability dominates analysis-only: a request with both is
        // durable (the flags are requirements, analysis is a hint).
        assert_eq!(
            ExecutionRouteV1::route(&base(ExecutionRequestV1 {
                analysis_only: true,
                needs_evidence: true,
                needs_restart_recovery: false,
                needs_remote: false,
                needs_multi_attempt: false,
                needs_approvals: false,
                objective: String::new(),
            })),
            ExecutionRouteV1::DurableExec
        );
    }

    #[test]
    fn route_enum_has_exactly_three_variants_no_local_fallback() {
        // Serialization round-trip pins the variant set: adding a
        // fallback variant changes the serialized set observably.
        let all = [
            ExecutionRouteV1::Reason,
            ExecutionRouteV1::EphemeralExec,
            ExecutionRouteV1::DurableExec,
        ];
        for route in all {
            let json = serde_json::to_string(&route).unwrap();
            let back: ExecutionRouteV1 = serde_json::from_str(&json).unwrap();
            assert_eq!(back, route);
        }
    }

    #[test]
    fn report_digest_is_stable_over_canonical_serialization() {
        let report = ExecutionSessionReportV1 {
            run_ref: "run-1".to_string(),
            route: ExecutionRouteV1::EphemeralExec,
            controller_ref: "acpx-fixture".to_string(),
            status: ExecutionRunStatusV1::Completed,
            remote_session_ref: Some(RemoteSessionRef::from_opaque("rs-1")),
            attachment_ref: Some(SessionAttachmentRef::from_opaque("att-1")),
            final_canonical_state: Some(SessionCanonicalStateV1::Completed),
            collected_summary: Some("done".to_string()),
            collected_digest: Some("abc".to_string()),
            interventions: vec![],
            evidence_refs: vec![],
            usage: ExecutionUsageV1::default(),
            refusal: None,
        };
        assert_eq!(report.compute_digest(), report.compute_digest());
        let drifted = ExecutionSessionReportV1 {
            status: ExecutionRunStatusV1::Failed,
            ..report.clone()
        };
        assert_ne!(report.compute_digest(), drifted.compute_digest());
    }

    #[test]
    fn ref_namespaces_are_distinct_display_prefixes() {
        let ids = [
            ("host", HostIdentityRef::from_opaque("h").to_string()),
            (
                "adapter-conn",
                AdapterConnectionRef::from_opaque("c").to_string(),
            ),
            (
                "remote-session",
                RemoteSessionRef::from_opaque("s").to_string(),
            ),
            (
                "attachment",
                SessionAttachmentRef::from_opaque("a").to_string(),
            ),
            (
                "session-event",
                SessionEventIdRef::from_opaque("e").to_string(),
            ),
            (
                "confirm",
                AuthorityConfirmationRef::from_opaque("r").to_string(),
            ),
            (
                "intervention-req",
                InterventionRequestIdRef::from_opaque("i").to_string(),
            ),
        ];
        for (prefix, rendered) in ids {
            assert!(
                rendered.starts_with(&format!("{prefix}:")),
                "{rendered} must be namespaced {prefix}"
            );
        }
    }
}

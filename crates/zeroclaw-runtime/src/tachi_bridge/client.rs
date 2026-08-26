//! The bridge client seam: the transport port trait, the read views, the
//! three per-dimension state mapping tables (TB-16), and the client-side
//! TB-7/TB-20 laws (vertical V2b; contract clauses TB-5b/TB-7/TB-8/
//! TB-9/TB-13/TB-16/TB-20).
//!
//! The port trait [`TachiTaskBridge`] is the **transport-binding point**:
//! production transports (the tachi MCP facade, when tachi wires one, or
//! an in-process host binding) implement it; the client law above it is
//! transport-independent. Exactly four operations exist here — submit /
//! get / watch / collect (owner scope; no intervene/request_stop client
//! surface in this leaf).
//!
//! TB-16 law: the bridge publishes lifecycle state in THREE independent
//! dimensions (execution / adjudication / delivery). This module holds
//! one mapping table per dimension and NO lifecycle enum anywhere else:
//! the projected-state newtypes below are constructible ONLY through the
//! mapping functions, which read their own dimension's wire label and
//! nothing else.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use zeroclaw_api::subagent_v1::SupervisorAuthority;
use zeroclaw_api::taskintent::{
    AttemptRef, BoundedText, InterventionError, InterventionReceipt, InterventionV1, RequestId,
    RequesterRef, StopMode, StopReceipt, TaskIntentV1, TaskRef,
};

use super::compose::{ComposeRejection, scan_client_authored_refs, scan_intent, scan_text};

// ─────────────────────────────────────────────────────────────────────────
// TB-16: three per-dimension mapping tables
// ─────────────────────────────────────────────────────────────────────────

/// The bridge's execution-dimension wire labels (the ~13-state
/// task-level execution vocabulary, snake_case wire form). This table is
/// the ONLY admitted label set for the execution dimension.
pub const EXECUTION_STATE_LABELS: &[&str] = &[
    "queued",
    "running",
    "waiting_input",
    "submitted",
    "completed",
    "failed",
    "partial",
    "timed_out",
    "cancellation_requested",
    "cancelled",
    "orphaned",
    "inconsistent",
    "outcome_unknown",
];

/// The bridge's adjudication-dimension wire labels.
pub const ADJUDICATION_STATE_LABELS: &[&str] = &[
    "unreviewed",
    "accepted",
    "rejected",
    "not_required",
    "needs_follow_up",
    "inconsistent",
];

/// The bridge's delivery-dimension wire labels (pull-only V2 subset).
pub const DELIVERY_STATE_LABELS: &[&str] = &["not_ready", "ready"];

macro_rules! dimension_state {
    ($(#[$meta:meta])* $name:ident, $labels:expr, $dimension:expr) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub struct $name(String);

        impl $name {
            /// The projected label (always a member of this dimension's
            /// mapping table).
            pub fn label(&self) -> &str {
                &self.0
            }

            /// The dimension mapping table: the only construction path.
            /// Reads its own dimension's wire label and nothing else
            /// (TB-16 cross-dimension law is structural — the function
            /// signature admits no other dimension's input).
            pub fn project(wire_label: &str) -> Option<Self> {
                if $labels.contains(&wire_label) {
                    Some(Self(wire_label.to_string()))
                } else {
                    None
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}:{}", $dimension, self.0)
            }
        }
    };
}

dimension_state!(
    /// Projected EXECUTION-dimension state (TB-16). Constructed only by
    /// [`ProjectedExecutionState::project`] from the bridge's own
    /// execution label — this client defines no independent lifecycle
    /// enum, and never sources state from a `tachi_task` read-model row.
    ProjectedExecutionState,
    EXECUTION_STATE_LABELS,
    "exec"
);
dimension_state!(
    /// Projected ADJUDICATION-dimension state (TB-16).
    ProjectedAdjudicationState,
    ADJUDICATION_STATE_LABELS,
    "adjudication"
);
dimension_state!(
    /// Projected DELIVERY-dimension state (TB-16).
    ProjectedDeliveryState,
    DELIVERY_STATE_LABELS,
    "delivery"
);

// ─────────────────────────────────────────────────────────────────────────
// Read views (bridge projections; refs, never relay prose)
// ─────────────────────────────────────────────────────────────────────────

/// `get(task_ref)` projection over Tachi truth (TB-8). Lifecycle state
/// arrives per-dimension from the bridge and is projected through the
/// TB-16 tables above.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSnapshotView {
    /// The Tachi-minted task identity.
    pub task_ref: TaskRef,
    /// Monotonic snapshot revision.
    pub task_revision: u64,
    /// Execution-dimension projected state.
    pub execution: ProjectedExecutionState,
    /// Adjudication-dimension projected state.
    pub adjudication: ProjectedAdjudicationState,
    /// Delivery-dimension projected state.
    pub delivery: ProjectedDeliveryState,
    /// Lifecycle mode label the bridge advertised (e.g.
    /// `tachi_managed_batch`), if any. Contract-sanctioned observation
    /// (TB-2): reading the plan's mode is not placement authority.
    pub lifecycle_mode: Option<String>,
    /// Canonical digest of the admitted intent (TB-7 rule 1).
    pub intent_digest: String,
}

/// One page of durable events (TB-9). Reconnect with
/// `after_seq = last_seen` replays exactly the missed events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskEventPageView {
    /// The watched task.
    pub task_ref: TaskRef,
    /// Events with `seq > after_seq`, in seq order.
    pub events: Vec<TaskEventView>,
    /// Whether more events exist beyond this page.
    pub has_more: bool,
}

/// One durable event binding the TB-9 8-field set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskEventView {
    /// 1) Monotonic per-task sequence.
    pub seq: u64,
    /// 2) Stable event id (deterministic duplicate suppression key).
    pub event_id: String,
    /// 3) Source identity label (which truth surface produced the fact).
    pub source: String,
    /// 4) Source revision.
    pub source_revision: String,
    /// 5) When the fact occurred.
    pub occurred_at: String,
    /// 6) When Tachi recorded the fact.
    pub recorded_at: String,
    /// 7) Canonical payload digest (SHA-256 hex).
    pub payload_digest: String,
    /// 8) Visibility/redaction class.
    pub visibility: String,
    /// Payload kind label (typed projection name, e.g. `task_submitted`,
    /// `outcome_observed`). Payload detail stays host-side; the Parent
    /// consumes refs and projections, not raw worker prose.
    pub kind: String,
}

/// Verification summary inside a result projection (TB-13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationSummaryView {
    /// Whether verification evidence is present.
    pub verification_present: bool,
    /// Whether a diff artifact is present.
    pub diff_present: bool,
    /// How many evidence refs back the result.
    pub evidence_ref_count: usize,
}

/// A TB-13 contract violation (e.g. a required artifact class missing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractViolationView {
    /// The artifact class whose expectation was violated.
    pub artifact_class: String,
    /// Machine-readable violation statement.
    pub violation: String,
}

/// `collect(task_ref, revision?)` projection (TB-13): artifact/evidence
/// first. A worker `success` without the required artifact/evidence does
/// not satisfy the contract — it surfaces as a violation here, never as
/// an accepted result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultProjectionView {
    /// The task the result belongs to.
    pub task_ref: TaskRef,
    /// The attempt that produced this result, if attributed.
    pub attempt_ref: Option<AttemptRef>,
    /// Terminal classification label (machine-resolved).
    pub terminal_classification: String,
    /// Canonical result artifact ref, if any.
    pub canonical_artifact_ref: Option<String>,
    /// Artifact and evidence refs backing the result.
    pub artifact_evidence_refs: Vec<String>,
    /// Verification summary.
    pub verification: VerificationSummaryView,
    /// Adjudication-dimension projected state.
    pub adjudication: ProjectedAdjudicationState,
    /// Contract violations (TB-13: presence here is not contract
    /// satisfaction).
    pub contract_violations: Vec<ContractViolationView>,
    /// Provenance projection (vendor/model identity basis). Observation
    /// only (TB-13): provenance is readable, never placement authority.
    pub provenance: String,
    /// Pending user action, if the result awaits one.
    pub pending_user_action: Option<String>,
    /// Tachi-minted monotonic result revision (newer wins by default).
    pub result_revision: u64,
}

// ─────────────────────────────────────────────────────────────────────────
// The transport port (binding point)
// ─────────────────────────────────────────────────────────────────────────

/// The `submit` typed envelope (TB-5b): every failure mode is a typed
/// receipt, never a bare `TaskRef` return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitReceipt {
    /// Admitted (or idempotently replayed): the Tachi-minted task ref.
    /// `replayed` marks a TB-7 rule-2 duplicate that returned the SAME
    /// TaskRef and started no second worker.
    Admitted {
        /// The Tachi-minted task identity.
        task_ref: TaskRef,
        /// Whether this was an idempotent replay.
        replayed: bool,
    },
    /// Typed admission rejection from the host (TB-4/TB-5 surface).
    Rejected {
        /// Machine-readable rejection reason label.
        reason: String,
    },
    /// The bridge truth source is unavailable (TB-20 fail-closed).
    Unavailable,
    /// An ambiguous submit is pending reconciliation (TB-7 rule 4).
    ReconciliationUnknown {
        /// The submitted canonical digest.
        digest: String,
    },
    /// Same `(requester, request_id)` bound to a different digest
    /// (TB-7 rule 3). Zero new execution.
    RequestIdConflict {
        /// Digest the tuple is already bound to.
        bound_digest: String,
        /// Digest of the incoming request.
        submitted_digest: String,
    },
}

/// Transport-level submit failure that is NOT a typed host receipt: the
/// response was lost before the client could observe it (timeout /
/// disconnect). TB-7 rule 4: the caller must replay the SAME
/// `(requester, request_id)`, never invent a new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("submit response lost before observation (ambiguous submit)")]
pub struct SubmitTransportError;

/// Encode-side rejection surfaced through the typed client API.
pub type SubmitRejection = ComposeRejection;

/// Typed query failure for `get`/`watch`/`collect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BridgeQueryError {
    /// The bridge is unreachable (TB-20).
    #[error("bridge unavailable")]
    Unavailable,
    /// Unknown task ref.
    #[error("task not found")]
    NotFound,
    /// No collectable result projection yet (TB-13).
    #[error("result not ready")]
    NotReady,
    /// A pinned result revision does not exist (TB-13).
    #[error("result revision not found")]
    ResultRevisionNotFound,
}

/// The Tachi TaskIntent bridge transport port — the binding point every
/// production transport implements. The owner-scoped operation set:
/// submit / get / watch / collect (V2b) plus intervene / request_stop
/// (vertical V3, TB-11/TB-12).
///
/// Implementations are Tachi-side truth: they mint `TaskRef` values,
/// derive lifecycle state through the bridge's own TB-16 mapping tables,
/// and never fall back to local execution. The ZeroClaw side constructs
/// no refs and executes no work through this seam.
///
/// **This trait is for transport IMPLEMENTORS, not callers.** Calling
/// `submit` on a port directly bypasses the encode-side admission law;
/// route submissions through [`TachiBridgeClient`], which always runs
/// the fail-closed scan first. For interventions, route through
/// [`TachiBridgeClient::intervene`] / [`TachiBridgeClient::request_stop`]
/// (same law) or — for supervisor sessions —
/// [`TachiBridgeClient::supervisor_intervene`], which additionally gates
/// on the session's granted authority set.
#[async_trait]
pub trait TachiTaskBridge: Send + Sync {
    /// `submit(TaskIntentV1, RequestId)` (TB-5b/TB-6/TB-7).
    async fn submit(
        &self,
        intent: &TaskIntentV1,
        request_id: &RequestId,
    ) -> Result<SubmitReceipt, SubmitTransportError>;

    /// `get(TaskRef)` (TB-8).
    async fn get(&self, task_ref: &TaskRef) -> Result<TaskSnapshotView, BridgeQueryError>;

    /// `watch(TaskRef, after_seq, limit)` (TB-9 durable backfill).
    async fn watch(
        &self,
        task_ref: &TaskRef,
        after_seq: u64,
        limit: usize,
    ) -> Result<TaskEventPageView, BridgeQueryError>;

    /// `collect(TaskRef, result_revision?)` (TB-13).
    async fn collect(
        &self,
        task_ref: &TaskRef,
        result_revision: Option<u64>,
    ) -> Result<ResultProjectionView, BridgeQueryError>;

    /// `intervene(TaskRef, InterventionV1, RequesterRef, RequestId,
    /// expected_task_revision?)` (TB-11, vertical V3): typed receipts or
    /// typed refusals — `unsupported_by_lifecycle_owner` /
    /// `requires_new_task_lineage` carry zero silent fallback and zero
    /// state mutation; stop-alias variants resolve to the single stop
    /// authority (TB-12).
    async fn intervene(
        &self,
        task_ref: &TaskRef,
        intervention: &InterventionV1,
        requester: &RequesterRef,
        request_id: &RequestId,
        expected_task_revision: Option<u64>,
    ) -> Result<InterventionReceipt, InterventionError>;

    /// `request_stop(TaskRef, StopMode, RequesterRef, RequestId,
    /// expected_task_revision?)` (TB-12, vertical V3): the multi-stage
    /// stop fact — requested → forwarded → confirmed. A receipt is never
    /// a `cancelled` confirmation.
    async fn request_stop(
        &self,
        task_ref: &TaskRef,
        mode: StopMode,
        requester: &RequesterRef,
        request_id: &RequestId,
        expected_task_revision: Option<u64>,
    ) -> Result<StopReceipt, InterventionError>;
}

// ─────────────────────────────────────────────────────────────────────────
// The client law (TB-7 rule 4 replay; TB-9 cursors; TB-13 revision pick)
// ─────────────────────────────────────────────────────────────────────────

/// How many times `submit_reconciling` replays the SAME tuple after an
/// ambiguous submit before surfacing `ReconciliationUnknown`.
pub const SUBMIT_RECONCILE_ATTEMPTS: usize = 3;

/// The transport-independent client over [`TachiTaskBridge`].
///
/// Holds NO durable state: the per-watched-task event cursors are
/// process-lifetime in-memory only (the TB-19 restart vertical is Batch
/// 4; a durable cursor store would need a TB-22 manifest exemption that
/// this leaf deliberately does not take).
#[derive(Clone)]
pub struct TachiBridgeClient {
    port: Arc<dyn TachiTaskBridge>,
    /// Per-watched-task last-seen event cursor (in-memory; NOT a ledger).
    cursors: Arc<Mutex<BTreeMap<String, u64>>>,
}

impl TachiBridgeClient {
    /// Bind the client to a transport port implementation.
    pub fn new(port: Arc<dyn TachiTaskBridge>) -> Self {
        Self {
            port,
            cursors: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Submit once. The encode-side admission scan ALWAYS runs here —
    /// even for a programmatically constructed intent that bypassed
    /// [`super::compose::compose_intent`] — so no submit path can carry
    /// forbidden content to a transport (TB-4; fail closed locally,
    /// never fail open). The typed host receipts pass through otherwise
    /// unchanged.
    pub async fn submit(
        &self,
        intent: &TaskIntentV1,
        request_id: &RequestId,
    ) -> Result<SubmitReceipt, SubmitTransportError> {
        if let Err(rejection) = scan_intent(intent).and_then(|()| scan_client_authored_refs(intent))
        {
            return Ok(SubmitReceipt::Rejected {
                reason: rejection.to_string(),
            });
        }
        self.port.submit(intent, request_id).await
    }

    /// Submit with the TB-7 rule-4 law: if the response is lost before
    /// observation, REPLAY THE SAME `(intent, request_id)` — bounded
    /// attempts — and never invent a new request id. A replay that
    /// observes the binding reconciles to exactly one task; if every
    /// attempt is ambiguous, surface the host's typed
    /// `ReconciliationUnknown`/conflict receipt when one arrives, else
    /// the transport error as the caller's signal to keep replaying the
    /// SAME tuple later.
    pub async fn submit_reconciling(
        &self,
        intent: &TaskIntentV1,
        request_id: &RequestId,
    ) -> Result<SubmitReceipt, SubmitTransportError> {
        if let Err(rejection) = scan_intent(intent).and_then(|()| scan_client_authored_refs(intent))
        {
            return Ok(SubmitReceipt::Rejected {
                reason: rejection.to_string(),
            });
        }
        let mut last_err = None;
        for _ in 0..SUBMIT_RECONCILE_ATTEMPTS {
            match self.port.submit(intent, request_id).await {
                Ok(receipt) => return Ok(receipt),
                Err(error) => last_err = Some(error),
            }
        }
        Err(last_err.unwrap_or(SubmitTransportError))
    }

    /// `get(task_ref)` (TB-8).
    pub async fn get(&self, task_ref: &TaskRef) -> Result<TaskSnapshotView, BridgeQueryError> {
        self.port.get(task_ref).await
    }

    /// Watch from the task's in-memory last-seen cursor (first watch
    /// starts from seq 0 — full history backfill, TB-9). The cursor
    /// advances only after a successfully observed page, and only
    /// MONOTONICALLY: a slower in-flight response carrying an older page
    /// can never regress the cursor below a sequence a faster response
    /// already recorded (TB-9 reconnect truth). Concurrent watch calls
    /// each observe from the cursor at their start — overlap between
    /// concurrent pages is duplicate delivery, tolerated by the TB-9
    /// law and deterministically suppressible by `(seq, event_id)`.
    pub async fn watch_new_events(
        &self,
        task_ref: &TaskRef,
        limit: usize,
    ) -> Result<TaskEventPageView, BridgeQueryError> {
        let after_seq = *self.cursors.lock().get(task_ref.as_wire()).unwrap_or(&0);
        let page = self.port.watch(task_ref, after_seq, limit).await?;
        if let Some(last) = page.events.last() {
            let mut cursors = self.cursors.lock();
            let cursor = cursors.entry(task_ref.as_wire().to_string()).or_insert(0);
            if last.seq > *cursor {
                *cursor = last.seq;
            }
        }
        Ok(page)
    }

    /// The last-seen cursor for a watched task (observability).
    pub fn cursor(&self, task_ref: &TaskRef) -> u64 {
        *self.cursors.lock().get(task_ref.as_wire()).unwrap_or(&0)
    }

    /// `collect` the latest result revision (TB-13: newer wins).
    pub async fn collect_latest(
        &self,
        task_ref: &TaskRef,
    ) -> Result<ResultProjectionView, BridgeQueryError> {
        self.port.collect(task_ref, None).await
    }

    /// `collect` exactly the pinned revision, or typed
    /// `ResultRevisionNotFound` (TB-13).
    pub async fn collect_pinned(
        &self,
        task_ref: &TaskRef,
        result_revision: u64,
    ) -> Result<ResultProjectionView, BridgeQueryError> {
        self.port.collect(task_ref, Some(result_revision)).await
    }

    /// `intervene` with the encode-side admission law applied first:
    /// every text-bearing intervention field is content-scanned (TB-4,
    /// the same fail-closed mirror as submit) BEFORE the port is
    /// touched, so no intervention path can carry forbidden content to a
    /// transport. Typed host receipts/refusals pass through unchanged
    /// (TB-11: `unsupported_by_lifecycle_owner` /
    /// `requires_new_task_lineage` are TRUTH, not errors to route
    /// around).
    pub async fn intervene(
        &self,
        task_ref: &TaskRef,
        intervention: &InterventionV1,
        requester: &RequesterRef,
        request_id: &RequestId,
        expected_task_revision: Option<u64>,
    ) -> Result<InterventionReceipt, InterventionError> {
        if let Err(rejection) = scan_intervention_texts(intervention) {
            return Err(rejection);
        }
        self.port
            .intervene(
                task_ref,
                intervention,
                requester,
                request_id,
                expected_task_revision,
            )
            .await
    }

    /// `request_stop` (TB-12): the reason text is content-scanned first,
    /// then the port is called. The receipt is the multi-stage stop
    /// fact — the client never converts it into a `cancelled`
    /// projection.
    pub async fn request_stop(
        &self,
        task_ref: &TaskRef,
        mode: StopMode,
        reason: &str,
        requester: &RequesterRef,
        request_id: &RequestId,
        expected_task_revision: Option<u64>,
    ) -> Result<StopReceipt, InterventionError> {
        let reason = BoundedText::new(reason).map_err(|_| InterventionError::ForbiddenContent {
            category: "oversize text".to_string(),
            field: "stop_reason".to_string(),
        })?;
        let intervention = match mode {
            StopMode::Graceful => InterventionV1::RequestGracefulStop {
                reason: reason.clone(),
            },
            StopMode::Hard => InterventionV1::RequestHardCancel { reason },
        };
        let receipt = self
            .intervene(
                task_ref,
                &intervention,
                requester,
                request_id,
                expected_task_revision,
            )
            .await?;
        match receipt {
            InterventionReceipt::Stop(stop) => Ok(stop),
            // The port maps stop aliases to the single stop authority
            // (TB-11); any other shape is a transport contract break —
            // surfaced as a typed unavailable rather than guessed.
            other => {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "task_ref": task_ref.as_wire(),
                            "receipt": format!("{other:?}"),
                        })),
                    "tachi_bridge: stop-alias intervention did not resolve to the stop authority; refusing to guess"
                );
                Err(InterventionError::Unavailable)
            }
        }
    }

    /// The supervisor-gated intervention surface (vertical V3, SA-29):
    /// every operation is checked against the session's granted
    /// authority set BEFORE anything is sent, and the supervisor
    /// vocabulary structurally cannot express `RequestPause` /
    /// `RequestResume` / `Escalate` (outside the SA-29 set — see
    /// `InterventionStatic::supervisor_authority`).
    pub async fn supervisor_intervene(
        &self,
        granted: &BTreeSet<SupervisorAuthority>,
        op: SupervisorIntervention,
        task_ref: &TaskRef,
        requester: &RequesterRef,
        request_id: &RequestId,
        expected_task_revision: Option<u64>,
    ) -> Result<InterventionReceipt, SupervisorInterventionError> {
        if !granted.contains(&op.required_authority()) {
            return Err(SupervisorInterventionError::AuthorityNotGranted {
                authority: op.required_authority(),
            });
        }
        self.intervene(
            task_ref,
            &op.to_wire(),
            requester,
            request_id,
            expected_task_revision,
        )
        .await
        .map_err(SupervisorInterventionError::Host)
    }
}

// ─────────────────────────────────────────────────────────────────────────
// The supervisor intervention vocabulary (vertical V3, SA-29 gate)
// ─────────────────────────────────────────────────────────────────────────

/// A supervisor-session intervention: the six SESSION-side TB-11
/// operations that correspond one-to-one with SA-29 authorities. The
/// other SA-29 authorities are not interventions at all:
/// `ObserveTask`/`ReadResultRefs` are the read ops (`get`/`watch`/
/// `collect`), `RequestIndependentReview` maps to a NEW task submission
/// (see `supervisor_v1`), and `ProposeJudgment` is a run-scoped
/// proposal. `RequestPause`/`RequestResume`/`Escalate` are outside the
/// Supervisor grant set and are UNREPRESENTABLE in this type — no
/// variant exists for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorIntervention {
    /// Provide additional context (authority: `ProvideContext`).
    ProvideContext {
        /// The context note (content, scanned).
        note: BoundedText,
    },
    /// Request a correction (authority: `RequestCorrection`).
    RequestCorrection {
        /// What to correct (content, scanned).
        note: BoundedText,
    },
    /// Request continuation (authority: `RequestContinuation`). NEVER
    /// independent review — TB-17: the receipt this produces is a
    /// continuation fact and can never satisfy an independence-marked
    /// requirement.
    RequestContinuation {
        /// Continuation note (content, scanned).
        note: BoundedText,
    },
    /// Request user input (authority: `RequestUserInput`; the PARENT
    /// owns asking and wording — SA-25).
    RequestUserInput {
        /// The question to surface (content, scanned).
        prompt: BoundedText,
    },
    /// Request a graceful stop (authority: `RequestGracefulStop`).
    RequestGracefulStop {
        /// Why (content, scanned).
        reason: BoundedText,
    },
    /// Request a hard cancel (authority: `RequestCancel`).
    RequestHardCancel {
        /// Why (content, scanned).
        reason: BoundedText,
    },
}

impl SupervisorIntervention {
    /// The SA-29 authority this operation requires.
    #[must_use]
    pub fn required_authority(&self) -> SupervisorAuthority {
        match self {
            Self::ProvideContext { .. } => SupervisorAuthority::ProvideContext,
            Self::RequestCorrection { .. } => SupervisorAuthority::RequestCorrection,
            Self::RequestContinuation { .. } => SupervisorAuthority::RequestContinuation,
            Self::RequestUserInput { .. } => SupervisorAuthority::RequestUserInput,
            Self::RequestGracefulStop { .. } => SupervisorAuthority::RequestGracefulStop,
            Self::RequestHardCancel { .. } => SupervisorAuthority::RequestCancel,
        }
    }

    /// The wire form (TB-11 `InterventionV1` mirror).
    #[must_use]
    pub fn to_wire(&self) -> InterventionV1 {
        match self {
            Self::ProvideContext { note } => {
                InterventionV1::ProvideAdditionalContext { note: note.clone() }
            }
            Self::RequestCorrection { note } => {
                InterventionV1::RequestCorrection { note: note.clone() }
            }
            Self::RequestContinuation { note } => {
                InterventionV1::RequestContinuation { note: note.clone() }
            }
            Self::RequestUserInput { prompt } => InterventionV1::RequestUserInput {
                prompt: prompt.clone(),
            },
            Self::RequestGracefulStop { reason } => InterventionV1::RequestGracefulStop {
                reason: reason.clone(),
            },
            Self::RequestHardCancel { reason } => InterventionV1::RequestHardCancel {
                reason: reason.clone(),
            },
        }
    }
}

/// Typed failure of a supervisor-gated intervention.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SupervisorInterventionError {
    /// The session's admitted authority set does not include the
    /// operation (SA-29: the typed set is the authority; anything
    /// outside it is refused before any transport call).
    #[error("supervisor authority not granted: {authority:?}")]
    AuthorityNotGranted { authority: SupervisorAuthority },
    /// A text-bearing field matched a forbidden-content category
    /// (TB-4 fail-closed mirror).
    #[error("{0}")]
    Forbidden(#[from] InterventionError),
    /// The host refused or failed the intervention (typed truth —
    /// `unsupported_by_lifecycle_owner`, `requires_new_task_lineage`,
    /// revision conflicts, outages — never routed around).
    #[error("{0}")]
    Host(InterventionError),
}

/// Scan every text-bearing field of an intervention against the
/// mirrored TB-4 categories (the same fail-closed law submit applies,
/// with the client-side watershed superset on top).
fn scan_intervention_texts(intervention: &InterventionV1) -> Result<(), InterventionError> {
    let scan = |field: &'static str, text: &BoundedText| {
        scan_text(field, text).map_err(|rejection: ComposeRejection| {
            let ComposeRejection::ForbiddenContent { category, field } = rejection else {
                return InterventionError::ForbiddenContent {
                    category: "rejected text".to_string(),
                    field: field.to_string(),
                };
            };
            InterventionError::ForbiddenContent {
                category: category.to_string().replace(char::is_whitespace, "_"),
                field: field.to_string(),
            }
        })
    };
    match intervention {
        InterventionV1::ProvideAdditionalContext { note } => scan("intervention.note", note),
        InterventionV1::RequestCorrection { note } => scan("intervention.note", note),
        InterventionV1::RequestContinuation { note } => scan("intervention.note", note),
        InterventionV1::RequestUserInput { prompt } => scan("intervention.prompt", prompt),
        InterventionV1::RequestGracefulStop { reason }
        | InterventionV1::RequestHardCancel { reason } => scan("intervention.reason", reason),
        InterventionV1::RequestIndependentReview { .. } => Ok(()),
        InterventionV1::RequestPause | InterventionV1::RequestResume => Ok(()),
        InterventionV1::Escalate { reason } => scan("intervention.reason", reason),
    }
}

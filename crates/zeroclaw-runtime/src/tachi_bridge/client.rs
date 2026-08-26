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

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use zeroclaw_api::taskintent::{AttemptRef, RequestId, TaskIntentV1, TaskRef};

use super::compose::{ComposeRejection, scan_intent};

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
/// production transport implements. Exactly four operations (owner
/// scope): no intervene/request_stop client surface exists in this leaf.
///
/// Implementations are Tachi-side truth: they mint `TaskRef` values,
/// derive lifecycle state through the bridge's own TB-16 mapping tables,
/// and never fall back to local execution. The ZeroClaw side constructs
/// no refs and executes no work through this seam.
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
        if let Err(rejection) = scan_intent(intent) {
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
        if let Err(rejection) = scan_intent(intent) {
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
    /// advances only after a successfully observed page.
    pub async fn watch_new_events(
        &self,
        task_ref: &TaskRef,
        limit: usize,
    ) -> Result<TaskEventPageView, BridgeQueryError> {
        let after_seq = *self.cursors.lock().get(task_ref.as_wire()).unwrap_or(&0);
        let page = self.port.watch(task_ref, after_seq, limit).await?;
        if let Some(last) = page.events.last() {
            self.cursors
                .lock()
                .insert(task_ref.as_wire().to_string(), last.seq);
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
}

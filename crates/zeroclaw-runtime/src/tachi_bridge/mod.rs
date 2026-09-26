//! Tachi bridge — the ZeroClaw CLIENT half of delegation (ADR-017).
//!
//! Two surfaces live here while the bridge migrates:
//!
//! - [`staff`] — the production client of Tachi's `tachi_staff` MCP tool
//!   (start / status / result / cancel over the MCP HTTP transport). This is
//!   the surface a real Tachi daemon serves; see ADR-017 "ZeroClaw ↔
//!   tachi_staff mapping".
//! - the TaskIntent port below (vertical V2b; frozen contract rev 3). No
//!   Tachi build serves it. It stays only because `procedure_v1` and
//!   `supervisor_v1` still compile against it, and leaves with them (#381
//!   PR D).
//!
//! ```text
//! Parent
//!   → TaskIntentV1     composed: five task-specific values + policy      (compose)
//!   → submit(intent, request_id) → SubmitReceipt → TaskRef              (TB-5b/TB-6/TB-7)
//!   → get(task_ref)               → TaskSnapshotView                    (TB-8/TB-16)
//!   → watch(task_ref, after_seq)  → TaskEventPageView                   (TB-9)
//!   → collect(task_ref)           → ResultProjectionView                (TB-13)
//!   → Parent receives: TaskRef, AttemptRef, Artifact, Evidence,
//!     ResultProjection — refs, never relay prose
//! ```
//!
//! Authority boundaries encoded here:
//!
//! - **The Parent-side expression surface is exactly five values**
//!   (DoD row 2): objective, `capability_request`, constraints,
//!   expected_artifacts, evaluation_requirement ([`compose`]). The
//!   authority-bearing subset (`capability_request`, `workspace_source`,
//!   `routing_preference`, `approval_requirement`) is filled from the
//!   requester's own admitted policy, independent of bundle/guidance
//!   content (TB-4 seam law).
//! - **ZeroClaw never names an implementation** (TB-1/TB-4/TB-5): the
//!   wire admits no execution-detail FIELD under any name (schema
//!   admission), the capability
//!   vocabulary is a closed enum, and the encode-side admission scan
//!   rejects forbidden content per category before anything is sent.
//! - **No second task ledger** (TB-1/TB-22): this module owns no DDL,
//!   opens no database, and writes no durable state of any kind. Watch
//!   cursors are process-lifetime in-memory only; the TB-19 restart
//!   vertical is Batch 4 and deliberately not built here.
//! - **Fail closed, no fallback** (TB-20): on Tachi outage the client
//!   returns typed `Unavailable` and there is NO local execution path —
//!   this module holds no process/command capability at all (source-scan
//!   test below).
//! - **Scope (owner-specified)**: submit / get / watch / collect (V2b)
//!   plus intervene / request_stop (vertical V3, TB-11/TB-12 — the
//!   supervisor-gated entry point is
//!   `TachiBridgeClient::supervisor_intervene`). No requester-restart
//!   delivery (tachi durable delivery not landed). No production
//!   transport ships in this crate yet: the binding point is the
//!   `TachiTaskBridge` port; the live stage-B harness implements it
//!   over HTTP against the real tachi host, and the parent-runtime
//!   wiring is the V4 leaf.

pub mod client;
pub mod compose;
pub mod procedure;
pub mod staff;

/// Test doubles for the [`TachiTaskBridge`] port — STRICTLY test-only
/// (TB-22): the in-memory bridge is structurally a task/status ledger
/// (tuple bindings, fact log, counters) and must never be constructible
/// from production code, where it would be exactly the second task
/// ledger the freeze forbids. The live host is a transport-backed
/// implementation of the same port, not this module. `pub(crate)` under
/// `cfg(test)` so sibling crates' test modules (supervisor_v1) can bind
/// to it; it never exists in production builds.
#[cfg(test)]
pub(crate) mod in_memory;

#[cfg(test)]
mod staff_tests;
#[cfg(test)]
mod tests;

pub use client::{
    BridgeQueryError, ProjectedAdjudicationState, ProjectedDeliveryState, ProjectedExecutionState,
    ResultProjectionView, SubmitReceipt, SubmitRejection, SubmitTransportError,
    SupervisorIntervention, SupervisorInterventionError, TachiBridgeClient, TachiTaskBridge,
    TaskEventPageView, TaskEventView, TaskSnapshotView, VerificationSummaryView,
};
pub use compose::{
    ComposeError, ComposeRejection, ForbiddenCategory, RequesterBridgePolicy,
    StructuralIntentContext, TaskIntentInputs, compose_intent, scan_client_authored_refs,
    scan_intent, scan_text,
};
pub use procedure::ProcedureSubmitPort;
pub use staff::{
    CancelOutcome, CancelReceipt, RunResult, RunState, RunStatus, StaffReceipt, StaffRefs,
    StaffingReason, TachiStaffClient, TachiStaffError, TachiStaffSettings,
};

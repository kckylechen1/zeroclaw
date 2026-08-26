//! Tachi TaskIntent bridge — the ZeroClaw CLIENT half (vertical V2b of
//! the gated-open program; frozen contract rev 3; host half = the tachi
//! TaskIntent bridge, vertical V2a).
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
//!   wire admits no execution detail under any name, the capability
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
//! - **Scope (owner-specified)**: submit / get / watch / collect ONLY.
//!   No intervene/request_stop surface exists on this client (V3 leaf);
//!   no requester-restart delivery (tachi durable delivery not landed).

pub mod client;
pub mod compose;
pub mod in_memory;

#[cfg(test)]
mod tests;

pub use client::{
    BridgeQueryError, ProjectedAdjudicationState, ProjectedDeliveryState, ProjectedExecutionState,
    ResultProjectionView, SubmitReceipt, SubmitRejection, SubmitTransportError, TachiBridgeClient,
    TachiTaskBridge, TaskEventPageView, TaskEventView, TaskSnapshotView, VerificationSummaryView,
};
pub use compose::{
    ComposeError, ComposeRejection, ForbiddenCategory, RequesterBridgePolicy,
    StructuralIntentContext, TaskIntentInputs, compose_intent, scan_intent, scan_text,
};
pub use in_memory::{AmbiguousSubmitOnce, InMemoryTachiTaskBridge, UnavailableTachiTaskBridge};

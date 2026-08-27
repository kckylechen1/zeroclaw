//! Procedure vertical V4 — the ZeroClaw half of the Tachi-owned
//! ProcedureRun (frozen contracts #205/#207 rev 3; ticket #236).
//!
//! ```text
//! existing SOP package (<sops_dir>/<name>/SOP.toml + SOP.md)
//!   → ProcedureDefinitionV1          ZeroClaw-owned definition half   (KP-10)
//!   → immutable ProcedureSnapshotV1  content-addressed, minted from a
//!                                    PUBLISHED revision               (KP-11)
//!   → Tachi ProcedureRun             run side owned by Tachi, submitted
//!                                    through the bridge carrier       (KP-13/15)
//!   → Attempt / Artifact / Evidence / Eval   Tachi-owned              (KP-21)
//!   → projection back via get/watch/collect refs                      (TB-8/9/13)
//!   → LearningCandidateV1 into the reviewed promotion path            (KP-18/19)
//! ```
//!
//! What this module deliberately is NOT:
//!
//! - **No legacy coupling**: nothing here constructs the legacy SOP
//!   engine, run store, approval broker, or audit mirror. The legacy
//!   engine stays untouched (#197 deletes it after V4 green); the only
//!   legacy surface reused is the pure definition parsing
//!   (`SopManifest`, `parse_steps`) — authored-format reading, not the
//!   run engine.
//! - **No durable run state** (KP-16): this module opens no database,
//!   creates no directory, and writes no file. The definition capture
//!   READS the definitions tree; the snapshot bytes exist in memory and
//!   in the submit envelope (DECISION KP-16/E option (b)); every
//!   durable copy is Tachi-side. Run truth comes back through bridge
//!   refs only.
//! - **No second eval/adjudication ledger** (KP-21): the driver
//!   consumes adjudication state exclusively from Tachi projections.
//! - **No apply path for candidates** (KP-18): learning output is
//!   candidate-only and routes into the existing reviewed-promotion
//!   surface; no method dispatches an apply.

pub mod definition;
pub mod run;
pub mod snapshot;

#[cfg(test)]
mod tests;

pub use definition::{CapturedDefinition, capture_definition};
pub use run::{
    ProcedureRunClient, ProcedureRunDriverOutput, ProcedureSubmitError, derive_learning_candidate,
    derive_request_id,
};
pub use snapshot::{
    SnapshotContentCategory, SnapshotMintError, mint_snapshot, snapshot_content_scan,
};

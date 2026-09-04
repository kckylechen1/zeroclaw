//! The procedure-run submit carrier (vertical V4; DECISION KP-16/E
//! option (b) — embedded bounded payload). This is a TRANSPORT
//! ATTACHMENT extension of the bridge, deliberately NOT a seventh
//! operation on [`TachiTaskBridge`]: the owner-scoped base port stays
//! frozen at submit/get/watch/collect (+ intervene/request_stop from
//! V3). A transport that can carry procedure runs implements this
//! extension trait next to the base port.
//!
//! Carrier law (the DECISION KP-16/E ratified common denominator):
//!
//! - **Verify-before-ack**: the implementation MUST persist the
//!   snapshot bytes (its own content-addressed store) and byte-verify
//!   the canonical digest BEFORE acknowledging run creation — there is
//!   no acknowledge-then-fetch window on any conforming transport.
//! - **The retained ref never resolves through the mutable definitions
//!   directory**: implementations hold the bytes (or a CAS ref into
//!   their own immutable store), never a path into ZeroClaw's
//!   definitions tree.
//! - **The intent still rides the frozen wire unchanged**: the snapshot
//!   is envelope content beside `(intent, request_id)`, never a
//!   `TaskIntentV1` field; the intent's `context_bundle_ref` carries
//!   the `proceduresnap:<digest>` CAS ref.
//! - **ZeroClaw-side**: the client half of this seam (see
//!   `procedure_v1::run`) writes no durable state — the snapshot bytes
//!   exist at admission handoff only (in the envelope), and every
//!   durable copy is Tachi-side.

use async_trait::async_trait;
use zeroclaw_api::procedure_v1::ProcedureSnapshotV1;
use zeroclaw_api::taskintent::{RequestId, TaskIntentV1};

use super::client::{SubmitReceipt, SubmitTransportError};

/// The DECISION KP-16/E option-(b) carrier port: transports that accept
/// procedure-run submissions implement this beside
/// [`super::TachiTaskBridge`]. Implementations are Tachi-side truth:
/// they verify + retain the snapshot before acknowledging, mint the
/// task through the same admission law as every other submit, and never
/// resolve the retained ref against a ZeroClaw definitions directory.
#[async_trait]
pub trait ProcedureSubmitPort: Send + Sync {
    /// Submit a procedure run: `submit` semantics (TB-5b/TB-6/TB-7) with
    /// the immutable snapshot attached in the envelope. The receipt is
    /// the same typed envelope as base submit.
    async fn submit_procedure_run(
        &self,
        intent: &TaskIntentV1,
        request_id: &RequestId,
        snapshot: &ProcedureSnapshotV1,
    ) -> Result<SubmitReceipt, SubmitTransportError>;

    /// The Tachi-retained snapshot for a content-addressed ref, if this
    /// transport still holds it (read-side projection for audit /
    /// rehydration checks; KP-13 — Tachi truth while ZeroClaw is
    /// offline). Returns the retained bytes' digest match, never a
    /// filesystem path.
    async fn retained_snapshot(
        &self,
        snapshot_ref: &str,
    ) -> Result<Option<ProcedureSnapshotV1>, SubmitTransportError>;
}

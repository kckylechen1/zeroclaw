//! The Tachi fact-reporting path — the [`SessionFactSink`] port through
//! which an ephemeral ExecutionSubAgent run reports authoritative session
//! facts to the tachi attached-session receipt spine (the attached-session receipt spine: `ingest_session_event`, `advertise_session_capabilities`,
//! `request_intervention` pickup, `record_intervention_result`,
//! `mark_session_connection`, `reconnect_session`, `get_session_state`).
//!
//! ```text
//! SessionController (host lifecycle)         SessionFactSink (THIS FILE)
//!   start/watch/prompt/stop/collect  ──facts──▶ attach / advertise /
//!   (spawn/wait/cancel stay HERE,             ingest_event / record_result /
//!    host-owned)                              mark_connection / reconnect /
//!                                             get_state / get_intervention
//!                                          ──▶ tachi spine (receipts only)
//! ```
//!
//! Law recap encoded here:
//!
//! - **Receipts only.** The port's surface has no spawn/wait/cancel/resume
//!   operation and no session handle: the sink can record, read, and
//!   reconnect-bind facts; it can never signal or reap a process. The
//!   source-scan test in this module's tests pins the absence of any
//!   process/filesystem capability across the module.
//! - **No new durable store.** This module owns no DDL and opens no
//!   database; the spine is tachi-owned. The in-memory double is
//!   `#[cfg(test)]`-gated for the same reason the tachi_bridge's
//!   in-memory ledger is (it is structurally a fact ledger).
//! - **Fail closed.** [`SessionFactError::Unavailable`] never degrades to
//!   a local-only path: the run reports the outage and ends `failed` (or
//!   refuses to start when facts cannot flow at all) — the facts ARE the
//!   product here.
//! - **A request is never a state.** Intervention receipts record asks;
//!   only the spine's terminal event facts move canonical state, and a
//!   `cancelled` terminal must bind the authority confirmation reference
//!   recorded via `record_intervention_result`.

use async_trait::async_trait;
use zeroclaw_api::session_exec::{
    AdapterConnectionRef, HostIdentityRef, InterventionRequestIdRef, RemoteSessionRef,
    SessionAdvertiseReceiptView, SessionAttachmentRef, SessionConnectionFactV1, SessionEventIdRef,
    SessionEventKindV1, SessionEventReceiptView, SessionFactError,
    SessionInterventionDispositionV1, SessionInterventionRequestView, SessionReconnectReceiptView,
    SessionStateView, SessionTerminalOutcomeV1,
};

/// The identity/binding facts one session attachment carries. All fields
/// are host-side binding facts; none is a credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionBinding {
    pub host_identity: HostIdentityRef,
    pub adapter_connection: AdapterConnectionRef,
    pub remote_session: RemoteSessionRef,
    /// The host-assigned idempotency key for the attach (stable across
    /// attach replays of the same session binding).
    pub idempotency_key: String,
}

/// One event fact to ingest. `source_revision` is the host's monotone
/// revision for the session's fact stream (the spine's stale-guard rank).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionEventFact {
    pub event_id: SessionEventIdRef,
    pub kind: SessionEventKindV1,
    pub outcome: Option<SessionTerminalOutcomeV1>,
    pub source_revision: u64,
    /// Authority confirmation reference (required by the spine for a
    /// `cancelled` terminal to bind the recorded cancel result).
    pub authority_confirmation_ref: Option<String>,
    /// Bounded summary (spine ceiling: 2000 chars; control-free).
    pub summary: Option<String>,
    /// Payload digest over the bounded projection (hex), when carried.
    pub payload_digest: Option<String>,
}

/// The receipts-only fact sink. Implementations transport these calls to
/// the tachi spine (or, test-only, to an in-memory double); the port
/// itself has zero execution capability.
#[async_trait]
pub trait SessionFactSink: Send + Sync {
    /// Attach the host-owned session to the spine (admission + replay).
    /// `capabilities` are the declared capability names for the binding.
    async fn attach(
        &self,
        binding: &SessionBinding,
        capabilities: &[String],
    ) -> Result<SessionAttachmentRef, SessionFactError>;

    /// Advertise the capability set the host actually supports (the gate
    /// the spine consults for intervention requests).
    async fn advertise_capabilities(
        &self,
        attachment: &SessionAttachmentRef,
        capabilities: &[String],
    ) -> Result<SessionAdvertiseReceiptView, SessionFactError>;

    /// Ingest one authoritative event fact (replay-idempotent by event
    /// id; stale/out-of-order facts are journaled as stale and can never
    /// regress canonical state).
    async fn ingest_event(
        &self,
        attachment: &SessionAttachmentRef,
        fact: &SessionEventFact,
    ) -> Result<SessionEventReceiptView, SessionFactError>;

    /// Pick up one intervention request receipt (an ask issued through
    /// the spine for this attachment). `Ok(None)` when no such request.
    async fn get_intervention(
        &self,
        attachment: &SessionAttachmentRef,
        request_id: &InterventionRequestIdRef,
    ) -> Result<Option<SessionInterventionRequestView>, SessionFactError>;

    /// Record the host's authoritative outcome for one intervention
    /// request. An `Accepted` cancel disposition must carry the harness
    /// confirmation reference.
    async fn record_intervention_result(
        &self,
        attachment: &SessionAttachmentRef,
        request_id: &InterventionRequestIdRef,
        disposition: SessionInterventionDispositionV1,
        authority_confirmation_ref: Option<&str>,
        detail: Option<&str>,
    ) -> Result<(), SessionFactError>;

    /// Report a connection dropout fact (`disconnected` /
    /// `reconnect_failed`). There is no "connected" write: recovery is
    /// the reconnect receipt.
    async fn mark_connection(
        &self,
        attachment: &SessionAttachmentRef,
        fact: SessionConnectionFactV1,
    ) -> Result<(), SessionFactError>;

    /// Reconnect after attachment loss: the spine verifies the full
    /// fresh-claim admission and returns `resume_from_revision` — the
    /// revision the host resumes fact replay from. Canonical state never
    /// regresses across the marker; `unknown_orphaned` is recoverable by
    /// authoritative facts after this.
    async fn reconnect(
        &self,
        binding: &SessionBinding,
    ) -> Result<SessionReconnectReceiptView, SessionFactError>;

    /// Read the canonical state projection.
    async fn get_state(
        &self,
        attachment: &SessionAttachmentRef,
    ) -> Result<SessionStateView, SessionFactError>;
}

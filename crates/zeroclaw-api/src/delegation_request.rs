//! Local ingress request references for Tachi-controlled delegation.
//!
//! These types describe only whether the body has claimed a request and which
//! canonical dispatch it references. Tachi remains the owner of run state.

/// The outcome of atomically claiming an agent-scoped delegation request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegationRequestClaim {
    /// The durable claim was created. Only this outcome permits submission.
    Created,
    /// This exact request was already claimed. `None` means the external
    /// outcome is unresolved; it must be reconciled rather than resubmitted.
    Existing { dispatch_id: Option<String> },
    /// The request ID was already used for a different request digest.
    Conflict,
}

/// A durable local request's reference to canonical Tachi dispatch truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationRequestBinding {
    /// Digest of the admitted request, including its resolved routing choices.
    pub request_digest: String,
    /// A known canonical dispatch ID, or an unresolved external outcome.
    pub dispatch_id: Option<String>,
}

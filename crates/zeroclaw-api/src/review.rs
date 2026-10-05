//! Shared evidence and receipts of owner-governed companion reflection.

/// One owner-authored message in the bounded reflection input.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ReflectionMessage {
    pub session_id: String,
    pub at_unix: u64,
    pub text: String,
}

/// Historical outcome of a weekly reflection. Counts describe this run,
/// rather than the current queue state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SoulReflectionReceipt {
    pub period_from_unix: u64,
    pub messages_read: u64,
    pub proposals_created: u64,
    pub user_model_candidates_created: u64,
    pub outcome: String,
    pub ran_at_unix: u64,
}

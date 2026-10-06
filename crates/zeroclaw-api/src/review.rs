//! Shared evidence and receipts of owner-governed companion reflection.

/// Immutable origin supplied by the trusted message ingress, never inferred
/// from mutable session routing metadata or model-visible content.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UserMessageSource {
    Operator,
    Channel { sender_id: String },
}

/// Canonical input captured at trusted ingress before hooks, media annotations,
/// link previews or model context modify it. Stored atomically with the history
/// row; history content is a derived view and is never reflection evidence.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserMessageIngress {
    pub source: UserMessageSource,
    pub text: String,
}

/// One owner-authored message in the bounded reflection input.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ReflectionMessage {
    pub session_id: String,
    pub at_unix: u64,
    pub text: String,
    pub source: UserMessageSource,
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

/// Current trusted ingress for one front-stage correction. The resolver is
/// scoped to the turn, so operator revocation can be checked at tool use.
#[derive(Debug, Clone)]
pub struct OwnerCorrectionContext {
    pub agent_alias: String,
    pub session_key: String,
    pub ingress: UserMessageIngress,
}

pub type OwnerCorrectionResolver =
    std::sync::Arc<dyn Fn() -> Option<OwnerCorrectionContext> + Send + Sync>;

tokio::task_local! {
    /// Set only by owner-facing ingress; never inherited by spawned workers.
    pub static OWNER_CORRECTION_CONTEXT: OwnerCorrectionResolver;
}

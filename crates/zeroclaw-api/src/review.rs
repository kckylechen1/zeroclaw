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

/// Who set a Soul anchor (ADR-018 §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorSource {
    /// Derived on read from the earliest seed or owner revisions; never stored.
    Default,
    /// The owner chose "keep as anchor"; stored as `owner`.
    Owner,
}

/// A pointer to one Tone revision and one Story revision that the owner marks
/// as "this is the agent I want". Pointers only, never content: the revisions
/// themselves stay the single source of the content.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SoulAnchor {
    /// Row id in `soul_anchors`; `None` for a [`AnchorSource::Default`] anchor.
    pub id: Option<i64>,
    /// Anchored Tone revision; `None` means "no stored heads".
    pub tone_revision_id: Option<u64>,
    /// Anchored Story revision; `None` means "no entries".
    pub story_revision_id: Option<u64>,
    pub set_at_unix: u64,
    pub source: AnchorSource,
}

/// One Tone dial against the anchor. Levels are effective levels (stored
/// heads layered over the configured persona), as `PersonaLevel` names.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DialDrift {
    pub key: String,
    pub anchor_level: String,
    pub current_level: String,
    /// Signed distance from the anchor; positive means the dial moved up.
    pub steps: i8,
}

/// Deterministic summary of how far the current Soul sits from its anchor.
/// No model is involved in producing it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SoulDrift {
    pub anchor: SoulAnchor,
    /// All five Tone dials, in registry order.
    pub dials: Vec<DialDrift>,
    pub story_added: u32,
    pub story_retired: u32,
    pub approved_proposals_since_anchor: u32,
}

impl SoulDrift {
    /// Largest absolute dial distance from the anchor.
    #[must_use]
    pub fn max_abs_steps(&self) -> u8 {
        self.dials
            .iter()
            .map(|dial| dial.steps.unsigned_abs())
            .max()
            .unwrap_or(0)
    }

    /// Whether any dial moved or any Story entry was added or retired.
    /// Approved proposals alone do not count: they may have changed nothing
    /// that is still different from the anchor.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.max_abs_steps() == 0 && self.story_added == 0 && self.story_retired == 0
    }
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
    /// Drift against the anchor when the receipt was written; `None` on
    /// receipts written before ADR-018 §4.3 or when the configured Tone was
    /// unavailable.
    pub drift: Option<SoulDrift>,
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

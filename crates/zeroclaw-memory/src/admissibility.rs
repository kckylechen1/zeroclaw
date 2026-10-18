//! Turn-local read boundaries shared by automatic recall and model-facing tools.
//! Backend filtering is an optimization, not proof of row admissibility.

use zeroclaw_api::{ingress::TurnOrigin, memory_traits::MemoryReadPolicy};

use crate::{MemoryCategory, MemoryEntry};

#[must_use]
pub fn policy_for_turn(origin: TurnOrigin, has_session: bool) -> MemoryReadPolicy {
    match origin {
        TurnOrigin::SubTurn => MemoryReadPolicy::Disabled,
        TurnOrigin::Cron | TurnOrigin::Daemon => MemoryReadPolicy::NonConversation,
        TurnOrigin::Interactive | TurnOrigin::Channel | TurnOrigin::AgentDirect => {
            if has_session {
                MemoryReadPolicy::SessionScoped
            } else {
                MemoryReadPolicy::NonConversation
            }
        }
    }
}

/// Apply after backend reads and before deduplication, reranking, or serialization.
/// Session IDs come only from the active turn, never model-selected filters.
#[must_use]
pub fn entry_is_admissible(
    entry: &MemoryEntry,
    backend: &str,
    sessions: &[String],
    policy: MemoryReadPolicy,
) -> bool {
    if policy == MemoryReadPolicy::Disabled
        || entry.namespace == "soul"
        || entry.key.starts_with("soul::")
        || (matches!(entry.category, MemoryCategory::Conversation)
            && policy != MemoryReadPolicy::SessionScoped)
    {
        return false;
    }
    match entry.session_id.as_deref() {
        Some(source) => !source.trim().is_empty() && sessions.iter().any(|s| s == source),
        None => {
            !matches!(entry.category, MemoryCategory::Conversation)
                // Markdown daily files erase both original category and session.
                // Only Core rows can establish an unscoped durable fact there.
                && (backend != "markdown" || matches!(entry.category, MemoryCategory::Core))
        }
    }
}

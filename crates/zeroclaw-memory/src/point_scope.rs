//! Host scope for point reads/deletes. SQLite and Tachi share these rules;
//! physical keys/paths are backend details, never session authority.

pub(crate) trait PointRow {
    fn is_conversation(&self) -> bool;
    fn session(&self) -> Option<&str>;
    fn agent(&self) -> &str;
    fn namespace(&self) -> &str;
}

/// No task-local context is the historical administrative API. An installed
/// context, even an empty one, cannot see another session's Conversation rows.
/// Secondary compatibility aliases are not scope.
pub(crate) struct PointScope(Option<Option<String>>);

impl PointScope {
    #[cfg(feature = "tachi")]
    pub(crate) fn has_canonical_session(&self) -> bool {
        matches!(&self.0, Some(Some(_)))
    }

    pub(crate) fn current() -> Self {
        Self(
            zeroclaw_api::TOOL_LOOP_MEMORY_SESSIONS
                .try_with(|sessions| {
                    sessions
                        .first()
                        .filter(|sid| !sid.trim().is_empty())
                        .cloned()
                })
                .ok(),
        )
    }

    pub(crate) fn select<T: PointRow>(&self, mut rows: Vec<T>, include_soul: bool) -> Vec<T> {
        rows.retain(|row| include_soul || row.namespace() != crate::SOUL_NAMESPACE);
        if let Some(session) = &self.0 {
            rows.retain(|row| {
                !row.is_conversation()
                    || session
                        .as_deref()
                        .is_some_and(|sid| row.session() == Some(sid))
            });
            // A point key resolves to its Conversation row in this session,
            // falling back to the global key separately for each agent.
            let scoped_agents: std::collections::HashSet<String> = rows
                .iter()
                .filter(|row| row.is_conversation())
                .map(|row| row.agent().to_owned())
                .collect();
            rows.retain(|row| row.is_conversation() || !scoped_agents.contains(row.agent()));
        }
        rows
    }
}

#[cfg(feature = "tachi")]
impl PointRow for crate::traits::MemoryEntry {
    fn is_conversation(&self) -> bool {
        self.category == crate::traits::MemoryCategory::Conversation
    }

    fn session(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    fn agent(&self) -> &str {
        self.agent_id.as_deref().unwrap_or("default")
    }

    fn namespace(&self) -> &str {
        &self.namespace
    }
}

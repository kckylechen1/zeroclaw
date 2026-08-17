//! Typed scopes and applicability for the User Model.
//! A revision's scope decides which turns it may project into;
//! the string form is the durable contract, the enum is the runtime view.

use std::fmt;

/// Where a revision applies. `global` everywhere; the rest only inside
/// their matching context. `task` is intentionally absent from the durable
/// grammar — task-scoped overrides live in the session overlay, never in
/// the durable store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Global,
    Agent(String),
    Channel(String),
    Session(String),
}

impl Scope {
    /// Parse the durable string form. Unknown/malformed scopes are
    /// rejected (write surfaces must not invent grammar).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        if raw.trim() != raw {
            // Surrounding whitespace is a write-surface mistake, not a
            // variant of the grammar — reject instead of canonicalizing.
            return None;
        }
        let raw = raw.trim();
        if raw == "global" {
            return Some(Self::Global);
        }
        let (kind, rest) = raw.split_once(':')?;
        let value = rest.trim();
        if value.is_empty() {
            return None;
        }
        match kind {
            "agent" => Some(Self::Agent(value.to_string())),
            "channel" => Some(Self::Channel(value.to_string())),
            "session" => Some(Self::Session(value.to_string())),
            _ => None,
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Global => write!(f, "global"),
            Self::Agent(v) => write!(f, "agent:{v}"),
            Self::Channel(v) => write!(f, "channel:{v}"),
            Self::Session(v) => write!(f, "session:{v}"),
        }
    }
}

/// The context a turn runs in, used to decide applicability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicabilityContext {
    pub agent_alias: String,
    /// Channel composite key (`channel` or `channel.alias`).
    pub channel: String,
    /// Conversation history key (the session identity for projections).
    pub session: String,
}

impl ApplicabilityContext {
    #[must_use]
    pub fn new(agent_alias: &str, channel: &str, session: &str) -> Self {
        Self {
            agent_alias: agent_alias.to_string(),
            channel: channel.to_string(),
            session: session.to_string(),
        }
    }

    /// Global applies everywhere; scoped revisions apply only when the
    /// context component matches exactly.
    #[must_use]
    pub fn applies(&self, scope: &Scope) -> bool {
        match scope {
            Scope::Global => true,
            Scope::Agent(agent) => *agent == self.agent_alias,
            Scope::Channel(channel) => *channel == self.channel,
            Scope::Session(session) => *session == self.session,
        }
    }

    /// Parse-and-test for the durable string form.
    #[must_use]
    pub fn applies_str(&self, scope: &str) -> bool {
        Scope::parse(scope).is_some_and(|parsed| self.applies(&parsed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ApplicabilityContext {
        ApplicabilityContext::new("main", "telegram.work", "telegram.work/alice")
    }

    #[test]
    fn grammar_roundtrips() {
        for raw in [
            "global",
            "agent:main",
            "channel:telegram.work",
            "session:s1",
        ] {
            assert_eq!(
                Scope::parse(raw).map(|s| s.to_string()),
                Some(raw.to_string())
            );
        }
        for bad in [
            "",
            "global ",
            "task:x",
            "agent:",
            ":",
            "agent",
            "planet:earth",
        ] {
            assert!(Scope::parse(bad).is_none(), "'{bad}' must be rejected");
        }
    }

    #[test]
    fn applicability_matrix() {
        let c = ctx();
        assert!(c.applies_str("global"));
        assert!(c.applies_str("agent:main"));
        assert!(c.applies_str("channel:telegram.work"));
        assert!(c.applies_str("session:telegram.work/alice"));
        assert!(!c.applies_str("agent:other"));
        assert!(!c.applies_str("channel:slack.team"));
        assert!(!c.applies_str("session:elsewhere"));
    }

    /// The reviewer's live leak: a session.trading preference must NOT
    /// project into an unrelated conversation.
    #[test]
    fn scoped_revision_stays_in_its_scope() {
        let trading = ApplicabilityContext::new("main", "telegram.work", "telegram.work/alice");
        let casual = ApplicabilityContext::new("main", "telegram.work", "telegram.work/bob");
        assert!(trading.applies_str("session:telegram.work/alice"));
        assert!(
            !casual.applies_str("session:telegram.work/alice"),
            "a session-scoped revision must not leak into other sessions"
        );
    }
}

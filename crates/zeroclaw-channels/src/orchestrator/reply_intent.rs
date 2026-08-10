//! Channel reply-intent classification types and pure parsers.
//!
//! Extracted from `orchestrator/mod.rs` so the kinded `NO_REPLY[...]`
//! contract can evolve independently of the LLM classifier call and
//! provider-route resolution.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoReplyKind {
    /// "Got it, no action needed" — informational, social, or
    /// non-addressed messages. Reaction: 👍.
    Informational,
    /// "I will not do this" — safety / policy refusals (prompt injection,
    /// blocked tool, disallowed request). Reaction: 🚫.
    Refused,
    /// "I tried but couldn't fulfil" — external failures, missing
    /// resources, timeouts where the assistant gave up. Reaction: ⚠️.
    Failed,
}

impl NoReplyKind {
    pub(crate) fn emoji(self) -> &'static str {
        match self {
            NoReplyKind::Informational => "👍",
            NoReplyKind::Refused => "🚫",
            NoReplyKind::Failed => "⚠️",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AssistantChannelOutcome {
    Reply(String),
    NoReply {
        kind: NoReplyKind,
        reason: Option<String>,
    },
}

impl AssistantChannelOutcome {
    pub(crate) fn history_marker(&self) -> String {
        match self {
            Self::Reply(text) => text.clone(),
            Self::NoReply {
                reason: Some(reason),
                ..
            } if !reason.trim().is_empty() => {
                format!("[No reply sent: {}]", reason.trim())
            }
            Self::NoReply { .. } => "[No reply sent]".to_string(),
        }
    }
}

/// Parse the classifier's raw output into an `AssistantChannelOutcome`. Pure
/// helper extracted so the LLM-call wrapper has no parsing logic and the
/// kinded `NO_REPLY[...]` forms can be unit-tested without a model_provider.
pub(crate) fn parse_reply_intent(response: &str) -> AssistantChannelOutcome {
    let trimmed = response.trim();
    if trimmed.is_empty() {
        return AssistantChannelOutcome::NoReply {
            kind: NoReplyKind::Informational,
            reason: None,
        };
    }
    if trimmed.eq_ignore_ascii_case("REPLY") {
        return AssistantChannelOutcome::Reply(String::new());
    }

    for (tag, kind) in &[
        ("NO_REPLY[INFO]:", NoReplyKind::Informational),
        ("NO_REPLY[REFUSE]:", NoReplyKind::Refused),
        ("NO_REPLY[FAIL]:", NoReplyKind::Failed),
    ] {
        if let Some(reason) = trimmed.strip_prefix(tag) {
            return outcome_for_no_reply(reason.trim(), *kind);
        }
    }

    if let Some(reason) = trimmed.strip_prefix("NO_REPLY:") {
        return outcome_for_no_reply(reason.trim(), NoReplyKind::Informational);
    }
    if trimmed.eq_ignore_ascii_case("NO_REPLY") {
        return AssistantChannelOutcome::NoReply {
            kind: NoReplyKind::Informational,
            reason: None,
        };
    }

    AssistantChannelOutcome::Reply(String::new())
}

pub(crate) fn outcome_for_no_reply(reason: &str, kind: NoReplyKind) -> AssistantChannelOutcome {
    if matches!(kind, NoReplyKind::Informational) && looks_like_meta_instruction_echo(reason) {
        return AssistantChannelOutcome::Reply(String::new());
    }
    AssistantChannelOutcome::NoReply {
        kind,
        reason: (!reason.is_empty()).then(|| reason.to_string()),
    }
}

pub(crate) fn looks_like_meta_instruction_echo(reason: &str) -> bool {
    if reason.is_empty() {
        return false;
    }
    let lower = reason.to_ascii_lowercase();
    const MARKERS: &[&str] = &[
        "classification task",
        "only classify",
        "must not answer",
        "not answering the user",
        "do not answer the user",
        "do not reply to the user",
        "classifier instruction",
    ];
    MARKERS.iter().any(|m| lower.contains(m))
}

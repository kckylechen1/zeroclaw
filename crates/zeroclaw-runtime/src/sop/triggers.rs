//! Pure SOP trigger topic/path/payload matchers.
//!
//! Extracted from `engine.rs` so matching helpers can evolve without touching
//! the run-lifecycle chokepoints. `trigger_source` owns per-source behavior;
//! this module owns the shared string/path matchers those behaviors call.

use super::types::{FilesystemEventKind, SopEvent};
use crate::calendar::{CALENDAR_NO_SHOW_TOPIC, CalendarNoShowEvent};

/// Match a channel trigger against an event topic. Two producer forms are
/// accepted through the shared [`zeroclaw_api::channel::ChannelSopTopic`] grammar:
/// the plain `channel` / `channel/alias` form used by agent-loop message
/// triggers, and the forge form `channel.alias:event_type`. Channel type
/// compares case-insensitively; an aliased trigger requires an exact alias, an
/// alias-less trigger matches any instance. No topic fails closed. The
/// `event_type` (forge form) is left for an authored `condition` to match.
pub(crate) fn channel_trigger_topic_matches(
    channel: &str,
    alias: Option<&str>,
    topic: Option<&str>,
) -> bool {
    let Some(topic) = topic else {
        return false;
    };
    let (topic_channel, topic_alias, _event_type) =
        zeroclaw_api::channel::ChannelSopTopic::parse(topic);
    if !topic_channel.eq_ignore_ascii_case(channel) {
        return false;
    }
    match alias {
        Some(a) => topic_alias.is_some_and(|ta| ta == a),
        None => true,
    }
}

pub(crate) fn calendar_trigger_matches(
    calendar_source: &str,
    calendar_ids: &[String],
    event: &SopEvent,
) -> bool {
    if event.topic.as_deref() != Some(CALENDAR_NO_SHOW_TOPIC) {
        return false;
    }

    let Some(payload) = event.payload.as_deref() else {
        return false;
    };
    let Ok(payload) = serde_json::from_str::<CalendarNoShowEvent>(payload) else {
        return false;
    };

    if payload.calendar_source != calendar_source {
        return false;
    }

    if calendar_ids.is_empty() {
        return true;
    }

    calendar_ids.iter().any(|id| id == &payload.calendar_id)
}

/// Simple MQTT topic matching with `+` (single-level) and `#` (multi-level) wildcards.
pub(crate) fn mqtt_topic_matches(pattern: &str, topic: &str) -> bool {
    let pat_parts: Vec<&str> = pattern.split('/').collect();
    let top_parts: Vec<&str> = topic.split('/').collect();

    let mut pi = 0;
    let mut ti = 0;

    while pi < pat_parts.len() && ti < top_parts.len() {
        match pat_parts[pi] {
            "#" => return true, // multi-level wildcard matches everything remaining
            "+" => {
                // single-level wildcard matches one segment
                pi += 1;
                ti += 1;
            }
            seg => {
                if seg != top_parts[ti] {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
        }
    }

    // Both must be fully consumed (unless pattern ended with #)
    pi == pat_parts.len() && ti == top_parts.len()
}

/// AMQP topic-exchange routing-key matching. Keys are `.`-delimited words;
/// `*` matches exactly one word and `#` matches zero or more words. A `#` that
/// can absorb zero segments is what distinguishes this from MQTT matching.
pub(crate) fn amqp_routing_key_matches(pattern: &str, key: &str) -> bool {
    let pat: Vec<&str> = pattern.split('.').collect();
    let words: Vec<&str> = key.split('.').collect();
    amqp_match_from(&pat, &words)
}

fn amqp_match_from(pat: &[&str], words: &[&str]) -> bool {
    match pat.first() {
        None => words.is_empty(),
        Some(&"#") => (0..=words.len()).any(|skip| amqp_match_from(&pat[1..], &words[skip..])),
        Some(&"*") => !words.is_empty() && amqp_match_from(&pat[1..], &words[1..]),
        Some(seg) => {
            !words.is_empty() && *seg == words[0] && amqp_match_from(&pat[1..], &words[1..])
        }
    }
}

/// Glob match a filesystem trigger `pattern` against a normalized `path`,
/// supporting `*` (single segment) and `**` (recursive) wildcards via the
/// `glob` crate. A bare directory pattern also matches paths nested beneath it.
pub(crate) fn filesystem_path_matches(pattern: &str, path: &str) -> bool {
    if let Ok(compiled) = glob::Pattern::new(pattern)
        && compiled.matches(path)
    {
        return true;
    }
    let prefix = pattern.trim_end_matches('/');
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

/// Whether the payload's `event` field names one of the trigger's listed kinds.
pub(crate) fn filesystem_event_listed(
    events: &[FilesystemEventKind],
    payload: Option<&str>,
) -> bool {
    let Some(payload) = payload else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
        return false;
    };
    let Some(kind) = value.get("event").and_then(|e| e.as_str()) else {
        return false;
    };
    events.iter().any(|e| e.to_string() == kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amqp_routing_key_exact_star_hash() {
        assert!(amqp_routing_key_matches("a.b.c", "a.b.c"));
        assert!(!amqp_routing_key_matches("a.b.c", "a.b"));
        assert!(amqp_routing_key_matches("a.*.c", "a.b.c"));
        assert!(!amqp_routing_key_matches("a.*.c", "a.b.b.c"));
        assert!(amqp_routing_key_matches("a.#", "a.b.c.d"));
        assert!(amqp_routing_key_matches("a.#", "a"));
        assert!(amqp_routing_key_matches("#", ""));
        assert!(amqp_routing_key_matches("a.#.d", "a.d"));
        assert!(amqp_routing_key_matches("a.#.d", "a.b.c.d"));
        assert!(!amqp_routing_key_matches("a.#.d", "a.b.c"));
    }

    #[test]
    fn mqtt_topic_matching_edge_cases() {
        assert!(mqtt_topic_matches("a/b/c", "a/b/c"));
        assert!(!mqtt_topic_matches("a/b/c", "a/b/d"));
        assert!(!mqtt_topic_matches("a/b/c", "a/b"));
        assert!(!mqtt_topic_matches("a/b", "a/b/c"));
        assert!(mqtt_topic_matches("+/+/+", "a/b/c"));
        assert!(!mqtt_topic_matches("+/+", "a/b/c"));
        assert!(mqtt_topic_matches("#", "a/b/c"));
        assert!(mqtt_topic_matches("a/#", "a/b/c"));
        assert!(!mqtt_topic_matches("b/#", "a/b/c"));
    }

    #[test]
    fn channel_trigger_topic_case_and_alias() {
        assert!(channel_trigger_topic_matches(
            "telegram",
            None,
            Some("Telegram")
        ));
        assert!(channel_trigger_topic_matches(
            "telegram",
            None,
            Some("telegram/bot-a")
        ));
        assert!(channel_trigger_topic_matches(
            "telegram",
            Some("bot-a"),
            Some("telegram/bot-a")
        ));
        assert!(!channel_trigger_topic_matches(
            "telegram",
            Some("bot-a"),
            Some("telegram/bot-b")
        ));
        assert!(!channel_trigger_topic_matches("telegram", None, None));
    }

    #[test]
    fn filesystem_path_and_event_listed() {
        assert!(filesystem_path_matches("/data/inbox", "/data/inbox"));
        assert!(filesystem_path_matches(
            "/data/inbox",
            "/data/inbox/file.txt"
        ));
        assert!(!filesystem_path_matches("/data/inbox", "/data/other"));
        assert!(filesystem_path_matches("logs/*.txt", "logs/a.txt"));
        assert!(!filesystem_event_listed(
            &[FilesystemEventKind::Created],
            None
        ));
        assert!(filesystem_event_listed(
            &[FilesystemEventKind::Created],
            Some(r#"{"event":"created"}"#)
        ));
        assert!(!filesystem_event_listed(
            &[FilesystemEventKind::Created],
            Some(r#"{"event":"modified"}"#)
        ));
    }
}

//! Session-scoped task preferences (#51 slice 4).
//!
//! A task-scoped request ("for this task use Codex") is an override, NOT a
//! durable preference: it lives in this in-process overlay keyed by the
//! conversation session, expires with a TTL, and never touches the
//! append-only User Model store (which only owner-authored or
//! owner-ratified revisions may enter).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Task overrides outlive a single turn but never a session's practical
/// lifetime; process restart wipes them by design.
const DEFAULT_TTL: Duration = Duration::from_secs(4 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPref {
    pub kind: &'static str,
    pub statement: String,
    pub expires_at: Instant,
}

#[derive(Default)]
pub struct TaskPreferenceOverlay {
    by_session: Mutex<HashMap<String, HashMap<String, TaskPref>>>,
}

impl TaskPreferenceOverlay {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record (or replace, per semantic key) a task-scoped override for
    /// one conversation session.
    pub fn set(&self, session_key: &str, kind: &'static str, semantic_key: &str, statement: &str) {
        self.set_with_ttl(session_key, kind, semantic_key, statement, DEFAULT_TTL);
    }

    pub fn set_with_ttl(
        &self,
        session_key: &str,
        kind: &'static str,
        semantic_key: &str,
        statement: &str,
        ttl: Duration,
    ) {
        let mut by_session = self.by_session.lock();
        by_session
            .entry(session_key.to_string())
            .or_default()
            .insert(
                semantic_key.to_string(),
                TaskPref {
                    kind,
                    statement: statement.to_string(),
                    expires_at: Instant::now() + ttl,
                },
            );
    }

    /// Non-expired overrides for one session, deterministic order.
    #[must_use]
    pub fn for_session(&self, session_key: &str) -> Vec<TaskPref> {
        let mut by_session = self.by_session.lock();
        let now = Instant::now();
        let empty;
        let mut prefs: Vec<TaskPref> = {
            let Some(session) = by_session.get_mut(session_key) else {
                return Vec::new();
            };
            session.retain(|_, pref| pref.expires_at > now);
            empty = session.is_empty();
            session.values().cloned().collect()
        };
        if empty {
            by_session.remove(session_key);
        }
        prefs.sort_by(|a, b| a.kind.cmp(b.kind).then(a.statement.cmp(&b.statement)));
        prefs
    }

    /// Render the per-session additions to the projected owner-profile
    /// section: task-scoped entries are marked so the model can tell a
    /// temporary instruction from a durable preference.
    #[must_use]
    pub fn render_section(&self, session_key: &str) -> String {
        let prefs = self.for_session(session_key);
        if prefs.is_empty() {
            return String::new();
        }
        let mut section = String::from("Task-scoped for this session only:\n");
        for pref in prefs {
            let _ = std::fmt::Write::write_fmt(
                &mut section,
                format_args!("- [task-scoped] {}: {}\n", pref.kind, pref.statement),
            );
        }
        section
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Discrimination 3 (#51): a task-scoped request stays inside its
    /// session and never appears in an unrelated one.
    #[test]
    fn task_prefs_are_session_scoped() {
        let overlay = TaskPreferenceOverlay::new();
        overlay.set(
            "session-a",
            "preference",
            "model.choice",
            "use Codex for this task",
        );
        assert_eq!(overlay.for_session("session-b").len(), 0);
        let a = overlay.for_session("session-a");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].statement, "use Codex for this task");
    }

    #[test]
    fn task_prefs_expire() {
        let overlay = TaskPreferenceOverlay::new();
        overlay.set_with_ttl(
            "session-a",
            "preference",
            "model.choice",
            "brief",
            Duration::from_millis(0),
        );
        std::thread::sleep(Duration::from_millis(2));
        assert!(overlay.for_session("session-a").is_empty());
    }

    #[test]
    fn semantic_key_replaces_within_session() {
        let overlay = TaskPreferenceOverlay::new();
        overlay.set("session-a", "preference", "model.choice", "first");
        overlay.set("session-a", "preference", "model.choice", "second");
        let a = overlay.for_session("session-a");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].statement, "second");
    }

    #[test]
    fn render_marks_entries_task_scoped() {
        let overlay = TaskPreferenceOverlay::new();
        assert!(overlay.render_section("missing").is_empty());
        overlay.set(
            "session-a",
            "preference",
            "model.choice",
            "use Codex for this task",
        );
        let section = overlay.render_section("session-a");
        assert!(section.contains("[task-scoped]"));
        assert!(section.contains("use Codex for this task"));
    }
}

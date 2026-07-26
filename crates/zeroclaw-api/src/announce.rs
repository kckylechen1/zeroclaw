//! Announcements: what a finished background child tells its parent.
//!
//! A parent agent that dispatches work to several children must not be
//! responsible for remembering to ask whether they are done. Ten workers and a
//! model that has to poll each one is a design that fails the first time the
//! model forgets — and it will. So completion travels the other way: a child
//! reaching a terminal state is announced into its parent's next turn.
//!
//! These are the wire types for that hand-off. They live here, apart from the
//! runtime that produces them, because the set of things that want to *read* a
//! completion is wider than the set that can run one: a fleet console showing
//! "three of your agents finished" needs these shapes and nothing else.
//!
//! ## Every ending is news
//!
//! [`AnnouncedOutcome`] has no variant for "nothing happened". A child that
//! failed, timed out, was cancelled, or was lost to a restart is information
//! the parent needs in order to stop waiting. Silence is the single outcome
//! that must never be delivered, because a parent cannot distinguish it from
//! work still in flight.

use serde::{Deserialize, Serialize};

/// How a child's run ended.
///
/// Mirrors the terminal half of the control plane's task status, deliberately
/// without the in-flight states: a record that is not finished is not an
/// announcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnouncedOutcome {
    /// Ran to completion and produced a result.
    Completed,
    /// Ran and failed. The reason travels in [`Announcement::detail`].
    Failed,
    /// Stopped on request before finishing.
    Cancelled,
    /// Exceeded its deadline.
    TimedOut,
    /// The process that owned it went away before it reported anything. The
    /// work may or may not have happened; nobody can say which, and saying so
    /// is the point.
    Lost,
}

impl AnnouncedOutcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
            Self::Lost => "lost",
        }
    }

    /// Whether the child produced a usable result.
    ///
    /// Everything else is still worth announcing — it just cannot be built on.
    #[must_use]
    pub fn is_success(self) -> bool {
        self == Self::Completed
    }

    /// Whether the outcome leaves the child's actual effect unknown.
    ///
    /// A lost child may have finished its work, or may have died halfway. A
    /// parent must not treat this as either success or a clean failure — the
    /// honest reading is "go and check".
    #[must_use]
    pub fn is_indeterminate(self) -> bool {
        self == Self::Lost
    }
}

/// One finished child, as delivered to its parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Announcement {
    /// The child's task id, so the parent can correlate it with whatever it
    /// dispatched.
    pub task_id: String,
    /// The agent alias that ran, which for a subagent is the parent's own.
    pub agent: String,
    pub outcome: AnnouncedOutcome,
    /// The child's result on success, or `None` when it produced none.
    pub output: Option<String>,
    /// Why it ended badly. Present for every non-success outcome, so a parent
    /// reading only this field still learns something.
    pub detail: Option<String>,
    /// RFC 3339, from the control plane record.
    pub finished_at: Option<String>,
}

impl Announcement {
    /// Render one line for injection into the parent's context.
    ///
    /// Deliberately terse. A parent waking to ten of these should spend its
    /// context on the results, not on framing.
    #[must_use]
    pub fn to_line(&self) -> String {
        let mut line = format!("[{}] {}", self.outcome.as_str(), self.task_id);
        if let Some(detail) = self.detail.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
            line.push_str(": ");
            line.push_str(detail);
        } else if let Some(output) = self.output.as_deref().map(str::trim).filter(|o| !o.is_empty())
        {
            line.push_str(": ");
            line.push_str(output);
        }
        line
    }
}

/// Everything claimed for one parent in a single sweep.
///
/// Batched rather than delivered one at a time: a parent with ten workers that
/// all finish together should wake once and see ten results, not wake ten
/// times. Each wake is a model turn, and turns cost money and actions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnouncementBatch {
    pub parent_task_id: String,
    pub announcements: Vec<Announcement>,
}

impl AnnouncementBatch {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.announcements.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.announcements.len()
    }

    /// Whether any child ended in a way the parent cannot build on.
    #[must_use]
    pub fn has_bad_news(&self) -> bool {
        self.announcements.iter().any(|a| !a.outcome.is_success())
    }

    /// The batch as context text, or `None` when there is nothing to say.
    ///
    /// `None` and an empty string are different: the caller must be able to
    /// tell "no news" from "news that renders blank", because only the first
    /// justifies not waking the parent.
    #[must_use]
    pub fn to_context_block(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut out = String::from("## Background tasks finished\n\n");
        for announcement in &self.announcements {
            out.push_str("- ");
            out.push_str(&announcement.to_line());
            out.push('\n');
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn announcement(id: &str, outcome: AnnouncedOutcome) -> Announcement {
        Announcement {
            task_id: id.into(),
            agent: "main".into(),
            outcome,
            output: None,
            detail: None,
            finished_at: None,
        }
    }

    /// No news must be distinguishable from news that renders blank — only the
    /// former justifies leaving the parent asleep.
    #[test]
    fn an_empty_batch_produces_no_context_at_all() {
        assert_eq!(AnnouncementBatch::default().to_context_block(), None);
    }

    #[test]
    fn a_batch_renders_one_line_per_child() {
        let batch = AnnouncementBatch {
            parent_task_id: "mum".into(),
            announcements: vec![
                announcement("a", AnnouncedOutcome::Completed),
                announcement("b", AnnouncedOutcome::Failed),
            ],
        };
        let block = batch.to_context_block().expect("two children");
        assert_eq!(block.lines().filter(|l| l.starts_with("- ")).count(), 2);
        assert!(block.contains("[completed] a"), "{block}");
        assert!(block.contains("[failed] b"), "{block}");
    }

    /// A failure must carry its reason even when there is no output, otherwise
    /// the parent learns only that something went wrong.
    #[test]
    fn a_failure_announces_its_reason() {
        let mut failed = announcement("b", AnnouncedOutcome::Failed);
        failed.detail = Some("provider refused the request".into());
        assert_eq!(
            failed.to_line(),
            "[failed] b: provider refused the request"
        );
    }

    #[test]
    fn a_success_announces_its_output() {
        let mut done = announcement("a", AnnouncedOutcome::Completed);
        done.output = Some("42".into());
        assert_eq!(done.to_line(), "[completed] a: 42");
    }

    /// A child with neither output nor detail still announces — the parent
    /// needs to know it stopped waiting on it.
    #[test]
    fn a_bare_outcome_still_announces() {
        assert_eq!(
            announcement("c", AnnouncedOutcome::Cancelled).to_line(),
            "[cancelled] c"
        );
    }

    /// `Lost` is neither success nor a clean failure. Collapsing it into either
    /// would have the parent build on work that may not have happened, or
    /// abandon work that did.
    #[test]
    fn a_lost_child_is_indeterminate_not_failed() {
        let lost = AnnouncedOutcome::Lost;
        assert!(!lost.is_success());
        assert!(lost.is_indeterminate());

        for other in [
            AnnouncedOutcome::Completed,
            AnnouncedOutcome::Failed,
            AnnouncedOutcome::Cancelled,
            AnnouncedOutcome::TimedOut,
        ] {
            assert!(
                !other.is_indeterminate(),
                "{other:?} has a definite meaning"
            );
        }
    }

    #[test]
    fn only_completion_counts_as_success() {
        assert!(AnnouncedOutcome::Completed.is_success());
        for bad in [
            AnnouncedOutcome::Failed,
            AnnouncedOutcome::Cancelled,
            AnnouncedOutcome::TimedOut,
            AnnouncedOutcome::Lost,
        ] {
            assert!(!bad.is_success(), "{bad:?} must not read as success");
        }
    }

    #[test]
    fn a_batch_reports_whether_anything_went_wrong() {
        let good = AnnouncementBatch {
            parent_task_id: "mum".into(),
            announcements: vec![announcement("a", AnnouncedOutcome::Completed)],
        };
        assert!(!good.has_bad_news());

        let mixed = AnnouncementBatch {
            parent_task_id: "mum".into(),
            announcements: vec![
                announcement("a", AnnouncedOutcome::Completed),
                announcement("b", AnnouncedOutcome::TimedOut),
            ],
        };
        assert!(mixed.has_bad_news());
    }

    /// These cross a process boundary, so the wire form has to survive it.
    #[test]
    fn announcements_round_trip_through_json() {
        let batch = AnnouncementBatch {
            parent_task_id: "mum".into(),
            announcements: vec![Announcement {
                task_id: "a".into(),
                agent: "researcher".into(),
                outcome: AnnouncedOutcome::TimedOut,
                output: None,
                detail: Some("exceeded 600s".into()),
                finished_at: Some("2026-07-27T01:00:00Z".into()),
            }],
        };
        let json = serde_json::to_string(&batch).expect("serialize");
        assert!(json.contains("\"timed_out\""), "snake_case on the wire: {json}");
        let back: AnnouncementBatch = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, batch);
    }
}

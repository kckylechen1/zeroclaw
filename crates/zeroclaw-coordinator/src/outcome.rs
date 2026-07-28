// Derived from grok-build (Apache-2.0), revision
// 1adcd1f477870e4a97bacbd6be78c8a3bfbac46d, from
// `.../grok_build/task/types.rs`.
// Copyright 2023-2026 SpaceXAI. Licensed under the Apache License, Version 2.0.
//
// This file was CHANGED by ZeroClaw Labs: upstream expressed a child's ending
// as three booleans on `SubagentResult` (`success` / `cancelled` /
// `backgrounded`) plus a `status()` string. That vocabulary is replaced here by
// a single enum carrying ZeroClaw's own five terminal outcomes.
// See ../LICENSE and ../NOTICE.

//! How a child's run ended.

/// The terminal outcomes of a child run.
///
/// Deliberately identical, variant for variant, to `AnnouncedOutcome` in
/// `zeroclaw-api` and to the terminal half of `TaskStatus` in
/// `zeroclaw-runtime`. This crate does not depend on either yet — that
/// dependency edge belongs to the wiring phase — so the enum is restated here
/// with the same names and the same meanings. A later `From` impl is a
/// one-to-one match with no judgement in it; if writing that impl ever requires
/// a decision, this enum has drifted and is the thing to fix.
///
/// There is no variant for "still running" and none for "nothing happened". A
/// coordinator reply that is not terminal says so with a separate flag (see
/// `ChildResult::backgrounded`), because a parent that cannot tell "in flight"
/// from "finished" cannot tell whether to keep waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChildOutcome {
    /// Ran to completion and produced a result.
    Completed,
    /// Ran and failed. The reason travels in `ChildResult::detail`.
    Failed,
    /// Stopped on request before finishing.
    Cancelled,
    /// Exceeded its deadline.
    ///
    /// The coordinator's foreground budget does not produce this: exceeding it
    /// hands the caller a handle and lets the child keep running. Only a host
    /// that actually kills a child on time does.
    TimedOut,
    /// The process that owned it went away before it reported anything. The
    /// work may or may not have happened; nobody can say which, and saying so
    /// is the point.
    Lost,
}

impl ChildOutcome {
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
    /// Everything else is still worth reporting — it just cannot be built on.
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

#[cfg(test)]
mod tests {
    use super::ChildOutcome;

    #[test]
    fn only_completed_is_success() {
        assert!(ChildOutcome::Completed.is_success());
        for outcome in [
            ChildOutcome::Failed,
            ChildOutcome::Cancelled,
            ChildOutcome::TimedOut,
            ChildOutcome::Lost,
        ] {
            assert!(!outcome.is_success(), "{outcome:?} must not read as success");
        }
    }

    #[test]
    fn only_lost_is_indeterminate() {
        assert!(ChildOutcome::Lost.is_indeterminate());
        for outcome in [
            ChildOutcome::Completed,
            ChildOutcome::Failed,
            ChildOutcome::Cancelled,
            ChildOutcome::TimedOut,
        ] {
            assert!(!outcome.is_indeterminate(), "{outcome:?} is determinate");
        }
    }

    /// The wire spellings are `AnnouncedOutcome::as_str`'s, so a later mapping
    /// cannot silently rename an outcome.
    #[test]
    fn wire_spellings_match_zeroclaw_api() {
        assert_eq!(ChildOutcome::Completed.as_str(), "completed");
        assert_eq!(ChildOutcome::Failed.as_str(), "failed");
        assert_eq!(ChildOutcome::Cancelled.as_str(), "cancelled");
        assert_eq!(ChildOutcome::TimedOut.as_str(), "timed_out");
        assert_eq!(ChildOutcome::Lost.as_str(), "lost");
    }
}

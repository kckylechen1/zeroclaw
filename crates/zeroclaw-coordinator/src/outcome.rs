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
/// `zeroclaw-runtime`. This crate takes the `zeroclaw-api` dependency as of
/// the wiring phase (see the `From` impls below), but `zeroclaw-runtime` is
/// downstream of this crate and stays out of reach — `TaskStatus` is not
/// something this crate is allowed to know about. The enum is restated here,
/// rather than re-exported, so that this crate's internal vocabulary does not
/// shift out from under it if `zeroclaw-api`'s ever does. The `From` impls are
/// a one-to-one match with no judgement in them; if writing one ever requires
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

/// Cross into `zeroclaw-api`'s wire vocabulary.
///
/// Every arm here is a rename, never a translation: see the enum's own doc
/// for why. Both matches are written with no wildcard arm on purpose — that
/// is what makes them total. If either enum gains a variant, the *other*
/// match (the one that does not yet know about it) fails to compile, because
/// there is no `_ =>` to swallow the new case. A test cannot observe that
/// failure (the crate does not build), but it is the stronger guarantee: it
/// fires the moment someone forgets, not the next time someone remembers to
/// run a specific test.
impl From<ChildOutcome> for zeroclaw_api::announce::AnnouncedOutcome {
    fn from(outcome: ChildOutcome) -> Self {
        use zeroclaw_api::announce::AnnouncedOutcome as Announced;
        match outcome {
            ChildOutcome::Completed => Announced::Completed,
            ChildOutcome::Failed => Announced::Failed,
            ChildOutcome::Cancelled => Announced::Cancelled,
            ChildOutcome::TimedOut => Announced::TimedOut,
            ChildOutcome::Lost => Announced::Lost,
        }
    }
}

/// The reverse of the `From` above — see its doc for the totality argument.
impl From<zeroclaw_api::announce::AnnouncedOutcome> for ChildOutcome {
    fn from(outcome: zeroclaw_api::announce::AnnouncedOutcome) -> Self {
        use zeroclaw_api::announce::AnnouncedOutcome as Announced;
        match outcome {
            Announced::Completed => Self::Completed,
            Announced::Failed => Self::Failed,
            Announced::Cancelled => Self::Cancelled,
            Announced::TimedOut => Self::TimedOut,
            Announced::Lost => Self::Lost,
        }
    }
}

// ── What is deliberately NOT here ───────────────────────────────────────────
//
// There is no `From<ChildResult> for zeroclaw_api::announce::Announcement`,
// and there will not be one in this shape.
//
// `Announcement::outcome` converts cleanly (that is the pair of impls above),
// but two of its other fields are not properties of a `ChildResult` at all:
//
// - `agent` is the alias the control-plane row was filed under. A
//   `ChildResult` carries no agent/alias field of its own — only
//   `ChildRequest::agent_type` and `ChildCompletionSummary::agent_type` do,
//   and neither is this type.
// - `finished_at` is an RFC 3339 timestamp "from the control plane record".
//   `ChildResult` has no wall-clock timestamp, only `duration_ms` (elapsed,
//   not absolute). Filling this from `Instant::now()` at conversion time
//   would be inventing a provenance the field's own doc says it does not
//   have — the same lie this crate declines to tell anywhere else.
//
// Both are the control-plane row's properties, not the child result's; the
// row is the thing that knows the alias and the wall-clock. So the honest
// conversion is a two-input function — `(ChildResult, TaskRecord) ->
// Announcement` — not a one-input `From`, and it belongs in the wiring phase
// once both are actually in hand, not here.
//
// The third field, `output: Option<String>` against `ChildResult::output:
// Arc<str>` (always present, never optional), *is* a one-input decision, and
// it is already settled for whoever writes that two-input conversion: empty
// maps to `None`. `Announcement::to_line` already treats empty-after-trim as
// nothing to show, so carrying `Some("")` through would be a distinction its
// only consumer erases anyway.

#[cfg(test)]
mod tests {
    use super::ChildOutcome;
    use zeroclaw_api::announce::AnnouncedOutcome;

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

    /// Every known variant round-trips through `AnnouncedOutcome` and back to
    /// the same variant, with the same wire spelling on both sides.
    ///
    /// This does not prove the `From` impls are total — the exhaustive
    /// matches (no wildcard arm) prove that at compile time, and a variant
    /// added to either enum without the other breaks the build, not this
    /// test. What this test catches is the mistake exhaustiveness cannot: an
    /// existing arm mapped to the *wrong* variant. Change either `From` impl
    /// to map, say, `ChildOutcome::Lost` to `Announced::Failed` instead of
    /// `Announced::Lost` — still exhaustive, still compiles — and the
    /// `assert_eq!(outcome, back, ...)` line below is what turns red.
    #[test]
    fn round_trip_through_announced_outcome_is_total() {
        for outcome in [
            ChildOutcome::Completed,
            ChildOutcome::Failed,
            ChildOutcome::Cancelled,
            ChildOutcome::TimedOut,
            ChildOutcome::Lost,
        ] {
            let announced: AnnouncedOutcome = outcome.into();
            let back: ChildOutcome = announced.into();
            assert_eq!(
                outcome, back,
                "{outcome:?} did not round-trip through AnnouncedOutcome"
            );
            assert_eq!(
                outcome.as_str(),
                announced.as_str(),
                "wire spelling drifted for {outcome:?}"
            );
        }
    }
}

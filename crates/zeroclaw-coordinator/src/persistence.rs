// NOT derived from grok-build: this file contains no upstream code, so it
// carries no per-file change notice. Upstream had no durability seam at all —
// `pending`/`active`/`completed` lived only in the actor's own memory for the
// process's lifetime, and this trait is new surface area for the wiring phase.

//! The coordinator's durability seam.
//!
//! `pending` and `active` hold live-only material — cancellation tokens,
//! oneshot reply senders, `Instant` deadlines — that should die with the
//! process; nothing about a running child is worth persisting, because
//! nothing about it can be resumed from a row. Only the *undelivered* subset
//! of `completed` is worth writing down: a terminal result nobody has read
//! yet is exactly the thing a restart, a crash, or a slow parent can lose.
//!
//! This module defines the port for that write-through. It does not
//! implement one: no method here talks to a database, and the only
//! implementation shipped is the no-op below, so a [`crate::Coordinator`]
//! that never plugs in a store behaves exactly as it does today.
//!
//! ## The two moments, and why there are only two
//!
//! A child's persisted row is touched at exactly two points in its life:
//!
//! 1. **Spawn** ([`ChildPersistence::record_spawn`]) creates the row, in a
//!    non-terminal state. This is the only write while the child is pending
//!    or active.
//! 2. **`finish_child`** ([`ChildPersistence::record_finish`]) makes *one*
//!    update that carries the terminal status, the output, the error detail,
//!    and the delivered flag together, in the same write.
//!
//! ## Why the second moment must be one write, not two
//!
//! A claim query on the store side — some parent-side poller looking for
//! "finished, not yet delivered" rows — can only match a row whose status is
//! already terminal. While a row is still non-terminal, that query's filter
//! cannot see it, full stop. That is the whole trick: the row goes from
//! *unclaimable* straight to *terminal, with the correct delivered flag
//! already set*, in one atomic transition. There is no window in which the
//! row is terminal but its delivered flag has not caught up yet.
//!
//! **If an implementation splits this into "write terminal" and then, in a
//! second write, "mark delivered", that window reopens.** Between the two
//! writes, the row reads as terminal and undelivered — exactly the state the
//! claim query is looking for — so a concurrent claim on the parent side can
//! announce a result that the coordinator's own foreground delivery (the
//! spawn caller's inline `await`, or a blocking waiter) is *also* about to
//! deliver in the same instant. The parent then sees the same child announced
//! twice, once through each path. One write that sets both fields together is
//! what closes that race; a store implementation MUST NOT decompose
//! [`ChildPersistence::record_finish`] into two statements that make the
//! terminal state visible before the delivered flag is known.
//!
//! ## Why synchronous
//!
//! [`Coordinator::finish_child`](crate::Coordinator) — the only call site
//! `record_finish` will ever have, once the actor is wired to a store — is
//! itself a synchronous `&mut self` method: the actor is a single-writer
//! state machine polled from one task, not an async pipeline. An `async fn`
//! in this trait would force `finish_child` to become `.await`-shaped for a
//! write that, in the one store this workspace ships (sqlite), is already
//! synchronous internally. A sync trait matches the caller it will have and
//! the implementation it will get; there is no round trip to design around.
//!
//! ## What this is not
//!
//! This trait is defined, not wired: no field on [`Coordinator`](crate::Coordinator)
//! holds a `dyn ChildPersistence` yet, and nothing in this crate calls
//! [`ChildPersistence::record_spawn`] or [`ChildPersistence::record_finish`].
//! Plugging a real implementation in — sqlite or otherwise — and threading it
//! through `handle_spawn` / `finish_child` is later work, owned by whoever
//! owns that store.

use crate::types::{ChildRequest, ChildResult};

/// Write-through port for the coordinator's durability seam.
///
/// See the module docs for the two-moment contract and why
/// [`record_finish`](Self::record_finish) must be a single write. Every
/// method has a no-op default, so a host that implements nothing at all
/// still satisfies the trait — see [`NoopPersistence`], the concrete type
/// that does exactly that.
pub trait ChildPersistence {
    /// Create the row for a newly spawned child, in a non-terminal state.
    ///
    /// Called once per child, at spawn, before the child has run at all.
    /// There is no corresponding "active" write: nothing about a running
    /// child changes what would be persisted here.
    fn record_spawn(&mut self, request: &ChildRequest) {
        let _ = request;
    }

    /// Write a child's ending: terminal status, output, error detail, and
    /// whether it was already delivered to its parent in-process — all in
    /// one update.
    ///
    /// `delivered` is true when the coordinator's own foreground path
    /// (the spawn caller's inline reply, or a blocking waiter) already
    /// handed this result to a parent before this call. An implementation
    /// MUST write `outcome`/`output`/`detail`/`delivered` together, in a
    /// single statement — see the module doc for the double-announce race
    /// that reopens if the terminal write and the delivered write are split.
    fn record_finish(&mut self, child_id: &str, result: &ChildResult, delivered: bool) {
        let _ = (child_id, result, delivered);
    }
}

/// The default: no store, no writes, no behavior change.
///
/// Exists so a caller can hand the coordinator *something* implementing
/// [`ChildPersistence`] without writing a type of its own — every method is
/// the trait's own no-op default, inherited unmodified.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopPersistence;

impl ChildPersistence for NoopPersistence {}

#[cfg(test)]
mod tests {
    use super::{ChildPersistence, NoopPersistence};
    use crate::outcome::ChildOutcome;
    use crate::types::{ChildOverrides, ChildRequest, ChildResult};
    use std::sync::Arc;

    fn request() -> ChildRequest {
        ChildRequest {
            child_id: "c1".into(),
            prompt: "do it".into(),
            description: "d".into(),
            agent_type: "explore".into(),
            parent_session_id: "parent".into(),
            parent_prompt_id: None,
            resume_from: None,
            cwd: None,
            overrides: ChildOverrides::default(),
            run_in_background: false,
            surface_completion: true,
            await_to_completion: false,
            fork_context: false,
            cancel_token: crate::cancel::CancelToken::new(),
        }
    }

    /// The shipped no-op must not panic or change behavior — a host that
    /// never plugs in a store gets exactly what it has today.
    #[test]
    fn noop_persistence_accepts_both_calls_without_effect() {
        let mut persistence = NoopPersistence;
        let request = request();
        persistence.record_spawn(&request);
        let result = ChildResult {
            outcome: ChildOutcome::Completed,
            output: Arc::from("done"),
            child_id: "c1".into(),
            ..Default::default()
        };
        persistence.record_finish("c1", &result, true);
        // Reaching here without a panic is the whole assertion: there is no
        // observable state to inspect on a no-op.
    }
}

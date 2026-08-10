//! Session-key scoping and background-child announcement claims for the agent loop.
//!
//! Extracted from `loop_.rs` so the claim/unclaim RAII contract and ambient
//! session-key helpers are not interleaved with interactive turn assembly.

use std::sync::Arc;
#[cfg(test)]
use std::sync::{LazyLock, Mutex};

// Re-export from zeroclaw-types for backwards compatibility.
pub use zeroclaw_api::TOOL_LOOP_SESSION_KEY;
pub use zeroclaw_api::TOOL_LOOP_THREAD_ID;

/// Run a future with the thread ID set in task-local storage.
/// Rate-limiting reads this to assign per-sender buckets.
pub async fn scope_thread_id<F>(thread_id: Option<String>, future: F) -> F::Output
where
    F: std::future::Future,
{
    TOOL_LOOP_THREAD_ID.scope(thread_id, future).await
}

/// Run a future with the session key set in task-local storage.
/// The scope wraps the entire agent turn, so all tools invoked during
/// the turn (including nested calls) see the same session key.
/// SessionsCurrentTool reads this to identify the active session.
pub async fn scope_session_key<F>(session_key: Option<String>, future: F) -> F::Output
where
    F: std::future::Future,
{
    TOOL_LOOP_SESSION_KEY.scope(session_key, future).await
}

/// The ambient session key for this turn, or `None` when nothing scoped one.
///
/// A key scoped as `None`, or as whitespace, reads the same as "no key": both
/// would otherwise claim under a parent id no child can ever be filed under.
pub(crate) fn current_session_key() -> Option<String> {
    TOOL_LOOP_SESSION_KEY
        .try_with(Clone::clone)
        .ok()
        .flatten()
        .filter(|key| !key.trim().is_empty())
}

/// Whether a caller already put a usable session key in scope.
///
/// `try_with` errors when the task-local is not in scope at all — the common
/// case for a plain [`run`] caller (CLI, cron, heartbeat, SOP) — and that reads
/// as "unset", same as a scope carrying `None`.
pub(crate) fn session_key_is_scoped() -> bool {
    TOOL_LOOP_SESSION_KEY
        .try_with(|key| key.as_ref().is_some_and(|k| !k.trim().is_empty()))
        .unwrap_or(false)
}

/// The session key a [`run`] turn adopts when no caller scoped one.
///
/// **This is the single source of the `agent:<alias>` convention, and the
/// coupling is load-bearing in both directions.** Producers that register a
/// background child must write exactly this string into the row's `parent_id`
/// (the spawn seam in `crate::tools::spawn_subagent` is the caller that has to
/// agree), because it is the key [`claim_child_announcements_context`] looks
/// children up by. The two are a lock and a key cut from one blank: a drift as
/// small as a separator character does not fail loudly — it files every child
/// under a name no turn ever asks about, and the parent waits forever for
/// announcements that are sitting in the table. Change the format here and
/// nowhere else.
///
/// Deliberately per-alias and not per-run. Two consequences, both intended:
///
/// - Concurrent one-shot runs of the same alias share this key, so a sibling
///   run may claim a child another run dispatched. The claim is still atomic —
///   the announcement is delivered exactly once, and to the same logical agent.
/// - A per-run key would instead orphan every detached child of a finished
///   one-shot turn: nothing would ever run under that key again, so the child's
///   completion would sit undelivered forever. Losing news outright is worse
///   than delivering it to a sibling turn of the same agent.
pub(crate) fn synthetic_session_key_for_run(agent_alias: &str) -> String {
    format!("agent:{agent_alias}")
}

/// Test seam for [`child_announcement_store`]: a store to claim from without
/// installing the process-global control plane, which is a `OnceLock` that
/// cannot be uninstalled between tests (see `control_plane::global`).
#[cfg(test)]
pub(crate) static CHILD_ANNOUNCEMENT_STORE_TEST_HOOK: LazyLock<
    Mutex<Option<Arc<dyn crate::control_plane::TaskRegistry>>>,
> = LazyLock::new(|| Mutex::new(None));

/// The task store to claim child announcements from, or `None` when there is
/// no daemon — in which case nothing was ever supervised, so there is nothing
/// to announce and the turn proceeds untouched.
fn child_announcement_store() -> Option<Arc<dyn crate::control_plane::TaskRegistry>> {
    #[cfg(test)]
    {
        let hooked = CHILD_ANNOUNCEMENT_STORE_TEST_HOOK
            .lock()
            .expect("child-announcement store test hook lock should not be poisoned")
            .clone();
        if hooked.is_some() {
            return hooked;
        }
    }
    crate::control_plane::control_plane().map(|cp| Arc::clone(&cp.store))
}

/// Claim this turn's finished background children and render them for the
/// parent's context. `None` when there is nothing to say.
///
/// Called **once per turn**, at turn start, before the model sees anything —
/// never per prompt build. A child that finishes mid-turn is the *next* turn's
/// news; claiming again inside the tool loop would splice a completion into a
/// prompt the model is already reasoning about, and (worse) would race the
/// same rows against the next turn's claim.
///
/// Ordering is load-bearing, and injection is **not** the end of it.
/// `claim_undelivered_children` marks the rows delivered in the same statement
/// that returns them, so once it succeeds the announcements exist nowhere else.
/// Rendering is infallible string building
/// ([`AnnouncementBatch::to_context_block`]) and so is splicing the result into
/// the turn's user message — but that message goes into a *local* history
/// vector, and the model has still not seen it. Between there and the provider
/// call sit four fallible steps, every one of which returns `?` on a turn that
/// has already consumed its announcements: `build_iteration_tool_specs`
/// (`agent/turn/mod.rs:528`), `resolve_vision_provider` (`:535`),
/// `prepare_messages_for_iteration` (`:566`) and `enforce_tool_loop_budget`
/// (`:584`); the provider is not called until `:628`. That window is why every
/// claim comes with an [`UnclaimOnDrop`] guard.
///
/// A claim failure is logged and swallowed: the waker must never break a turn.
/// Nothing is lost in that case, because a failed claim marks nothing delivered
/// and the same rows are still there for the next turn.
///
/// **One claimant per conversation.** Callers must be the entry point that owns
/// the ambient key. [`process_message`] and the [`crate::agent::Agent`]
/// pipeline always are — their keys come from the channel orchestrator, the
/// gateway, ACP or RPC, and no inner turn shape sits between those and the
/// model. [`run`] is the exception and gates on it: it claims only when it
/// scoped the key itself, because a nested `run` inherits its caller's key and
/// claiming there would deliver the caller's children into the nested turn.
pub(crate) async fn claim_child_announcements_context() -> Option<ClaimedAnnouncements> {
    let session_key = current_session_key()?;
    let store = child_announcement_store()?;
    let announcements = match store.claim_undelivered_children(&session_key).await {
        Ok(announcements) => announcements,
        Err(error) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_category(::zeroclaw_log::EventCategory::Agent)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "session_key": session_key,
                        "error": error.to_string(),
                    })),
                "Could not claim finished background children for this turn; \
                 the turn proceeds and the announcements stay claimable next turn"
            );
            return None;
        }
    };
    if announcements.is_empty() {
        return None;
    }
    let ids: Vec<String> = announcements.iter().map(|a| a.task_id.clone()).collect();
    // Rendering is infallible from here on: the batch is non-empty (checked
    // above) and `to_context_block` returns `None` only for an empty batch, so
    // there is deliberately no `?` — nor any other early exit — between the
    // committed claim and the guard that can hand these rows back.
    let batch = zeroclaw_api::announce::AnnouncementBatch {
        parent_task_id: session_key.clone(),
        announcements,
    };
    let block = batch.to_context_block().unwrap_or_else(|| {
        debug_assert!(false, "a non-empty batch always renders a context block");
        String::new()
    });
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_category(::zeroclaw_log::EventCategory::Agent)
            .with_attrs(::serde_json::json!({
                "session_key": session_key,
                "claimed": ids.len(),
            })),
        "Announcing finished background children into this turn"
    );
    Some(ClaimedAnnouncements {
        // Trailing blank line: the block is spliced directly above the
        // timestamped user message, the same shape hardware RAG context uses.
        context: format!("{block}\n"),
        guard: UnclaimOnDrop::armed(store, ids, session_key),
    })
}

/// Claim for a turn, split into the text to splice in and the guard that must
/// outlive the provider call.
///
/// `should_claim` is the caller's ownership answer (see
/// [`claim_child_announcements_context`]'s "one claimant per conversation");
/// `false` claims nothing and yields no guard, so a caller that must not claim
/// cannot accidentally hold one.
pub(crate) async fn claim_announcements_for_turn(
    should_claim: bool,
) -> (String, Option<UnclaimOnDrop>) {
    if !should_claim {
        return (String::new(), None);
    }
    match claim_child_announcements_context().await {
        Some(claimed) => {
            let (context, guard) = claimed.into_parts();
            (context, Some(guard))
        }
        None => (String::new(), None),
    }
}

/// Claim for a turn whose caller owns the conversation key but builds its
/// messages *outside* the future that scopes it.
///
/// [`claim_announcements_for_turn`] reads the ambient key
/// ([`current_session_key`]), which is the right shape for every turn that
/// assembles its user message inside its own [`scope_session_key`] — the CLI
/// `run` turns, [`process_message`] and the [`crate::agent::Agent`] pipeline all
/// are. An outer turn shape that scopes the key only around the tool-loop future
/// is not: at turn-assembly time nothing has been scoped yet, the ambient key
/// reads `None`, and a claim there is silently a no-op. **Today the channel
/// orchestrator is the only such caller** (`orchestrator/mod.rs`: the key is
/// scoped at the `scope_session_key(Some(history_key.clone()), tool_loop)` line,
/// long after `history` is built).
///
/// So this scopes the key itself and delegates. The one-claimant reasoning, the
/// claim/render ordering and the guard contract stay in
/// [`claim_child_announcements_context`] and are not restated here — a second
/// copy of that reasoning is exactly how the two drift apart.
///
/// The caller is asserting ownership of `session_key` by calling this at all
/// (the `should_claim` gate [`claim_announcements_for_turn`] carries for `run`'s
/// nested case has no analogue here: an outer turn shape is never nested inside
/// another turn). It must hold the returned guard until its turn has ended,
/// then settle it against that turn's outcome
/// ([`settle_announcement_guards`], or [`UnclaimOnDrop::settle`] for a single
/// guard); on any earlier exit the guard drops armed and the announcements go
/// back.
pub async fn claim_announcements_for_scoped_turn(
    session_key: &str,
) -> (String, Option<UnclaimOnDrop>) {
    scope_session_key(
        Some(session_key.to_owned()),
        claim_announcements_for_turn(true),
    )
    .await
}

/// One turn's claimed announcements: the text to splice in, and the guard that
/// gives them back if this turn never reaches the model.
pub(crate) struct ClaimedAnnouncements {
    context: String,
    guard: UnclaimOnDrop,
}

impl ClaimedAnnouncements {
    /// Split into the context block and its guard. The caller must keep the
    /// guard alive until the turn has ended, then settle it against that
    /// turn's outcome ([`settle_announcement_guards`]); dropping it earlier
    /// hands the announcements back to the store.
    pub(crate) fn into_parts(self) -> (String, UnclaimOnDrop) {
        (self.context, self.guard)
    }
}

/// Hands claimed announcements back to the store unless the turn that claimed
/// them got them in front of the model.
///
/// The claim commits `delivered = 1` before anything can be done with the
/// announcements — that is what makes a completion arrive exactly once, and it
/// is not negotiable, because the alternative (read, then flag) lets two wakers
/// announce the same completion. The consequence is a window: a turn can claim,
/// splice the text into a local history vector, and *still* fail before the
/// provider is ever called. Those rows would be flagged delivered with nobody
/// having read them, and nothing would ever look at them again.
///
/// So the failure path trades exactly-once for at-least-once. On drop while
/// armed, the ids go back to `delivered = 0` and the next turn under the same
/// key claims them again. A parent shown a completion twice can reconcile it; a
/// parent never shown it cannot.
///
/// **The criterion is a turn that succeeded, not a provider that was called**,
/// and it is spelled exactly once, in [`TurnOutcome`]; this guard is settled
/// through [`UnclaimOnDrop::settle`] and never told what to do site by site.
/// External review read the old per-site disarms as claiming the latter and
/// found the gap in it: a turn can reach its provider and still fail after —
/// streaming the generated text, preparing or executing tool calls,
/// cancellation mid-batch — and every one of those drops this guard armed
/// even though the model demonstrably read the block. Announcing again there
/// is correct rather than merely tolerated, and that is the reason the rule is
/// written this way round: the block is spliced into the turn's *local*
/// history, while every caller persists its user message before the splice, so
/// a turn that dies takes the block down with it and the next turn's history
/// does not carry it. Disarming on provider contact would erase the
/// announcement from the only two places it could still be read.
///
/// Sibling turns make this sharper, not softer: one-shot `run()`s of the same
/// alias share the synthetic `agent:<alias>` key
/// ([`synthetic_session_key_for_run`]), so without this guard run A's failure
/// before its provider call would permanently destroy the announcement of a
/// child that run B dispatched.
///
/// **Residual window, stated plainly.** `Drop` cannot await, so the unclaim is
/// dispatched as a detached task and is fire-and-forget by necessity. If the
/// process dies between the drop and that task's UPDATE, the rows stay
/// `delivered = 1` having been seen by nobody, and no later turn will find
/// them. That requires process death inside a narrow interval, and it is the
/// one hole left in the chain — it is not closed here, it is named here.
/// `panic = "abort"` in the release and dist profiles (`Cargo.toml`) widens
/// the same hole from the other side: an abort runs no destructors at all, so
/// in a shipped binary a panicking turn never reaches this `Drop`. RAII
/// narrows this window; it cannot close it, and no reading of this type should
/// suggest otherwise.
///
/// Public because [`claim_announcements_for_scoped_turn`] hands it across a
/// crate boundary, and that is the point: the `Drop` semantics travel with the
/// value, so an out-of-crate caller that returns early — or is cancelled, or
/// panics — gives the announcements back without knowing it has to.
pub struct UnclaimOnDrop {
    store: Arc<dyn crate::control_plane::TaskRegistry>,
    ids: Vec<String>,
    session_key: String,
    armed: bool,
}

/// Did this turn succeed? The one criterion that decides whether the
/// announcements a turn claimed stay marked delivered.
///
/// [`UnclaimOnDrop`] hands its rows back unless the turn that claimed them
/// succeeded, and "succeeded" is spelled differently by different turn shapes:
/// the runtime's turns end in a `Result`, while the channel orchestrator ends
/// in a three-level `LlmExecutionResult` whose nesting separates cancellation
/// from timeout from tool-loop failure. Those are different *answers*; the
/// question is one, and it lives here. A site cannot invent a criterion of its
/// own without implementing this trait for its own outcome type, in the open,
/// next to the other implementations.
///
/// That is the point. The criterion used to be hand-written at every disarm
/// site, and with no single place being *the* criterion, [`UnclaimOnDrop`]'s
/// own documentation drifted a whole version away from what those sites did —
/// it described the rule as "the provider was reached" while every site
/// implemented "the turn returned success" — and nothing could notice.
pub trait TurnOutcome {
    /// `true` when the turn succeeded, so the announcements it claimed stay
    /// delivered; `false` hands them back to the store for a later turn.
    fn turn_succeeded(&self) -> bool;
}

/// A turn that returns `Ok` succeeded; every error hands its announcements
/// back. Every claim site in this crate ends in a `Result`, so this is the
/// implementation they all settle through.
impl<T, E> TurnOutcome for Result<T, E> {
    fn turn_succeeded(&self) -> bool {
        self.is_ok()
    }
}

/// Asking by reference asks the same question: [`settle_announcement_guards`]
/// settles several guards against one shared outcome it does not own.
impl<T: TurnOutcome + ?Sized> TurnOutcome for &T {
    fn turn_succeeded(&self) -> bool {
        (**self).turn_succeeded()
    }
}

impl UnclaimOnDrop {
    pub(crate) fn armed(
        store: Arc<dyn crate::control_plane::TaskRegistry>,
        ids: Vec<String>,
        session_key: String,
    ) -> Self {
        Self {
            store,
            ids,
            session_key,
            armed: true,
        }
    }

    /// Settle this claim against how the turn ended, and give the outcome back.
    ///
    /// Success keeps the ids `delivered = 1`; anything else lets this guard
    /// drop still armed, so `Drop` returns them to the store. The judgement is
    /// [`TurnOutcome::turn_succeeded`]'s, never the call site's.
    ///
    /// The guard is consumed, because a turn settles exactly once — at the
    /// point where it knows how it ended. A retry (a mid-turn model switch)
    /// must *not* settle: it loops with the same history, which the model has
    /// still not read. So a site with a retry loop yields its outcome out of
    /// the loop and settles once outside it, rather than settling per attempt.
    ///
    /// The outcome is returned rather than borrowed so that settling is an
    /// expression the call site can `return`, leaving no path that produces a
    /// success without passing this guard.
    pub fn settle<T: TurnOutcome>(mut self, outcome: T) -> T {
        if outcome.turn_succeeded() {
            self.armed = false;
        }
        outcome
    }
}

/// Settle every guard a turn claimed against that turn's one outcome, and give
/// the outcome back.
///
/// A streamed turn can claim more than once — the opening user message plus
/// each mid-turn steering message — and those guards stand or fall together:
/// one provider call either read all of them or none, and keeping one while
/// returning another would either repeat a completion or lose it. Taking
/// anything that yields guards means the single-claim sites
/// (`Option<UnclaimOnDrop>`) and the many-claim site (`Vec<UnclaimOnDrop>`)
/// settle through this one function instead of through two.
pub fn settle_announcement_guards<T: TurnOutcome>(
    guards: impl IntoIterator<Item = UnclaimOnDrop>,
    outcome: T,
) -> T {
    for guard in guards {
        guard.settle(&outcome);
    }
    outcome
}

impl Drop for UnclaimOnDrop {
    fn drop(&mut self) {
        if !self.armed || self.ids.is_empty() {
            return;
        }
        let store = Arc::clone(&self.store);
        let ids = std::mem::take(&mut self.ids);
        let session_key = self.session_key.clone();
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_category(::zeroclaw_log::EventCategory::Agent)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({
                    "session_key": session_key,
                    "task_ids": ids,
                })),
            "Turn ended before its claimed background announcements reached the model; \
             returning them to the store so a later turn announces them again"
        );
        // `Drop` cannot await, so the UPDATE is dispatched as a detached task.
        // Spawning needs a runtime: without one there is nowhere to dispatch
        // to, and panicking here — in a destructor — would take the process
        // with it. Say so instead.
        if tokio::runtime::Handle::try_current().is_err() {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_category(::zeroclaw_log::EventCategory::Agent)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "session_key": session_key,
                        "task_ids": ids,
                    })),
                "No runtime to return unseen background announcements on; \
                 these completions stay flagged delivered but were never read"
            );
            return;
        }
        zeroclaw_spawn::spawn!(async move {
            match store.unclaim_children(&ids).await {
                Ok(0) => {}
                Ok(returned) => {
                    ::zeroclaw_log::record!(
                        INFO,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_category(::zeroclaw_log::EventCategory::Agent)
                            .with_attrs(::serde_json::json!({
                                "session_key": session_key,
                                "returned": returned,
                            })),
                        "Returned unseen background announcements to the store"
                    );
                }
                Err(error) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                            .with_category(::zeroclaw_log::EventCategory::Agent)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "session_key": session_key,
                                "task_ids": ids,
                                "error": error.to_string(),
                            })),
                        "Could not return unseen background announcements; \
                         these completions are now flagged delivered but were never read"
                    );
                }
            }
        });
    }
}

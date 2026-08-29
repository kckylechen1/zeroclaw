//! Session-key scoping for the agent loop.
//!
//! Extracted from `loop_.rs` so the ambient session-key helpers are not
//! interleaved with interactive turn assembly. The background-child
//! announcement claim machinery that used to share this module retired with
//! the durable control plane (migration wall 4): its last producer died with
//! the spawn wall and durable task truth is Tachi's through the bridge
//! (frozen bridge contract, annex rows 1 and 6).

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
/// A key scoped as `None`, or as whitespace, reads the same as "no key".
pub(crate) fn current_session_key() -> Option<String> {
    TOOL_LOOP_SESSION_KEY
        .try_with(Clone::clone)
        .ok()
        .flatten()
        .filter(|key| !key.trim().is_empty())
}

/// The session key a [`run`] turn adopts when no caller scoped one.
///
/// **This is the single source of the `agent:<alias>` convention.** Channel
/// conversations use their real history key; a one-shot `run` under no caller
/// key gets this synthetic one so per-alias state (rate-limit buckets,
/// session-scoped tools) has a stable identity across one-shot invocations.
///
/// Deliberately per-alias and not per-run: concurrent one-shot runs of the
/// same alias share the key, so per-alias state stays coherent for the
/// logical agent rather than for each ephemeral process.
///
/// [`run`]: crate::agent::run
pub(crate) fn synthetic_session_key_for_run(agent_alias: &str) -> String {
    format!("agent:{agent_alias}")
}

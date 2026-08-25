// Derived from grok-build (Apache-2.0), revision
// 1adcd1f477870e4a97bacbd6be78c8a3bfbac46d, from
// `.../grok_build/task/types.rs`.
// Copyright 2023-2026 SpaceXAI. Licensed under the Apache License, Version 2.0.
//
// This file was CHANGED by ZeroClaw Labs: the outcome vocabulary is
// ZeroClaw's (`ChildOutcome`) rather than upstream's success/cancelled
// booleans; the `Resources` injection glue (`register_resource!`,
// `TaskModelValidator`, `SubagentForegroundWait`, `GoalLoopActive`, the depth
// and session-id resources), the tool-kind capability filtering, the
// model-argument sanitizers, usage accounting, the workflow owner, and the
// unused multi-wait request were dropped; `educe`'s Debug-ignore derives were
// replaced with hand-written `Debug` impls where needed.
// See ../LICENSE and ../NOTICE.

//! Request, reply, and command types for child coordination.
//!
//! Request data is deliberately separate from command reply envelopes: the
//! coordinator actor owns every reply sender and every lifecycle transition,
//! while a child runner receives only plain request data. One command enum goes
//! down one channel, so there is exactly one order of events.

use std::fmt;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use crate::cancel::CancelToken;
use crate::outcome::ChildOutcome;

// ── Spawn ────────────────────────────────────────────────────────────────

/// Everything a runner needs to start one child.
#[derive(Debug, Clone)]
pub struct ChildRequest {
    /// Stable id for the child, chosen by the caller. Also the id under which
    /// the child can later be queried or cancelled.
    pub child_id: String,
    pub prompt: String,
    pub description: String,
    /// Which agent definition the child runs as.
    pub agent_type: String,
    pub parent_session_id: String,
    /// The agent alias that owns the parent session — NOT a role/agent-type
    /// spelling like `"explore"`.
    ///
    /// This is what belongs in a persisted `TaskRecord.agent` (documented
    /// there as "the agent alias that owns and executes this task", and read
    /// by alias-keyed admin cascades) and, downstream of that, in
    /// `Announcement.agent` ("the agent alias that ran, which for a subagent
    /// is the parent's own"). `parent_session_id` stays session identity: the
    /// value `TaskRecord.parent_id` is keyed on, used to look up a session's
    /// children — it is a different axis than "whose alias owns this row".
    pub parent_alias: String,
    /// The parent turn that launched this child.
    ///
    /// Cancelling a turn cancels the children that turn spawned, and nothing
    /// else: children from earlier turns keep running.
    pub parent_prompt_id: Option<String>,
    /// Resume from a previously completed child's conversation.
    pub resume_from: Option<String>,
    /// Explicit working directory for the child. Validated by the runner.
    pub cwd: Option<String>,
    pub overrides: ChildOverrides,
    /// Launched as background work.
    ///
    /// Background does not mean fire-and-forget: the completion still reaches
    /// the parent when `surface_completion` is set, and cancelling the parent
    /// turn still cancels the child.
    pub run_in_background: bool,
    /// When false the child's completion is never buffered for the parent —
    /// used by internal children whose existence the parent must not see.
    pub surface_completion: bool,
    /// Wait for the real ending however long it takes: no foreground budget.
    pub await_to_completion: bool,
    /// Seed the child with the parent's conversation before `prompt`.
    /// A successful `resume_from` takes precedence.
    pub fork_context: bool,
    pub cancel_token: CancelToken,
}

/// Per-spawn overrides. `None` means "inherit from the parent or the role".
#[derive(Debug, Clone, Default)]
pub struct ChildOverrides {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    /// Named persona to apply to the child.
    pub persona: Option<String>,
    /// Cap, in bytes, on the output carried in this child's completion summary.
    pub completion_output_cap: Option<usize>,
    pub spawn_depth: Option<u32>,
    pub output_token_budget: Option<u64>,
    /// Groups children belonging to one repeating unit of work, so a host can
    /// ask whether that unit still has anything in flight.
    pub loop_task_id: Option<String>,
    /// Unified spawn lineage (SA-9): the CHILD's lineage (parent lineage
    /// advanced by one), so a native runner's registry rebuild inherits
    /// the spawning context's depth instead of minting a fresh ledger
    /// (SA-11). `None` means the spawner did not thread a lineage and
    /// the child run resolves its own root.
    pub lineage: Option<zeroclaw_api::subagent_v1::LineageRef>,
    /// See [`Self::hosted_execution`]. Private so only that constructor can
    /// turn hosted execution on; other crates read it through [`Self::hosted_run`].
    hosted_run: bool,
}

impl ChildOverrides {
    /// Overrides carrying the unified spawn lineage (SA-9): the CHILD's
    /// lineage (parent advanced by one), so a native runner's registry
    /// rebuild inherits the spawning context's depth (SA-11).
    #[must_use]
    pub fn with_lineage(lineage: Option<zeroclaw_api::subagent_v1::LineageRef>) -> Self {
        Self {
            lineage,
            ..Self::default()
        }
    }

    /// Hosted execution: the coordinator admits, persists, queries, and
    /// cancels, but the runner does not start a native agent turn. The host
    /// delivers the [`ChildResult`] (background `delegate` parks a oneshot
    /// the runner waits on).
    ///
    /// Trust boundary: only the background `delegate` worker should construct
    /// this. Coordinator Spawn still only enforces duplicate id, spawn depth,
    /// and capacity; policy gates are the constructor's responsibility.
    #[must_use]
    pub fn hosted_execution(spawn_depth: Option<u32>) -> Self {
        Self {
            spawn_depth,
            hosted_run: true,
            ..Self::default()
        }
    }

    /// Whether this spawn is hosted execution. See [`Self::hosted_execution`].
    #[must_use]
    pub fn hosted_run(&self) -> bool {
        self.hosted_run
    }
}

// ── Admission ────────────────────────────────────────────────────────────

/// Why the coordinator refused to admit a spawn.
///
/// A refusal is decided *before any child exists*, so it is deliberately not a
/// [`ChildResult`]: there is no run whose outcome could be reported, and a
/// refusal dressed as a failed result is indistinguishable from a child that
/// started and failed. That disguise is what let the detached spawn path drop
/// its reply receiver — defensible for a terminal result, fatal for a refusal —
/// and tell the model it had started a child that was never admitted.
///
/// Structured rather than a bare string: a caller has to be able to *branch* on
/// which gate refused (a capacity refusal is worth retrying later, a depth
/// refusal never is), and recovering that from prose means substring-matching
/// the very message the model reads. [`fmt::Display`] is the single source of
/// the human-readable sentence, so a caller that only wants to print one still
/// gets exactly what the coordinator would have said.
///
/// Deliberately NOT `#[non_exhaustive]`: a fourth admission gate must break
/// every out-of-crate `match` at compile time. Silently routing a new refusal
/// into a catch-all arm is the failure mode this whole type exists to end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnRefusal {
    /// A child under this id is already pending, active, or completed.
    DuplicateChildId { child_id: String },
    /// The child's *resolved* generation (see `Coordinator::handle_spawn`, which
    /// derives it from a live spawner and lets a declaration only raise it) is
    /// deeper than the coordinator admits.
    SpawnDepthExceeded { depth: u32, max: u32 },
    /// `pending + active` is already at the concurrency limit.
    ///
    /// `in_flight` is that population at the moment of the refusal, carried
    /// alongside `max` because the two together are what make this refusal
    /// legible: "6 of 6 are running" is a queue that will drain, while the
    /// limit alone reads like a capability that is broken. A caller — or a
    /// model — deciding whether to retry needs the difference, and once the
    /// default is a working limit rather than a runaway backstop, this
    /// refusal is an ordinary daily event rather than a bug report.
    ///
    /// At today's single capacity gate the two are necessarily equal — the
    /// gate fires at `in_flight >= max` and nothing admits past it — so this
    /// field is not carrying information the limit lacks. It is carrying
    /// *measurement*: the count is read from the live registry at the moment
    /// of refusal rather than restated from the limit, so the sentence stays
    /// true rather than merely lucky if a later gate refuses below the cap.
    /// A number that is really the same number typed twice is the failure
    /// mode this crate keeps finding; this one is not that.
    ///
    /// It is also captured rather than recomputed by the reader: by the time a
    /// refusal is read the actor has moved on, and a count taken then is a
    /// different number than the one the gate decided on.
    ChildCapacityReached { in_flight: usize, max: usize },
    /// The actor was torn down while this command was still queued in its
    /// mailbox, so it was never decided on its merits. Nothing ran; a retry
    /// against a live coordinator may well be admitted.
    ///
    /// This is a refusal rather than a dropped sender on purpose: an unanswered
    /// admission channel leaves the caller waiting out its own timeout for an
    /// answer that can never come.
    CoordinatorShuttingDown,
}

impl fmt::Display for SpawnRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateChildId { child_id } => {
                write!(f, "child id '{child_id}' already exists")
            }
            Self::SpawnDepthExceeded { depth, max } => write!(
                f,
                "spawn depth limit reached ({depth}/{max}): this child would be generation \
                 {depth}, deeper than the coordinator admits. Nothing was started."
            ),
            Self::ChildCapacityReached { in_flight, max } => write!(
                f,
                "too many children in flight ({in_flight} running, limit {max}). This is a full \
                 queue, not a broken tool: wait for one to finish, or cancel one, then try again. \
                 Nothing was started."
            ),
            Self::CoordinatorShuttingDown => f.write_str(
                "the coordinator shut down before deciding on this spawn. Nothing was started.",
            ),
        }
    }
}

impl std::error::Error for SpawnRefusal {}

/// The coordinator's answer to "may this child run at all?".
///
/// Sent once, at the moment the actor decides, and never later — it does not
/// wait for the child. A named enum rather than `Result<(), SpawnRefusal>`
/// because this is not a fallible operation reported to its caller: both arms
/// are ordinary, expected answers, `?` on it would be wrong, and `Admitted` is
/// where a future "…and here is what you were admitted as" payload belongs
/// without rewriting every match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnAdmission {
    Admitted,
    Refused(SpawnRefusal),
}

/// Spawn command envelope owned by the coordinator mailbox.
///
/// Two reply channels, because admission and outcome are different events with
/// different timing, different arity, and different meaning:
///
/// - [`admission_tx`](Self::admission_tx) — **one** answer, immediately, at the
///   moment the actor decides whether this child may run at all.
/// - [`result_tx`](Self::result_tx) — the caller's **outcome stream** for a
///   child that was admitted and did run.
///
/// A caller may keep only the first (a detached spawn: it needs to know it was
/// admitted, not when the child ends) but never only the second, because a
/// refused spawn never produces one.
pub struct SpawnCommand {
    pub request: Box<ChildRequest>,
    /// The coordinator's admission decision, answered exactly once,
    /// synchronously with the decision itself and before the child could have
    /// been handed to the runner.
    ///
    /// ## What a dropped sender means to the caller
    ///
    /// A `RecvError` here reads as **"not admitted / unknown"**, and never as
    /// "started". The actor resolves this channel on every path it controls —
    /// refusal, acceptance, and a shutdown drop that catches commands still
    /// queued in the mailbox (`Coordinator::drop`) — so a `RecvError` means the
    /// command reached no actor at all or died with one. Reading it as
    /// "started" re-creates, from the other direction, exactly the phantom
    /// child that splitting this channel exists to end.
    pub admission_tx: oneshot::Sender<SpawnAdmission>,
    /// The admitted child's outcome stream: at most an interim handoff, then
    /// the terminal result.
    ///
    /// NOT terminal-only. A foreground child whose budget elapses first gets an
    /// interim `ChildResult { backgrounded: true, .. }` here — the child is
    /// still running (`state::background_at_deadline`) — and its real ending
    /// arrives later through a query or a buffered completion. Callers branch
    /// on `backgrounded`. That handoff stays on this channel: it only exists
    /// when a `foreground_deadline` is set, which an explicitly
    /// `run_in_background` spawn never has, so it is not an admission event and
    /// moving it would change foreground behaviour.
    ///
    /// Only an *admitted* child ever answers here; on a refusal this sender is
    /// dropped untouched, because there is no run to report.
    pub result_tx: oneshot::Sender<ChildResult>,
}

impl fmt::Debug for SpawnCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpawnCommand")
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}

impl std::ops::Deref for SpawnCommand {
    type Target = ChildRequest;

    fn deref(&self) -> &Self::Target {
        &self.request
    }
}

impl SpawnCommand {
    /// Build and send a reply while the plain request remains borrowable.
    ///
    /// For channel adapters and deterministic test harnesses; production
    /// lifecycle replies belong to the coordinator. Producing a terminal
    /// result means the child was admitted, so this answers the admission
    /// channel first — a caller that awaits admission before the result (every
    /// one of them, since a refusal has no result) must not be left hanging.
    ///
    /// # Errors
    ///
    /// Returns the built result when the spawn caller is gone.
    #[allow(clippy::result_large_err)] // ChildResult is a public API type; Err only on dropped channel
    pub fn respond_with(
        self,
        build: impl FnOnce(&ChildRequest) -> ChildResult,
    ) -> Result<(), ChildResult> {
        let result = build(&self.request);
        let _ = self.admission_tx.send(SpawnAdmission::Admitted);
        self.result_tx.send(result)
    }
}

// ── Result ───────────────────────────────────────────────────────────────

/// What a child run produced.
#[derive(Debug, Clone)]
pub struct ChildResult {
    pub outcome: ChildOutcome,
    /// The child's final output text.
    ///
    /// `Arc<str>` because a child's output can be its whole transcript and it
    /// is cloned into every per-consumer summary; cloning must stay a refcount
    /// bump.
    pub output: Arc<str>,
    /// Why it ended the way it did. Populated for every non-success outcome, so
    /// a reader that looks only here still learns something.
    pub detail: Option<String>,
    pub child_id: String,
    /// The child's own session id, once it has one.
    pub child_session_id: String,
    pub tool_calls: u32,
    pub turns: u32,
    pub duration_ms: u64,
    pub tokens_used: u64,
    pub output_tokens_used: u64,
    pub total_tokens_used: u64,
    /// Path to the isolated worktree, if one was created for the child.
    pub worktree_path: Option<String>,
    /// This reply is a handle-off, not an ending: the child exceeded its
    /// foreground budget and is *still running*. Its real ending arrives later
    /// through a query or a buffered completion.
    pub backgrounded: bool,
}

impl Default for ChildResult {
    /// A result carrying no information is [`ChildOutcome::Lost`].
    ///
    /// The default must never read as success, and it must not claim a clean
    /// failure either — nothing here knows whether the work happened. Every
    /// site that means "failed" says so explicitly.
    fn default() -> Self {
        Self {
            outcome: ChildOutcome::Lost,
            output: Arc::from(""),
            detail: None,
            child_id: String::new(),
            child_session_id: String::new(),
            tool_calls: 0,
            turns: 0,
            duration_ms: 0,
            tokens_used: 0,
            output_tokens_used: 0,
            total_tokens_used: 0,
            worktree_path: None,
            backgrounded: false,
        }
    }
}

impl ChildResult {
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.outcome.is_success()
    }
}

// ── Query ────────────────────────────────────────────────────────────────

/// Look up one child's current state.
pub struct QueryCommand {
    pub child_id: String,
    /// Restrict the lookup to children owned by this parent session.
    pub parent_session_id: Option<String>,
    /// Wait (up to `timeout_ms`) for a terminal state before replying.
    pub block: bool,
    /// Max wait when blocking. Defaults to 30s.
    pub timeout_ms: Option<u64>,
    pub respond_to: oneshot::Sender<Option<ChildSnapshot>>,
}

impl fmt::Debug for QueryCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueryCommand")
            .field("child_id", &self.child_id)
            .field("parent_session_id", &self.parent_session_id)
            .field("block", &self.block)
            .field("timeout_ms", &self.timeout_ms)
            .finish_non_exhaustive()
    }
}

/// Point-in-time view of one child.
#[derive(Debug, Clone)]
pub struct ChildSnapshot {
    pub child_id: String,
    pub description: String,
    pub agent_type: String,
    pub status: ChildStatus,
    /// Wall-clock start time (epoch ms).
    pub started_at_epoch_ms: u64,
    pub duration_ms: u64,
    pub persona: Option<String>,
}

impl ChildSnapshot {
    /// Whether the child is still in flight — the liveness rule every blocking
    /// query loops on.
    #[must_use]
    pub fn is_running(&self) -> bool {
        matches!(
            self.status,
            ChildStatus::Running { .. } | ChildStatus::Initializing
        )
    }
}

/// Lifecycle metadata for presentation and extension callers.
#[derive(Debug, Clone)]
pub struct ChildInspection {
    pub snapshot: ChildSnapshot,
    pub parent_session_id: String,
    pub child_session_id: String,
    pub fork_parent_prompt_id: Option<String>,
    pub resumed_from: Option<String>,
}

/// Where a child is in its life.
///
/// The terminal states are one variant carrying a [`ChildOutcome`], not one
/// variant per ending: a reader that handles "finished" cannot forget to handle
/// a newly added ending, and there is only ever one vocabulary for how a run
/// ended.
#[derive(Debug, Clone)]
pub enum ChildStatus {
    /// Being set up — resolving config, preparing a workspace, starting the
    /// session. A query during this phase reports initializing, never
    /// "not found".
    Initializing,
    /// Running. Fields are pulled from the child at query time.
    Running {
        turn_count: u32,
        tool_call_count: u32,
        tokens_used: u64,
        context_window_tokens: u64,
        /// Context window usage as a percentage (0–100).
        context_usage_pct: u8,
        /// Distinct tool names called so far.
        tools_used: Vec<String>,
        error_count: u32,
    },
    /// Ended. `outcome` says how.
    ///
    /// Token fields are the terminal projection of [`ChildResult`]'s usage
    /// (`tokens_used` / `output_tokens_used` / `total_tokens_used`). They
    /// must be copied from the completed result — not zeroed — so inspect
    /// and query see the same accounting the runner produced.
    Finished {
        outcome: ChildOutcome,
        output: String,
        detail: Option<String>,
        tool_calls: u32,
        turns: u32,
        tokens_used: u64,
        output_tokens_used: u64,
        total_tokens_used: u64,
        worktree_path: Option<String>,
    },
}

impl ChildStatus {
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Finished { .. })
    }

    /// The outcome for a finished child, `None` while it is still in flight.
    #[must_use]
    pub fn outcome(&self) -> Option<ChildOutcome> {
        match self {
            Self::Finished { outcome, .. } => Some(*outcome),
            _ => None,
        }
    }
}

// ── Cancel ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum CancelTarget {
    ChildId(String),
    /// Every child spawned by one parent turn.
    ParentPromptId(String),
}

pub struct CancelCommand {
    pub parent_session_id: Option<String>,
    pub target: CancelTarget,
    pub respond_to: oneshot::Sender<CancelOutcome>,
}

impl fmt::Debug for CancelCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancelCommand")
            .field("parent_session_id", &self.parent_session_id)
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub enum CancelOutcome {
    Cancelled,
    /// Nothing to cancel: it had already ended, this way.
    AlreadyFinished {
        outcome: ChildOutcome,
    },
    NotFound,
}

// ── Completion buffering ─────────────────────────────────────────────────

/// A finished child, as handed to the parent between turns.
///
/// Session ownership lives on the coordinator's internal buffer entry, so a
/// delivered summary carries no owner field.
#[derive(Debug, Clone)]
pub struct ChildCompletionSummary {
    pub child_id: String,
    pub agent_type: String,
    pub description: String,
    pub outcome: ChildOutcome,
    pub duration_ms: u64,
    pub tool_calls: u32,
    pub turns: u32,
    /// The child's final output, refcount-shared with [`ChildResult::output`].
    pub output: Arc<str>,
}

/// Drain buffered completion summaries.
pub struct CompletionsCommand {
    pub parent_session_id: Option<String>,
    /// Ids the caller has already surfaced by other means.
    pub suppress_ids: Vec<String>,
    pub respond_to: oneshot::Sender<Vec<ChildCompletionSummary>>,
}

impl fmt::Debug for CompletionsCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompletionsCommand")
            .field("parent_session_id", &self.parent_session_id)
            .field("suppress_ids", &self.suppress_ids)
            .finish_non_exhaustive()
    }
}

// ── Outstanding work ─────────────────────────────────────────────────────

/// What one parent turn still has in flight.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutstandingReply {
    /// Turn-blocking children still pending or active.
    pub live_ids: Vec<String>,
    /// A background child is still running: it does not block the turn, but the
    /// turn is not the end of the story either.
    pub background_live: bool,
}

pub struct OutstandingCommand {
    pub parent_session_id: String,
    pub prompt_id: String,
    pub respond_to: oneshot::Sender<OutstandingReply>,
}

impl fmt::Debug for OutstandingCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutstandingCommand")
            .field("parent_session_id", &self.parent_session_id)
            .field("prompt_id", &self.prompt_id)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RegistryCounts {
    pub pending: usize,
    pub active: usize,
    pub completed: usize,
}

pub struct RegistryCountsCommand {
    pub respond_to: oneshot::Sender<RegistryCounts>,
}

impl fmt::Debug for RegistryCountsCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegistryCountsCommand").finish()
    }
}

// ── Inspection ───────────────────────────────────────────────────────────

/// Full metadata plus a resolved progress snapshot.
pub struct InspectCommand {
    pub child_id: String,
    pub parent_session_id: Option<String>,
    pub respond_to: oneshot::Sender<Option<ChildInspection>>,
}

impl fmt::Debug for InspectCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InspectCommand")
            .field("child_id", &self.child_id)
            .field("parent_session_id", &self.parent_session_id)
            .finish_non_exhaustive()
    }
}

/// Every running child owned by one parent session.
pub struct ListRunningCommand {
    pub parent_session_id: String,
    pub respond_to: oneshot::Sender<Vec<ChildInspection>>,
}

impl fmt::Debug for ListRunningCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ListRunningCommand")
            .field("parent_session_id", &self.parent_session_id)
            .finish_non_exhaustive()
    }
}

/// Lightweight summary of a running child.
#[derive(Debug, Clone)]
pub struct ActiveChildSummary {
    pub child_id: String,
    pub agent_type: String,
    pub description: String,
    /// Wall-clock time since spawn.
    pub elapsed_ms: u64,
}

/// List running children cheaply, without pulling live progress.
pub struct ListActiveCommand {
    pub parent_session_id: String,
    pub respond_to: oneshot::Sender<Vec<ActiveChildSummary>>,
}

impl fmt::Debug for ListActiveCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ListActiveCommand")
            .field("parent_session_id", &self.parent_session_id)
            .finish_non_exhaustive()
    }
}

/// Reference to a child spawned during one parent turn.
#[derive(Debug, Clone)]
pub struct SpawnedChildRef {
    pub child_id: String,
    pub child_session_id: String,
    pub agent_type: String,
    pub description: String,
    pub persona: Option<String>,
    pub resumed_from: Option<String>,
}

pub struct SpawnedRefsCommand {
    pub parent_session_id: String,
    pub prompt_id: String,
    pub respond_to: oneshot::Sender<Vec<SpawnedChildRef>>,
}

impl fmt::Debug for SpawnedRefsCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpawnedRefsCommand")
            .field("parent_session_id", &self.parent_session_id)
            .field("prompt_id", &self.prompt_id)
            .finish_non_exhaustive()
    }
}

/// Whether a repeating unit of work still has children in flight.
pub struct LoopUnitActiveCommand {
    pub task_id: String,
    pub respond_to: oneshot::Sender<bool>,
}

impl fmt::Debug for LoopUnitActiveCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoopUnitActiveCommand")
            .field("task_id", &self.task_id)
            .finish_non_exhaustive()
    }
}

// ── Resume ───────────────────────────────────────────────────────────────

/// In-memory source data a runner needs to resume a finished child.
#[derive(Debug, Clone)]
pub struct ResumeSource {
    pub child_id: String,
    pub child_session_id: String,
    pub child_cwd: String,
    pub worktree_path: Option<String>,
    pub snapshot_ref: Option<String>,
    pub agent_type: String,
    pub persona: Option<String>,
    pub model_id: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ResumeLookup {
    /// The source is still running; resuming it would fork a live child.
    Active,
    Completed(ResumeSource),
    Missing,
}

// ── Type validation / description ────────────────────────────────────────

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ValidateTypeOutcome {
    Ok,
    /// The type does not resolve. `available` is sorted.
    Unknown {
        available: Vec<String>,
    },
    Disabled,
    NotAllowed {
        allowed: Vec<String>,
    },
    /// Could not be checked. Distinct from `Unknown`: the type may be fine.
    ValidationUnavailable,
}

pub struct ValidateTypeCommand {
    pub agent_type: String,
    pub parent_session_id: String,
    pub respond_to: oneshot::Sender<ValidateTypeOutcome>,
}

impl fmt::Debug for ValidateTypeCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ValidateTypeCommand")
            .field("agent_type", &self.agent_type)
            .field("parent_session_id", &self.parent_session_id)
            .finish_non_exhaustive()
    }
}

/// Outcome of describing an agent type without spawning it.
///
/// Mirrors [`ValidateTypeOutcome`] one variant at a time, plus `Unavailable`
/// for infrastructure trouble, so a caller maps every variant to a reason.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum DescribeOutcome {
    Ok(ChildTypeSummary),
    Unknown {
        available: Vec<String>,
    },
    NotAllowed {
        allowed: Vec<String>,
    },
    Disabled,
    /// Could not be obtained — treat as fail-open.
    Unavailable,
}

/// What a child of some agent type would be able to do.
///
/// Built by the runner from the same resolution a real spawn performs, so the
/// described tools are the tools the child would actually get. Tool names are
/// keyed by the host's own tool-kind spelling; this crate has no tool registry
/// and does not interpret them.
#[derive(Debug, Clone, Default)]
pub struct ChildTypeSummary {
    pub tool_names: std::collections::BTreeMap<String, String>,
    pub can_read: bool,
    pub can_search: bool,
    pub can_execute: bool,
}

pub struct DescribeTypeCommand {
    pub agent_type: String,
    /// Host override deciding which harness flavour resolves the toolset.
    /// `None` means the parent decides.
    pub harness_agent_type: Option<String>,
    pub parent_session_id: String,
    pub respond_to: oneshot::Sender<DescribeOutcome>,
}

impl fmt::Debug for DescribeTypeCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DescribeTypeCommand")
            .field("agent_type", &self.agent_type)
            .field("harness_agent_type", &self.harness_agent_type)
            .field("parent_session_id", &self.parent_session_id)
            .finish_non_exhaustive()
    }
}

// ── The command channel ──────────────────────────────────────────────────

/// Every message the coordinator accepts.
///
/// Exhaustive on purpose: adding a command forces every match to account for
/// it, and one enum down one channel is what makes the actor a single writer.
#[derive(Debug)]
pub enum CoordinatorCommand {
    Spawn(SpawnCommand),
    Query(QueryCommand),
    Cancel(CancelCommand),
    ListActive(ListActiveCommand),
    ListRunning(ListRunningCommand),
    Completions(CompletionsCommand),
    /// Fire-and-forget: drop buffered completions owned by a session that no
    /// longer exists, so an unloaded session cannot leak into the buffer.
    DiscardSessionCompletions {
        parent_session_id: String,
    },
    Outstanding(OutstandingCommand),
    RegistryCounts(RegistryCountsCommand),
    Inspect(InspectCommand),
    SpawnedRefs(SpawnedRefsCommand),
    ValidateType(ValidateTypeCommand),
    DescribeType(DescribeTypeCommand),
    LoopUnitActive(LoopUnitActiveCommand),
}

/// Clonable handle for sending coordinator commands.
#[derive(Clone)]
pub struct CommandSender(pub mpsc::UnboundedSender<CoordinatorCommand>);

impl fmt::Debug for CommandSender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandSender").finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChildCompletionSummary, ChildOutcome, ChildSnapshot, ChildStatus, CommandSender,
        CompletionsCommand, CoordinatorCommand,
    };

    fn running() -> ChildStatus {
        ChildStatus::Running {
            turn_count: 0,
            tool_call_count: 0,
            tokens_used: 0,
            context_window_tokens: 0,
            context_usage_pct: 0,
            tools_used: vec![],
            error_count: 0,
        }
    }

    fn finished(outcome: ChildOutcome) -> ChildStatus {
        ChildStatus::Finished {
            outcome,
            output: "done".into(),
            detail: None,
            tool_calls: 1,
            turns: 1,
            tokens_used: 0,
            output_tokens_used: 0,
            total_tokens_used: 0,
            worktree_path: None,
        }
    }

    #[test]
    fn every_outcome_is_terminal() {
        for outcome in [
            ChildOutcome::Completed,
            ChildOutcome::Failed,
            ChildOutcome::Cancelled,
            ChildOutcome::TimedOut,
            ChildOutcome::Lost,
        ] {
            let status = finished(outcome);
            assert!(status.is_terminal(), "{outcome:?} must be terminal");
            assert_eq!(status.outcome(), Some(outcome));
        }
    }

    #[test]
    fn in_flight_states_are_not_terminal() {
        assert!(!running().is_terminal());
        assert!(!ChildStatus::Initializing.is_terminal());
        assert_eq!(running().outcome(), None);
    }

    #[test]
    fn snapshot_is_running_covers_initializing() {
        let snapshot = |status| ChildSnapshot {
            child_id: "c".into(),
            description: "d".into(),
            agent_type: "explore".into(),
            status,
            started_at_epoch_ms: 0,
            duration_ms: 0,
            persona: None,
        };
        assert!(snapshot(ChildStatus::Initializing).is_running());
        assert!(snapshot(running()).is_running());
        assert!(!snapshot(finished(ChildOutcome::Completed)).is_running());
    }

    #[test]
    fn completions_command_round_trips_through_the_channel() {
        use tokio::sync::{mpsc, oneshot};

        let (tx, mut rx) = mpsc::unbounded_channel::<CoordinatorCommand>();
        let sender = CommandSender(tx);
        let (respond_to, mut response_rx) = oneshot::channel();

        sender
            .0
            .send(CoordinatorCommand::Completions(CompletionsCommand {
                parent_session_id: Some("parent".into()),
                suppress_ids: vec!["id-1".into()],
                respond_to,
            }))
            .unwrap();

        let command = rx.try_recv().unwrap();
        let CoordinatorCommand::Completions(request) = command else {
            panic!("expected Completions");
        };
        assert_eq!(request.parent_session_id.as_deref(), Some("parent"));
        assert_eq!(request.suppress_ids, vec!["id-1"]);

        request
            .respond_to
            .send(vec![ChildCompletionSummary {
                child_id: "sub-1".into(),
                agent_type: "explore".into(),
                description: "test task".into(),
                outcome: ChildOutcome::Completed,
                duration_ms: 1500,
                tool_calls: 7,
                turns: 3,
                output: std::sync::Arc::from("child answer"),
            }])
            .unwrap();

        let delivered = response_rx.try_recv().unwrap();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].child_id, "sub-1");
        assert!(delivered[0].outcome.is_success());
        assert_eq!(delivered[0].duration_ms, 1500);
    }
}

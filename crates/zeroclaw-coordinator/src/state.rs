// Derived from grok-build (Apache-2.0), revision
// 1adcd1f477870e4a97bacbd6be78c8a3bfbac46d, from
// `.../grok_build/task/coordinator_state.rs`.
// Copyright 2023-2026 SpaceXAI. Licensed under the Apache License, Version 2.0.
//
// This file was CHANGED by ZeroClaw Labs: the workflow owner and its
// outstanding-count helper were removed (owner is the parent session/task
// only); results speak ZeroClaw's `ChildOutcome` instead of success/cancelled
// booleans; the three terminal snapshot variants collapsed into one
// `ChildStatus::Finished`; upstream's `tracing` calls were dropped when this
// crate had no logging dependency, and one of them (the auto-background
// warning in `background_at_deadline`) is restored here through
// `zeroclaw_log::record!` now that the wiring phase has taken that
// dependency; the panic guard that upstream got from
// `futures::FutureExt::catch_unwind` is implemented here instead, because this
// crate's `futures-util` is built without the `std` feature that provides it.
// See ../LICENSE and ../NOTICE.

//! State owned by the coordinator, and the seam a host plugs into.

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::sync::{mpsc, oneshot};

use crate::cancel::CancelToken;
use crate::outcome::ChildOutcome;
use crate::types::{
    ActiveChildSummary, ChildCompletionSummary, ChildInspection, ChildRequest, ChildResult,
    ChildSnapshot, ChildStatus, DescribeOutcome, ResumeLookup, ValidateTypeOutcome,
};

/// Cap on retained finished-child records before the oldest are evicted.
///
/// A coordinator that never forgets is a memory leak with a long fuse; a
/// coordinator that forgets too eagerly answers "not found" for work that just
/// finished. This is the compromise, and it is enforced every tick.
pub const MAX_COMPLETED_ENTRIES: usize = 1024;

/// Cap on completion summaries buffered for parents that have not drained.
///
/// A session that goes away without draining must not be able to grow this
/// without bound; past the cap the oldest entries are dropped.
pub const MAX_PENDING_COMPLETIONS: usize = 256;

pub(crate) const OUTPUT_UNAVAILABLE_PLACEHOLDER: &str = "[child output no longer available]";

pub type LocalBoxFuture<T> = Pin<Box<dyn Future<Output = T> + 'static>>;
pub type SendBoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Live progress for one active child, as the runtime sees it.
#[derive(Debug, Clone, Default)]
pub struct ChildProgress {
    pub turn_count: u32,
    pub tool_call_count: u32,
    pub tokens_used: u64,
    pub context_window_tokens: u64,
    pub context_usage_pct: u8,
    pub tools_used: Vec<String>,
    pub error_count: u32,
}

/// Runtime handle the coordinator retains while a child is active.
pub trait ChildControl: 'static {
    type ProgressFuture: Future<Output = ChildProgress> + 'static;

    fn progress(&self) -> Self::ProgressFuture;
    fn cancel(&self);
}

/// What the runner reports once initialization produced a live child.
pub struct StartedChild<C> {
    pub child_session_id: String,
    pub persona: Option<String>,
    pub resumed_from: Option<String>,
    pub child_cwd: String,
    pub worktree_path: Option<String>,
    pub effective_model_id: String,
    /// The resolved agent definition declares itself background work.
    ///
    /// Such a child is background for outstanding-work accounting even while
    /// the spawn caller is blocked on its result; the foreground budget stays
    /// keyed to the caller's own request.
    pub definition_background: bool,
    pub control: C,
}

/// Input to one runtime-specific child run.
pub struct ChildRunRequest<C> {
    pub request: ChildRequest,
    pub cancellation: CancelToken,
    pub reporter: ChildReporter<C>,
}

/// Terminal output from one runtime-specific child run.
pub struct ChildRunOutput<D> {
    pub result: ChildResult,
    pub completion_data: D,
    pub snapshot_ref: Option<String>,
}

/// Who was told about a child's ending, decided by the coordinator.
#[derive(Debug, Clone)]
pub struct CompletionDisposition {
    /// The spawn caller received the result inline.
    pub foreground_delivered: bool,
    /// Nobody was waiting inline; the child ran as background work.
    pub backgrounded: bool,
    /// A blocking query was waiting and got the snapshot.
    pub waiter_delivered: bool,
    /// Someone asked for this child to be killed.
    pub explicitly_killed: bool,
    /// Nobody has been told yet, so the host should surface it.
    pub should_surface: bool,
}

/// Terminal event handed to the runner after state is committed.
pub struct ChildCompletion<D> {
    pub request: ChildRequest,
    pub result: ChildResult,
    pub completion_data: D,
    pub disposition: CompletionDisposition,
}

/// The only host-specific seam.
///
/// The associated futures carry no unconditional `Send` bound: a
/// single-threaded runner may return non-`Send` futures, and a multithreaded
/// one may return `Send` futures. The coordinator inherits whichever it gets.
pub trait ChildRunner: 'static {
    type Control: ChildControl;
    type CompletionData: Default + 'static;
    type RunFuture: Future<Output = ChildRunOutput<Self::CompletionData>> + 'static;
    type ValidateFuture: Future<Output = ValidateTypeOutcome> + 'static;
    type DescribeFuture: Future<Output = DescribeOutcome> + 'static;

    fn run(&self, request: ChildRunRequest<Self::Control>) -> Self::RunFuture;

    fn validate_type(&self, agent_type: String, parent_session_id: String) -> Self::ValidateFuture;

    fn describe_type(
        &self,
        agent_type: String,
        harness_agent_type: Option<String>,
        parent_session_id: String,
    ) -> Self::DescribeFuture;

    fn on_completed(&self, completion: ChildCompletion<Self::CompletionData>);

    fn running_count_changed(&self, _running: usize) {}

    fn persisted_output_ref(&self, _completion_data: &Self::CompletionData) -> Option<String> {
        None
    }

    fn load_persisted_output(&self, _reference: &str) -> Option<Arc<str>> {
        None
    }
}

/// Host-configurable policy. The transition logic itself is not configurable.
#[derive(Debug, Clone)]
pub struct CoordinatorConfig {
    /// How long a spawn caller is made to wait inline before the child is
    /// handed off as background work. The child is not stopped.
    pub foreground_budget: std::time::Duration,
    /// Whether the host drains completion summaries between turns.
    pub buffer_completions: bool,
    /// Extra cap applied to buffered summaries only; the request's own
    /// `completion_output_cap` still applies first. Buffered entries pin the
    /// child's output until drained, so a host whose reminder never inlines the
    /// output should bound it. `None` keeps outputs verbatim — a host with no
    /// polling tool needs that, because the inline reminder is the parent's
    /// only chance to see the output.
    pub buffered_completion_output_cap: Option<usize>,
    /// Deepest spawn generation the actor will admit, counting a top-level
    /// child as depth 0.
    ///
    /// The number is inherited from the delegation path, which is the only
    /// place in this workspace that already enforces a recursion depth:
    /// `DelegateTool::resolve_max_depth`
    /// (`zeroclaw-runtime/src/tools/delegate.rs`) falls back to `3` when the
    /// runtime profile names no `max_delegation_depth`, and its own gate
    /// compares the *caller's* depth against it, so the deepest child that
    /// can exist there carries depth == the configured maximum. This gate
    /// compares the *child's* resolved depth against the same bound and so
    /// admits exactly the same set of generations.
    ///
    /// `0` disables the gate, matching `resolve_max_depth`'s
    /// `.filter(|&d| d > 0)` treatment of a zero profile value and
    /// `DelegateTool::at_background_capacity`'s "`cap == 0` disables the
    /// backstop".
    pub max_spawn_depth: u32,
    /// Runaway backstop: how many children may be pending or active at once.
    ///
    /// Inherited verbatim from
    /// `DelegateTool::MAX_CONCURRENT_BACKGROUND_DELEGATIONS = 128`
    /// (`zeroclaw-runtime/src/tools/delegate.rs`), whose rationale applies
    /// unchanged here: each child is a full agent loop, so an unbounded
    /// spawner — a runaway loop or a model that keeps calling the tool — must
    /// hit a wall somewhere. Normal use stays well under it.
    ///
    /// `0` disables the backstop, same convention as
    /// `DelegateTool::at_background_capacity`.
    pub max_concurrent_children: usize,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            foreground_budget: std::time::Duration::from_secs(45),
            buffer_completions: false,
            buffered_completion_output_cap: None,
            max_spawn_depth: 3,
            max_concurrent_children: 128,
        }
    }
}

/// Pure predicate for the spawn-depth gate — separated from the actor's live
/// registry read so it is unit-testable, the way delegation's own backstop
/// predicate is.
///
/// `depth` is the depth of the child being admitted, not its parent's:
/// `ChildOverrides::spawn_depth` is what the sole existing reader of that
/// field files as the child's own `TaskRecord.depth`
/// (`zeroclaw-runtime/src/control_plane/subagent_persistence.rs`). A child at
/// exactly `max` is therefore admitted and only its would-be child is not —
/// which is the same frontier delegation draws when it refuses a caller whose
/// own depth has already reached `max_delegation_depth`.
#[must_use]
pub fn exceeds_spawn_depth(depth: u32, max: u32) -> bool {
    max != 0 && depth > max
}

/// Pure predicate for the concurrency backstop, mirroring
/// `DelegateTool::at_background_capacity` including its `cap == 0` escape.
///
/// `in_flight` is the count *before* admitting this request, so the request
/// that brings the registry to exactly `cap` is admitted and the next one is
/// not.
#[must_use]
pub fn at_child_capacity(in_flight: usize, cap: usize) -> bool {
    cap != 0 && in_flight >= cap
}

/// The runner's channel back into the actor.
pub struct ChildReporter<C> {
    pub(crate) child_id: String,
    pub(crate) tx: mpsc::UnboundedSender<InternalEvent<C>>,
}

impl<C> Clone for ChildReporter<C> {
    fn clone(&self) -> Self {
        Self {
            child_id: self.child_id.clone(),
            tx: self.tx.clone(),
        }
    }
}

impl<C: 'static> ChildReporter<C> {
    /// Promote the pending child to active.
    ///
    /// The acknowledgement closes the cancel-at-promote race: `false` means
    /// cancellation won while the child was being built, and the runner must
    /// tear down the half-initialized runtime instead of handing it over.
    pub async fn started(&self, child: StartedChild<C>) -> bool {
        let (respond_to, response_rx) = oneshot::channel();
        if self
            .tx
            .send(InternalEvent::Started {
                child_id: self.child_id.clone(),
                child,
                respond_to,
            })
            .is_err()
        {
            return false;
        }
        response_rx.await.unwrap_or(false)
    }

    /// Resolve an in-memory resume source without sharing coordinator state.
    pub async fn resume_source(&self, source_id: &str, parent_session_id: &str) -> ResumeLookup {
        let (respond_to, response_rx) = oneshot::channel();
        if self
            .tx
            .send(InternalEvent::ResumeSource {
                source_id: source_id.to_owned(),
                parent_session_id: parent_session_id.to_owned(),
                respond_to,
            })
            .is_err()
        {
            return ResumeLookup::Missing;
        }
        response_rx.await.unwrap_or(ResumeLookup::Missing)
    }
}

pub(crate) enum InternalEvent<C> {
    Started {
        child_id: String,
        child: StartedChild<C>,
        respond_to: oneshot::Sender<bool>,
    },
    ResumeSource {
        source_id: String,
        parent_session_id: String,
        respond_to: oneshot::Sender<ResumeLookup>,
    },
}

pub(crate) struct PendingChild {
    pub(crate) request: ChildRequest,
    pub(crate) started_at: std::time::Instant,
    pub(crate) cancellation: CancelToken,
    pub(crate) spawn_reply: Option<oneshot::Sender<ChildResult>>,
    pub(crate) foreground_deadline: Option<tokio::time::Instant>,
    pub(crate) handle_only: bool,
    pub(crate) explicitly_killed: bool,
}

pub(crate) struct ActiveChild<C> {
    pub(crate) request: ChildRequest,
    pub(crate) started_at: std::time::Instant,
    pub(crate) cancellation: CancelToken,
    pub(crate) spawn_reply: Option<oneshot::Sender<ChildResult>>,
    pub(crate) foreground_deadline: Option<tokio::time::Instant>,
    pub(crate) handle_only: bool,
    /// See [`StartedChild::definition_background`].
    pub(crate) definition_background: bool,
    pub(crate) explicitly_killed: bool,
    pub(crate) child_session_id: String,
    pub(crate) persona: Option<String>,
    pub(crate) resumed_from: Option<String>,
    pub(crate) child_cwd: String,
    pub(crate) worktree_path: Option<String>,
    pub(crate) effective_model_id: String,
    pub(crate) control: C,
}

pub(crate) struct CompletedChild {
    pub(crate) request: ChildRequest,
    pub(crate) started_at: std::time::Instant,
    pub(crate) child_session_id: String,
    pub(crate) persona: Option<String>,
    pub(crate) resumed_from: Option<String>,
    pub(crate) child_cwd: String,
    pub(crate) worktree_path: Option<String>,
    pub(crate) snapshot_ref: Option<String>,
    pub(crate) persisted_output_ref: Option<String>,
    pub(crate) effective_model_id: String,
    pub(crate) result: ChildResult,
}

pub(crate) struct BlockingWaiter {
    pub(crate) deadline: tokio::time::Instant,
    pub(crate) respond_to: oneshot::Sender<Option<ChildSnapshot>>,
}

pub(crate) struct BufferedCompletion {
    pub(crate) parent_session_id: String,
    pub(crate) summary: ChildCompletionSummary,
}

/// One child run, tagged with its id and wrapped in a panic guard.
///
/// A child that panics must not take the actor down with it: the actor is the
/// single writer for every *other* child too, so its death is a fleet outage.
/// The panic is caught here, at the poll, and reported as an ordinary terminal
/// output the coordinator can account for.
///
/// ## This guard holds only where unwinding is enabled
///
/// `catch_unwind` catches a panic only if the panic unwinds. The ZeroClaw
/// workspace sets `panic = "abort"` in its release profiles (root `Cargo.toml`,
/// `[profile.release]` and `[profile.release-fast]`), so in a release binary a
/// panicking child aborts the process before this code runs and the guard is
/// dead weight. It is real in dev and test builds, which is where the actor's
/// own bookkeeping gets exercised.
///
/// Read this as "the actor's state machine is panic-safe", not "a shipped
/// daemon survives a panicking child". The containment that survives every
/// profile is running a child in another process, where the OS is the guard —
/// which is the shape this coordinator is expected to drive.
pub(crate) struct ChildRunFuture<F> {
    pub(crate) child_id: String,
    pub(crate) future: Pin<Box<F>>,
    pub(crate) finished: bool,
}

/// A child run either produced output or panicked.
pub(crate) type ChildRunResult<T> = Result<T, ChildPanicked>;

/// Marker for a child whose future unwound. The payload is deliberately
/// dropped: it is arbitrary attacker-influenced data, and nothing here needs it.
pub(crate) struct ChildPanicked;

impl<F: Future> Future for ChildRunFuture<F> {
    type Output = (String, ChildRunResult<F::Output>);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.finished {
            // A panicked future is never polled again: its state is unknown.
            return Poll::Pending;
        }
        let future = &mut this.future;
        let polled = std::panic::catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(cx)));
        match polled {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(output)) => {
                this.finished = true;
                Poll::Ready((this.child_id.clone(), Ok(output)))
            }
            Err(_payload) => {
                this.finished = true;
                Poll::Ready((this.child_id.clone(), Err(ChildPanicked)))
            }
        }
    }
}

pub(crate) struct ReplyFuture<F, T> {
    pub(crate) future: Pin<Box<F>>,
    pub(crate) respond_to: Option<oneshot::Sender<T>>,
}

impl<F, T> Future for ReplyFuture<F, T>
where
    F: Future<Output = T>,
{
    type Output = (oneshot::Sender<T>, T);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.future.as_mut().poll(cx).map(|output| {
            let respond_to = match this.respond_to.take() {
                Some(respond_to) => respond_to,
                None => unreachable!("reply future polled after completion"),
            };
            (respond_to, output)
        })
    }
}

#[derive(Clone)]
pub(crate) struct RunningSeed {
    pub(crate) child_id: String,
    pub(crate) description: String,
    pub(crate) agent_type: String,
    pub(crate) started_at_epoch_ms: u64,
    pub(crate) duration_ms: u64,
    pub(crate) persona: Option<String>,
    pub(crate) parent_session_id: String,
    pub(crate) child_session_id: String,
    pub(crate) fork_parent_prompt_id: Option<String>,
    pub(crate) resumed_from: Option<String>,
}

pub(crate) enum ProgressTarget {
    Query(oneshot::Sender<Option<ChildSnapshot>>),
    Inspect(oneshot::Sender<Option<ChildInspection>>),
    List { request_id: u64, index: usize },
}

pub(crate) struct ProgressFuture<F> {
    pub(crate) future: Pin<Box<F>>,
    pub(crate) seed: Option<RunningSeed>,
    pub(crate) target: Option<ProgressTarget>,
}

impl<F> Future for ProgressFuture<F>
where
    F: Future<Output = ChildProgress>,
{
    type Output = (RunningSeed, ProgressTarget, ChildProgress);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.future.as_mut().poll(cx).map(|progress| {
            let seed = match this.seed.take() {
                Some(seed) => seed,
                None => unreachable!("progress future polled without a seed"),
            };
            let target = match this.target.take() {
                Some(target) => target,
                None => unreachable!("progress future polled without a target"),
            };
            (seed, target, progress)
        })
    }
}

pub(crate) struct ListRequest {
    pub(crate) slots: Vec<Option<ChildInspection>>,
    pub(crate) remaining: usize,
    pub(crate) respond_to: oneshot::Sender<Vec<ChildInspection>>,
}

pub(crate) enum ChildRecord<C> {
    Pending(PendingChild),
    Active(ActiveChild<C>),
}

impl<C> ChildRecord<C> {
    pub(crate) fn request(&self) -> &ChildRequest {
        match self {
            Self::Pending(child) => &child.request,
            Self::Active(child) => &child.request,
        }
    }

    pub(crate) fn explicitly_killed(&self) -> bool {
        match self {
            Self::Pending(child) => child.explicitly_killed,
            Self::Active(child) => child.explicitly_killed,
        }
    }
}

/// The half of a child's record the foreground-handoff rules need, shared by
/// pending and active children so the rules cannot drift apart.
pub(crate) trait ForegroundChild {
    fn id(&self) -> &str;
    fn child_session_id(&self) -> &str;
    fn deadline(&self) -> Option<tokio::time::Instant>;
    /// True when the spawn caller dropped its result receiver while this child
    /// was still treated as turn-blocking.
    fn caller_gone(&self) -> bool;
    fn take_reply(&mut self) -> Option<oneshot::Sender<ChildResult>>;
    fn mark_backgrounded(&mut self);
}

impl ForegroundChild for PendingChild {
    fn id(&self) -> &str {
        &self.request.child_id
    }

    fn child_session_id(&self) -> &str {
        &self.request.child_id
    }

    fn deadline(&self) -> Option<tokio::time::Instant> {
        self.foreground_deadline
    }

    fn caller_gone(&self) -> bool {
        !self.handle_only && self.spawn_reply.as_ref().is_some_and(|tx| tx.is_closed())
    }

    fn take_reply(&mut self) -> Option<oneshot::Sender<ChildResult>> {
        self.spawn_reply.take()
    }

    fn mark_backgrounded(&mut self) {
        self.handle_only = true;
        self.foreground_deadline = None;
    }
}

impl<C: ChildControl> ForegroundChild for ActiveChild<C> {
    fn id(&self) -> &str {
        &self.request.child_id
    }

    fn child_session_id(&self) -> &str {
        &self.child_session_id
    }

    fn deadline(&self) -> Option<tokio::time::Instant> {
        self.foreground_deadline
    }

    fn caller_gone(&self) -> bool {
        !self.handle_only && self.spawn_reply.as_ref().is_some_and(|tx| tx.is_closed())
    }

    fn take_reply(&mut self) -> Option<oneshot::Sender<ChildResult>> {
        self.spawn_reply.take()
    }

    fn mark_backgrounded(&mut self) {
        self.handle_only = true;
        self.foreground_deadline = None;
    }
}

/// Hand the spawn caller a handle once the foreground budget is spent.
///
/// The child is NOT stopped — it keeps running and its real ending arrives
/// later. That is why the interim reply must not be recordable as a completion.
pub(crate) fn background_at_deadline(
    child: &mut impl ForegroundChild,
    now: tokio::time::Instant,
    _budget: std::time::Duration,
) {
    if child.deadline().is_none_or(|deadline| deadline > now) {
        return;
    }
    if let Some(respond_to) = child.take_reply() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Timeout)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({
                    "child_id": child.id(),
                    "child_session_id": child.child_session_id(),
                })),
            "coordinator: spawn caller's foreground budget elapsed; handing off a \
             background handle while the child keeps running"
        );
        // An interim handoff, not an ending. The default outcome is
        // `Lost` — never `Completed` — so no consumer of this reply can record
        // a finished status for a child that is still running. Callers branch
        // on `backgrounded`.
        let _ = respond_to.send(ChildResult {
            backgrounded: true,
            child_id: child.id().to_owned(),
            child_session_id: child.child_session_id().to_owned(),
            ..Default::default()
        });
    }
    child.mark_backgrounded();
}

/// Handle a foreground child whose spawn caller dropped the result channel —
/// the parent turn was stopped, or the await was abandoned.
///
/// The child keeps running and simply leaves the turn-blocking set. Killing it
/// would throw away work the parent may still want; the completion still
/// surfaces later.
pub(crate) fn background_if_caller_gone(child: &mut impl ForegroundChild) {
    if !child.caller_gone() {
        return;
    }
    let _ = child.take_reply();
    child.mark_backgrounded();
}

pub(crate) async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn instant_to_epoch_ms(instant: std::time::Instant) -> u64 {
    let now_instant = std::time::Instant::now();
    let now_system = std::time::SystemTime::now();
    let elapsed = now_instant.saturating_duration_since(instant);
    now_system
        .checked_sub(elapsed)
        .unwrap_or(now_system)
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(crate) fn active_summary<C>(child: &ActiveChild<C>) -> ActiveChildSummary {
    ActiveChildSummary {
        child_id: child.request.child_id.clone(),
        agent_type: child.request.agent_type.clone(),
        description: child.request.description.clone(),
        elapsed_ms: child.started_at.elapsed().as_millis() as u64,
    }
}

pub(crate) fn running_seed<C>(child: &ActiveChild<C>) -> RunningSeed {
    RunningSeed {
        child_id: child.request.child_id.clone(),
        description: child.request.description.clone(),
        agent_type: child.request.agent_type.clone(),
        started_at_epoch_ms: instant_to_epoch_ms(child.started_at),
        duration_ms: child.started_at.elapsed().as_millis() as u64,
        persona: child.persona.clone(),
        parent_session_id: child.request.parent_session_id.clone(),
        child_session_id: child.child_session_id.clone(),
        fork_parent_prompt_id: child.request.parent_prompt_id.clone(),
        resumed_from: child.resumed_from.clone(),
    }
}

pub(crate) fn running_inspection(seed: RunningSeed, progress: ChildProgress) -> ChildInspection {
    ChildInspection {
        snapshot: ChildSnapshot {
            child_id: seed.child_id,
            description: seed.description,
            agent_type: seed.agent_type,
            status: ChildStatus::Running {
                turn_count: progress.turn_count,
                tool_call_count: progress.tool_call_count,
                tokens_used: progress.tokens_used,
                context_window_tokens: progress.context_window_tokens,
                context_usage_pct: progress.context_usage_pct,
                tools_used: progress.tools_used,
                error_count: progress.error_count,
            },
            started_at_epoch_ms: seed.started_at_epoch_ms,
            duration_ms: seed.duration_ms,
            persona: seed.persona,
        },
        parent_session_id: seed.parent_session_id,
        child_session_id: seed.child_session_id,
        fork_parent_prompt_id: seed.fork_parent_prompt_id,
        resumed_from: seed.resumed_from,
    }
}

pub(crate) fn pending_snapshot(child: &PendingChild) -> ChildSnapshot {
    ChildSnapshot {
        child_id: child.request.child_id.clone(),
        description: child.request.description.clone(),
        agent_type: child.request.agent_type.clone(),
        status: ChildStatus::Initializing,
        started_at_epoch_ms: instant_to_epoch_ms(child.started_at),
        duration_ms: child.started_at.elapsed().as_millis() as u64,
        persona: child.request.overrides.persona.clone(),
    }
}

pub(crate) fn pending_inspection(child: &PendingChild) -> ChildInspection {
    ChildInspection {
        snapshot: pending_snapshot(child),
        parent_session_id: child.request.parent_session_id.clone(),
        child_session_id: String::new(),
        fork_parent_prompt_id: child.request.parent_prompt_id.clone(),
        resumed_from: child.request.resume_from.clone(),
    }
}

pub(crate) fn completed_snapshot(
    child: &CompletedChild,
    persisted_output: Option<&str>,
) -> ChildSnapshot {
    ChildSnapshot {
        child_id: child.request.child_id.clone(),
        description: child.request.description.clone(),
        agent_type: child.request.agent_type.clone(),
        status: ChildStatus::Finished {
            outcome: child.result.outcome,
            output: persisted_output
                .map(str::to_owned)
                .unwrap_or_else(|| child.result.output.to_string()),
            detail: child.result.detail.clone(),
            tool_calls: child.result.tool_calls,
            turns: child.result.turns,
            worktree_path: child.result.worktree_path.clone(),
        },
        started_at_epoch_ms: instant_to_epoch_ms(child.started_at),
        duration_ms: child.result.duration_ms,
        persona: child.persona.clone(),
    }
}

pub(crate) fn completed_inspection(
    child: &CompletedChild,
    persisted_output: Option<&str>,
) -> ChildInspection {
    ChildInspection {
        snapshot: completed_snapshot(child, persisted_output),
        parent_session_id: child.request.parent_session_id.clone(),
        child_session_id: child.child_session_id.clone(),
        fork_parent_prompt_id: child.request.parent_prompt_id.clone(),
        resumed_from: child.resumed_from.clone(),
    }
}

/// Truncate `output` to `cap` bytes (UTF-8 safe) with a truncation footer.
/// Returns a refcount clone when it already fits.
#[must_use]
pub fn cap_completion_output(output: &Arc<str>, cap: usize) -> Arc<str> {
    if output.len() <= cap {
        return output.clone();
    }
    let mut end = cap;
    while end > 0 && !output.is_char_boundary(end) {
        end -= 1;
    }
    Arc::from(format!(
        "{}\n[output truncated: {} of {} bytes shown]",
        &output[..end],
        end,
        output.len()
    ))
}

/// Parent-facing summary of a finished child, honouring the request's own
/// output cap. Shared by the buffered-reminder path and any host that wakes a
/// parent directly.
#[must_use]
pub fn completion_summary(request: &ChildRequest, result: &ChildResult) -> ChildCompletionSummary {
    let output = match request.overrides.completion_output_cap {
        Some(cap) => cap_completion_output(&result.output, cap),
        None => result.output.clone(),
    };
    ChildCompletionSummary {
        child_id: request.child_id.clone(),
        agent_type: request.agent_type.clone(),
        description: request.description.clone(),
        outcome: result.outcome,
        duration_ms: result.duration_ms,
        tool_calls: result.tool_calls,
        turns: result.turns,
        output,
    }
}

/// Result for a child whose runtime unwound.
pub(crate) fn panicked_result(request: &ChildRequest) -> ChildResult {
    ChildResult {
        outcome: ChildOutcome::Failed,
        detail: Some("child runtime panicked".to_owned()),
        child_id: request.child_id.clone(),
        child_session_id: request.child_id.clone(),
        ..Default::default()
    }
}

/// Bound `completed` by evicting the oldest ids first.
pub(crate) fn evict_completed(
    completed: &mut HashMap<String, CompletedChild>,
    order: &mut std::collections::VecDeque<String>,
) {
    while completed.len() > MAX_COMPLETED_ENTRIES {
        let Some(id) = order.pop_front() else {
            break;
        };
        completed.remove(&id);
    }
}

// Derived from grok-build (Apache-2.0), revision
// 1adcd1f477870e4a97bacbd6be78c8a3bfbac46d, from
// `.../grok_build/task/coordinator_tests.rs`.
// Copyright 2023-2026 SpaceXAI. Licensed under the Apache License, Version 2.0.
//
// This file was CHANGED by ZeroClaw Labs: assertions were moved onto this
// crate's outcome vocabulary; the workflow-owner test was dropped with the
// workflow owner, and the usage-accounting halves of the outstanding test went
// with the usage commands; four tests that upstream did not have were ADDED,
// for cancel-at-promote, child-panic containment, the buffered-completion
// bound, and (wiring phase 2b) a recording `ChildPersistence` mock covering
// `record_spawn`/`record_finish` call counts, the `delivered` flag, and a
// persistence backend that errors on every call. See ../LICENSE and
// ../NOTICE.

// Tests use bare `tokio::spawn` to drive coordinator actors — the fork's
// `disallowed_methods`/`disallowed_macros` lints target production code,
// not test harnesses.
#![allow(clippy::disallowed_methods, clippy::disallowed_macros)]

use super::*;
use crate::backend::{ChannelBackend, CoordinatorError};
use crate::cancel::CancelToken;
use crate::outcome::ChildOutcome;
use crate::persistence::PersistenceError;
use crate::state::{
    ChildProgress, MAX_COMPLETED_ENTRIES, MAX_PENDING_COMPLETIONS, SendBoxFuture, StartedChild,
};
use crate::types::{
    CancelOutcome, ChildCompletionSummary, ChildStatus, CompletionsCommand, ListActiveCommand,
    LoopUnitActiveCommand, OutstandingCommand, OutstandingReply, RegistryCounts, SpawnCommand,
};
use std::sync::Mutex;
use tokio::sync::oneshot;

#[derive(Clone)]
struct TestControl {
    cancellation: CancelToken,
}

impl ChildControl for TestControl {
    type ProgressFuture = std::future::Ready<ChildProgress>;

    fn progress(&self) -> Self::ProgressFuture {
        std::future::ready(ChildProgress {
            turn_count: 2,
            tool_call_count: 3,
            tokens_used: 100,
            context_window_tokens: 1_000,
            context_usage_pct: 10,
            tools_used: vec!["read_file".to_owned()],
            error_count: 0,
        })
    }

    fn cancel(&self) {
        self.cancellation.cancel();
    }
}

#[derive(Clone, Copy, Default)]
struct RunnerOptions {
    /// Hold the child before it reports `started`, so tests can act while it
    /// is still pending.
    wait_before_start: bool,
    /// After observing cancellation, wait for `finish` before returning.
    wait_after_cancel: bool,
    /// Hold the child immediately before it calls `reporter.started`, so a test
    /// can win the cancel-at-promote race deterministically.
    gate_promote: bool,
}

struct TestRunner {
    options: RunnerOptions,
    start: tokio::sync::broadcast::Sender<()>,
    finish: tokio::sync::broadcast::Sender<()>,
    promote_gate: tokio::sync::broadcast::Sender<()>,
    promote_reached: mpsc::UnboundedSender<String>,
    promote_acks: mpsc::UnboundedSender<bool>,
    completions: mpsc::UnboundedSender<CompletionDisposition>,
    requests: mpsc::UnboundedSender<ChildRequest>,
    started: mpsc::UnboundedSender<String>,
}

impl ChildRunner for TestRunner {
    type Control = TestControl;
    type CompletionData = ();
    type RunFuture = SendBoxFuture<ChildRunOutput<()>>;
    type ValidateFuture = SendBoxFuture<ValidateTypeOutcome>;
    type DescribeFuture = SendBoxFuture<DescribeOutcome>;

    fn run(&self, run: ChildRunRequest<Self::Control>) -> Self::RunFuture {
        let options = self.options;
        let mut start = self.start.subscribe();
        let mut finish = self.finish.subscribe();
        let mut promote_gate = self.promote_gate.subscribe();
        let promote_reached = self.promote_reached.clone();
        let promote_acks = self.promote_acks.clone();
        let requests = self.requests.clone();
        let started = self.started.clone();
        Box::pin(async move {
            let ChildRunRequest {
                request,
                cancellation,
                reporter,
            } = run;
            let _ = requests.send(request.clone());
            // A child whose id says so blows up, so the panic-containment test
            // can have a live actor and an exploding child at the same time.
            assert!(
                !request.child_id.starts_with("boom"),
                "child runtime exploded on purpose"
            );
            if options.wait_before_start {
                tokio::select! {
                    () = cancellation.cancelled() => {
                        if options.wait_after_cancel {
                            let _ = finish.recv().await;
                        }
                        return ChildRunOutput {
                            result: cancelled_result(&request),
                            completion_data: (),
                            snapshot_ref: None,
                        };
                    }
                    _ = start.recv() => {}
                }
            }
            if options.gate_promote {
                let _ = promote_reached.send(request.child_id.clone());
                let _ = promote_gate.recv().await;
            }
            let promoted = reporter
                .started(StartedChild {
                    child_session_id: request.child_id.clone(),
                    persona: None,
                    resumed_from: request.resume_from.clone(),
                    child_cwd: request.cwd.clone().unwrap_or_default(),
                    worktree_path: None,
                    effective_model_id: "test-model".to_owned(),
                    // Mock definition resolution: this type declares background.
                    definition_background: request.agent_type == "background-default",
                    control: TestControl {
                        cancellation: cancellation.clone(),
                    },
                })
                .await;
            let _ = promote_acks.send(promoted);
            if !promoted {
                return ChildRunOutput {
                    result: cancelled_result(&request),
                    completion_data: (),
                    snapshot_ref: None,
                };
            }
            let _ = started.send(request.child_id.clone());
            let result = tokio::select! {
                () = cancellation.cancelled() => {
                    if options.wait_after_cancel {
                        let _ = finish.recv().await;
                    }
                    cancelled_result(&request)
                },
                _ = finish.recv() => ChildResult {
                    outcome: ChildOutcome::Completed,
                    output: request.prompt.clone().into(),
                    child_id: request.child_id.clone(),
                    child_session_id: request.child_id.clone(),
                    tool_calls: 3,
                    turns: 2,
                    ..Default::default()
                },
            };
            ChildRunOutput {
                result,
                completion_data: (),
                snapshot_ref: None,
            }
        })
    }

    fn validate_type(
        &self,
        _agent_type: String,
        _parent_session_id: String,
    ) -> Self::ValidateFuture {
        Box::pin(std::future::ready(ValidateTypeOutcome::Ok))
    }

    fn describe_type(
        &self,
        _agent_type: String,
        _harness_agent_type: Option<String>,
        _parent_session_id: String,
    ) -> Self::DescribeFuture {
        Box::pin(std::future::ready(DescribeOutcome::Unavailable))
    }

    fn on_completed(&self, completion: ChildCompletion<Self::CompletionData>) {
        let _ = self.completions.send(completion.disposition);
    }
}

fn cancelled_result(request: &ChildRequest) -> ChildResult {
    ChildResult {
        outcome: ChildOutcome::Cancelled,
        detail: Some("cancelled".to_owned()),
        child_id: request.child_id.clone(),
        child_session_id: request.child_id.clone(),
        ..Default::default()
    }
}

fn request(id: &str, background: bool) -> ChildRequest {
    ChildRequest {
        child_id: id.to_owned(),
        prompt: "work".to_owned(),
        description: "test child".to_owned(),
        agent_type: "explore".to_owned(),
        parent_session_id: "parent".to_owned(),
        parent_alias: "parent-alias".to_owned(),
        parent_prompt_id: Some("prompt".to_owned()),
        resume_from: None,
        cwd: None,
        overrides: Default::default(),
        run_in_background: background,
        surface_completion: true,
        await_to_completion: false,
        fork_context: false,
        cancel_token: CancelToken::new(),
    }
}

struct Harness {
    backend: ChannelBackend,
    start: tokio::sync::broadcast::Sender<()>,
    finish: tokio::sync::broadcast::Sender<()>,
    promote_gate: tokio::sync::broadcast::Sender<()>,
    promote_reached: mpsc::UnboundedReceiver<String>,
    promote_acks: mpsc::UnboundedReceiver<bool>,
    completions: mpsc::UnboundedReceiver<CompletionDisposition>,
    requests: mpsc::UnboundedReceiver<ChildRequest>,
    started: mpsc::UnboundedReceiver<String>,
    actor: tokio::task::JoinHandle<()>,
}

fn harness(wait_before_start: bool, foreground_budget: std::time::Duration) -> Harness {
    harness_with_config(
        wait_before_start,
        CoordinatorConfig {
            foreground_budget,
            ..CoordinatorConfig::default()
        },
    )
}

fn harness_with_config(wait_before_start: bool, config: CoordinatorConfig) -> Harness {
    harness_with_options(
        RunnerOptions {
            wait_before_start,
            ..RunnerOptions::default()
        },
        config,
    )
}

fn harness_with_options(options: RunnerOptions, config: CoordinatorConfig) -> Harness {
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (start, _) = tokio::sync::broadcast::channel(4);
    let (finish, _) = tokio::sync::broadcast::channel(4);
    let (promote_gate, _) = tokio::sync::broadcast::channel(4);
    let (promote_reached_tx, promote_reached) = mpsc::unbounded_channel();
    let (promote_acks_tx, promote_acks) = mpsc::unbounded_channel();
    let (completion_tx, completions) = mpsc::unbounded_channel();
    let (request_tx, requests) = mpsc::unbounded_channel();
    let (started_tx, started) = mpsc::unbounded_channel();
    let actor = tokio::spawn(
        Coordinator::new(
            command_rx,
            TestRunner {
                options,
                start: start.clone(),
                finish: finish.clone(),
                promote_gate: promote_gate.clone(),
                promote_reached: promote_reached_tx,
                promote_acks: promote_acks_tx,
                completions: completion_tx,
                requests: request_tx,
                started: started_tx,
            },
            config,
        )
        .run(),
    );
    Harness {
        backend: ChannelBackend::new(command_tx),
        start,
        finish,
        promote_gate,
        promote_reached,
        promote_acks,
        completions,
        requests,
        started,
        actor,
    }
}

/// Same wiring as [`harness_with_options`], but backed by
/// `Coordinator::with_persistence` instead of `Coordinator::new` — the only
/// difference the persistence-mock tests need.
fn harness_with_persistence(
    options: RunnerOptions,
    config: CoordinatorConfig,
    persistence: RecordingPersistence,
) -> Harness {
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (start, _) = tokio::sync::broadcast::channel(4);
    let (finish, _) = tokio::sync::broadcast::channel(4);
    let (promote_gate, _) = tokio::sync::broadcast::channel(4);
    let (promote_reached_tx, promote_reached) = mpsc::unbounded_channel();
    let (promote_acks_tx, promote_acks) = mpsc::unbounded_channel();
    let (completion_tx, completions) = mpsc::unbounded_channel();
    let (request_tx, requests) = mpsc::unbounded_channel();
    let (started_tx, started) = mpsc::unbounded_channel();
    let actor = tokio::spawn(
        Coordinator::with_persistence(
            command_rx,
            TestRunner {
                options,
                start: start.clone(),
                finish: finish.clone(),
                promote_gate: promote_gate.clone(),
                promote_reached: promote_reached_tx,
                promote_acks: promote_acks_tx,
                completions: completion_tx,
                requests: request_tx,
                started: started_tx,
            },
            config,
            persistence,
        )
        .run(),
    );
    Harness {
        backend: ChannelBackend::new(command_tx),
        start,
        finish,
        promote_gate,
        promote_reached,
        promote_acks,
        completions,
        requests,
        started,
        actor,
    }
}

async fn loop_unit_active(backend: &ChannelBackend, task_id: &str) -> bool {
    let (respond_to, response_rx) = oneshot::channel();
    backend
        .sender()
        .send(CoordinatorCommand::LoopUnitActive(LoopUnitActiveCommand {
            task_id: task_id.to_owned(),
            respond_to,
        }))
        .expect("actor command channel open");
    response_rx.await.expect("loop activity response")
}

async fn outstanding(backend: &ChannelBackend, prompt_id: &str) -> OutstandingReply {
    let (respond_to, response_rx) = oneshot::channel();
    backend
        .sender()
        .send(CoordinatorCommand::Outstanding(OutstandingCommand {
            parent_session_id: "parent".to_owned(),
            prompt_id: prompt_id.to_owned(),
            respond_to,
        }))
        .expect("actor command channel open");
    response_rx.await.expect("outstanding response")
}

async fn drain_completions(backend: &ChannelBackend, parent: &str) -> Vec<ChildCompletionSummary> {
    let (respond_to, response_rx) = oneshot::channel();
    backend
        .sender()
        .send(CoordinatorCommand::Completions(CompletionsCommand {
            parent_session_id: Some(parent.to_owned()),
            suppress_ids: Vec::new(),
            respond_to,
        }))
        .expect("actor command channel open");
    response_rx.await.expect("completion response")
}

fn assert_finished(status: &ChildStatus, expected: ChildOutcome) {
    match status {
        ChildStatus::Finished { outcome, .. } => assert_eq!(*outcome, expected),
        other => panic!("expected a finished child, got {other:?}"),
    }
}

#[tokio::test]
async fn foreground_completion_is_delivered_inline() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("inline", false)).await }
    });
    tokio::task::yield_now().await;
    let _ = harness.finish.send(());

    let result = spawn.await.unwrap().unwrap();
    assert!(result.is_success());
    let disposition = harness.completions.recv().await.unwrap();
    assert!(disposition.foreground_delivered);
    assert!(!disposition.should_surface);
    harness.actor.abort();
}

#[tokio::test(start_paused = true)]
async fn foreground_deadline_hands_off_without_stopping_child() {
    let mut harness = harness(false, std::time::Duration::from_secs(1));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("slow", false)).await }
    });
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    let interim = spawn.await.unwrap().unwrap();
    assert!(interim.backgrounded);
    // The handoff must not read as an ending, least of all a successful one:
    // the child is still running.
    assert!(!interim.is_success());
    assert_eq!(interim.outcome, ChildOutcome::Lost);
    assert_eq!(
        outstanding(&harness.backend, "prompt").await,
        OutstandingReply {
            live_ids: Vec::new(),
            background_live: true,
        }
    );
    assert_eq!(
        harness.backend.registry_counts().await,
        RegistryCounts {
            pending: 0,
            active: 1,
            completed: 0,
        }
    );

    let running = harness.backend.query("slow", false, None).await.unwrap();
    assert!(running.is_running());
    let _ = harness.finish.send(());
    let disposition = harness.completions.recv().await.unwrap();
    assert!(disposition.backgrounded);
    assert!(disposition.should_surface);
    harness.actor.abort();
}

#[tokio::test]
async fn live_blocking_waiter_suppresses_async_surface() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("waited", true)).await }
    });
    tokio::task::yield_now().await;
    let wait = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.query("waited", true, Some(60_000)).await }
    });
    tokio::task::yield_now().await;
    let _ = harness.finish.send(());

    assert!(wait.await.unwrap().unwrap().status.is_terminal());
    let disposition = harness.completions.recv().await.unwrap();
    assert!(disposition.waiter_delivered);
    assert!(!disposition.should_surface);
    assert!(spawn.await.unwrap().unwrap().is_success());
    harness.actor.abort();
}

#[tokio::test(start_paused = true)]
async fn timed_out_waiter_does_not_suppress_later_completion() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("timeout", true)).await }
    });
    tokio::task::yield_now().await;
    let snapshot = harness
        .backend
        .query("timeout", true, Some(1_000))
        .await
        .unwrap();
    assert!(snapshot.is_running());

    let _ = harness.finish.send(());
    let disposition = harness.completions.recv().await.unwrap();
    assert!(!disposition.waiter_delivered);
    assert!(disposition.should_surface);
    assert!(spawn.await.unwrap().unwrap().is_success());
    harness.actor.abort();
}

#[tokio::test(start_paused = true)]
async fn surviving_waiter_suppresses_after_peer_times_out() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("two-waiters", true)).await }
    });
    tokio::task::yield_now().await;
    let short = tokio::spawn({
        let backend = harness.backend.clone();
        async move {
            backend
                .query("two-waiters", true, Some(1_000))
                .await
                .unwrap()
        }
    });
    let long = tokio::spawn({
        let backend = harness.backend.clone();
        async move {
            backend
                .query("two-waiters", true, Some(60_000))
                .await
                .unwrap()
        }
    });
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    assert!(short.await.unwrap().is_running());

    let _ = harness.finish.send(());
    assert!(long.await.unwrap().status.is_terminal());
    let disposition = harness.completions.recv().await.unwrap();
    assert!(disposition.waiter_delivered);
    assert!(!disposition.should_surface);
    assert!(spawn.await.unwrap().unwrap().is_success());
    harness.actor.abort();
}

#[tokio::test]
async fn dropped_waiter_does_not_suppress_completion() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("dropped-wait", true)).await }
    });
    tokio::task::yield_now().await;
    let wait = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.query("dropped-wait", true, Some(60_000)).await }
    });
    tokio::task::yield_now().await;
    wait.abort();
    let _ = wait.await;

    let _ = harness.finish.send(());
    let disposition = harness.completions.recv().await.unwrap();
    assert!(!disposition.waiter_delivered);
    assert!(disposition.should_surface);
    assert!(spawn.await.unwrap().unwrap().is_success());
    harness.actor.abort();
}

#[tokio::test]
async fn pending_cancel_delivers_waiter_once() {
    let mut harness = harness(true, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("pending-cancel", true)).await }
    });
    tokio::task::yield_now().await;
    let wait = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.query("pending-cancel", true, Some(60_000)).await }
    });
    tokio::task::yield_now().await;
    assert!(matches!(
        harness.backend.cancel("pending-cancel").await,
        CancelOutcome::Cancelled
    ));
    let snapshot = wait.await.unwrap().unwrap();
    assert_finished(&snapshot.status, ChildOutcome::Cancelled);
    let disposition = harness.completions.recv().await.unwrap();
    assert!(disposition.waiter_delivered);
    assert!(disposition.explicitly_killed);
    assert!(!disposition.should_surface);
    assert_eq!(
        spawn.await.unwrap().unwrap().outcome,
        ChildOutcome::Cancelled
    );
    harness.actor.abort();
}

/// Cancellation that lands while the runtime is mid-initialization must lose
/// the child, not gain a live one nobody is tracking: the promote
/// acknowledgement comes back `false` and the runner tears its half-built
/// child down.
#[tokio::test]
async fn cancel_at_promote_refuses_the_promotion() {
    let mut harness = harness_with_options(
        RunnerOptions {
            gate_promote: true,
            ..RunnerOptions::default()
        },
        CoordinatorConfig::default(),
    );
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("promote-race", true)).await }
    });

    // The child is built but not yet handed over.
    assert_eq!(
        harness.promote_reached.recv().await.as_deref(),
        Some("promote-race")
    );
    assert!(matches!(
        harness.backend.cancel("promote-race").await,
        CancelOutcome::Cancelled
    ));

    // Now let the promotion race in — it must lose.
    let _ = harness.promote_gate.send(());
    assert_eq!(
        harness.promote_acks.recv().await,
        Some(false),
        "a cancelled child must not be promoted to active"
    );
    assert_eq!(
        spawn.await.unwrap().unwrap().outcome,
        ChildOutcome::Cancelled
    );
    harness.actor.abort();
}

/// A child whose runtime panics is contained: the actor keeps its other
/// children, answers commands, and reports the panic as an ordinary failure.
///
/// What this proves and what it does not: tests are built with unwinding, so
/// this exercises the real guard in `ChildRunFuture::poll` — the actor's
/// bookkeeping survives, every waiter and caller is still served, and the
/// child is recorded. It says nothing about a shipped binary. The release
/// profiles set `panic = "abort"` (root `Cargo.toml`), where the child's panic
/// aborts the process and this path is never reached.
#[tokio::test]
async fn panicking_child_does_not_take_down_the_actor() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));

    let result = harness
        .backend
        .spawn(request("boom-1", true))
        .await
        .expect("the actor must survive its child and still answer");
    assert_eq!(result.outcome, ChildOutcome::Failed);
    assert!(
        result
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("panicked")),
        "detail: {:?}",
        result.detail
    );
    let disposition = harness.completions.recv().await.unwrap();
    assert!(disposition.should_surface);

    // The actor is still a working actor: a later child runs normally.
    let survivor = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("survivor", true)).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("survivor"));
    let _ = harness.finish.send(());
    assert!(survivor.await.unwrap().unwrap().is_success());
    assert_eq!(
        harness.backend.registry_counts().await,
        RegistryCounts {
            pending: 0,
            active: 0,
            completed: 2,
        }
    );
    harness.actor.abort();
}

#[tokio::test]
async fn caller_drop_during_initialization_does_not_drop_owned_run() {
    let mut harness = harness(true, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("owned", false)).await }
    });
    tokio::task::yield_now().await;
    spawn.abort();
    let _ = spawn.await;

    let initializing = harness.backend.query("owned", false, None).await.unwrap();
    assert!(matches!(initializing.status, ChildStatus::Initializing));
    let _ = harness.start.send(());
    tokio::task::yield_now().await;
    let _ = harness.finish.send(());
    let disposition = harness.completions.recv().await.unwrap();
    assert!(
        disposition.should_surface,
        "dropped foreground receiver becomes handle-only"
    );
    let terminal = harness.backend.query("owned", false, None).await.unwrap();
    assert!(terminal.status.is_terminal());
    harness.actor.abort();
}

/// Dropping the spawn await must leave the turn-blocking set immediately,
/// without waiting out the foreground budget — and without stopping the child.
#[tokio::test]
async fn abandoned_foreground_caller_clears_outstanding() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("abandoned", false)).await }
    });
    tokio::task::yield_now().await;
    assert_eq!(
        outstanding(&harness.backend, "prompt").await.live_ids,
        vec!["abandoned".to_owned()],
        "live foreground child blocks the turn"
    );

    spawn.abort();
    let _ = spawn.await;
    assert_eq!(
        outstanding(&harness.backend, "prompt").await,
        OutstandingReply {
            live_ids: Vec::new(),
            background_live: true,
        },
        "caller-gone foreground is handle-only for outstanding work"
    );
    let running = harness
        .backend
        .query("abandoned", false, None)
        .await
        .unwrap();
    assert!(
        running.is_running(),
        "child keeps running after its caller goes away"
    );

    let _ = harness.finish.send(());
    let disposition = harness.completions.recv().await.unwrap();
    assert!(disposition.backgrounded);
    assert!(disposition.should_surface);
    harness.actor.abort();
}

#[tokio::test]
async fn duplicate_child_id_is_rejected_without_replacing_live_child() {
    let harness = harness(false, std::time::Duration::from_secs(60));
    let first = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("duplicate", true)).await }
    });
    tokio::task::yield_now().await;

    // A duplicate id is an *admission* refusal, not a failed child: nothing
    // ran under that id a second time, so there is no outcome to report.
    let duplicate = refused_spawn(&harness.backend, request("duplicate", false)).await;
    assert_eq!(
        duplicate,
        SpawnRefusal::DuplicateChildId {
            child_id: "duplicate".to_owned()
        }
    );
    assert!(
        duplicate.to_string().contains("already exists"),
        "the printed reason must still name the collision, got: {duplicate}"
    );

    let running = harness
        .backend
        .query("duplicate", false, None)
        .await
        .expect("original child remains queryable");
    assert!(running.is_running());
    let _ = harness.finish.send(());
    assert!(first.await.unwrap().unwrap().is_success());
    harness.actor.abort();
}

#[tokio::test]
async fn external_cancel_token_cancels_live_child() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let request = request("external-cancel", false);
    let cancel_token = request.cancel_token.clone();
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request).await }
    });
    assert_eq!(
        harness.started.recv().await.as_deref(),
        Some("external-cancel")
    );

    cancel_token.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), spawn)
        .await
        .expect("external cancellation should finish")
        .unwrap()
        .unwrap();
    assert_eq!(result.outcome, ChildOutcome::Cancelled);
    let disposition = harness.completions.recv().await.unwrap();
    assert!(
        !disposition.explicitly_killed,
        "an external token is not a kill request"
    );
    harness.actor.abort();
}

#[tokio::test]
async fn dropping_coordinator_cancels_live_child() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let cancellation = CancelToken::new();
    let mut request = request("owner-drop", true);
    request.cancel_token = cancellation.clone();
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("owner-drop"));

    harness.actor.abort();
    tokio::time::timeout(std::time::Duration::from_secs(1), cancellation.cancelled())
        .await
        .expect("coordinator drop should cancel child");
    assert!(spawn.await.unwrap().is_err());
}

#[tokio::test(start_paused = true)]
async fn await_to_completion_has_no_foreground_deadline() {
    let mut harness = harness(false, std::time::Duration::from_secs(1));
    let mut request = request("await-completion", false);
    request.await_to_completion = true;
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request).await }
    });
    assert_eq!(
        harness.started.recv().await.as_deref(),
        Some("await-completion")
    );

    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    assert!(!spawn.is_finished());
    let _ = harness.finish.send(());
    let result = spawn.await.unwrap().unwrap();
    assert!(result.is_success());
    assert!(!result.backgrounded);
    harness.actor.abort();
}

#[tokio::test]
async fn outstanding_reply_is_sorted_and_prompt_cancel_reaches_every_child() {
    let mut harness = harness(true, std::time::Duration::from_secs(60));
    let mut spawns = Vec::new();
    for (id, is_background) in [
        ("z-foreground", false),
        ("a-foreground", false),
        ("background", true),
    ] {
        spawns.push(tokio::spawn({
            let backend = harness.backend.clone();
            async move { backend.spawn(request(id, is_background)).await }
        }));
        assert_eq!(
            harness
                .requests
                .recv()
                .await
                .as_ref()
                .map(|request| request.child_id.as_str()),
            Some(id)
        );
    }

    assert_eq!(
        outstanding(&harness.backend, "prompt").await,
        OutstandingReply {
            live_ids: vec!["a-foreground".to_owned(), "z-foreground".to_owned()],
            background_live: true,
        }
    );

    assert!(matches!(
        harness.backend.cancel_parent_prompt("prompt").await,
        CancelOutcome::Cancelled
    ));
    for spawn in spawns {
        assert_eq!(
            spawn.await.unwrap().unwrap().outcome,
            ChildOutcome::Cancelled
        );
    }
    harness.actor.abort();
}

#[tokio::test]
async fn loop_tracking_covers_pending_active_and_nested_reparenting() {
    let mut harness = harness(true, std::time::Duration::from_secs(60));
    let mut outer_request = request("outer", true);
    outer_request.overrides.loop_task_id = Some("loop-task".to_owned());
    let outer_spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(outer_request).await }
    });
    let observed_outer = harness.requests.recv().await.unwrap();
    assert_eq!(observed_outer.parent_session_id, "parent");
    assert!(loop_unit_active(&harness.backend, "loop-task").await);

    let _ = harness.start.send(());
    assert_eq!(harness.started.recv().await.as_deref(), Some("outer"));
    let refs = harness
        .backend
        .spawned_refs_for_prompt("parent", "prompt")
        .await;
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].description, "test child");

    let mut nested_request = request("nested", true);
    nested_request.parent_session_id = "outer".to_owned();
    let nested_spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(nested_request).await }
    });
    let observed_nested = harness.requests.recv().await.unwrap();
    assert_eq!(observed_nested.parent_session_id, "parent");
    assert!(!observed_nested.surface_completion);
    assert_eq!(
        observed_nested.overrides.loop_task_id.as_deref(),
        Some("loop-task")
    );
    assert!(loop_unit_active(&harness.backend, "loop-task").await);

    let _ = harness.start.send(());
    assert_eq!(harness.started.recv().await.as_deref(), Some("nested"));
    let _ = harness.finish.send(());
    assert!(outer_spawn.await.unwrap().unwrap().is_success());
    assert!(nested_spawn.await.unwrap().unwrap().is_success());
    assert!(!loop_unit_active(&harness.backend, "loop-task").await);
    harness.actor.abort();
}

#[tokio::test]
async fn completion_buffer_caps_summary_without_mutating_result() {
    let mut harness = harness_with_config(
        false,
        CoordinatorConfig {
            buffer_completions: true,
            ..CoordinatorConfig::default()
        },
    );
    let mut request = request("buffered", true);
    request.prompt = "aéb".to_owned();
    request.overrides.completion_output_cap = Some(2);
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("buffered"));
    let _ = harness.finish.send(());
    let result = spawn.await.unwrap().unwrap();
    assert_eq!(result.output.as_ref(), "aéb");
    let _ = harness.completions.recv().await;
    let snapshot = harness
        .backend
        .query("buffered", false, None)
        .await
        .unwrap();
    let ChildStatus::Finished { output, .. } = snapshot.status else {
        panic!("expected a finished child");
    };
    assert_eq!(output, "aéb");

    let buffered = drain_completions(&harness.backend, "parent").await;
    assert_eq!(buffered.len(), 1);
    assert_eq!(buffered[0].child_id, "buffered");
    assert_eq!(
        buffered[0].output.as_ref(),
        "a\n[output truncated: 1 of 4 bytes shown]"
    );
    harness.actor.abort();
}

/// Regression: an agent definition that declares itself background, spawned by
/// a BLOCKING call, is background for outstanding-work accounting — while the
/// caller still receives the result inline.
#[tokio::test]
async fn definition_background_counts_as_background_for_outstanding() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let mut blocking_request = request("bg-def", false);
    blocking_request.agent_type = "background-default".to_owned();
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(blocking_request).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("bg-def"));

    assert_eq!(
        outstanding(&harness.backend, "prompt").await,
        OutstandingReply {
            live_ids: Vec::new(),
            background_live: true,
        }
    );

    let _ = harness.finish.send(());
    let result = spawn.await.unwrap().unwrap();
    assert!(result.is_success());
    assert!(!result.backgrounded);
    harness.actor.abort();
}

#[tokio::test]
async fn buffered_completion_output_cap_bounds_buffered_summary() {
    let mut harness = harness_with_config(
        false,
        CoordinatorConfig {
            buffer_completions: true,
            buffered_completion_output_cap: Some(8),
            ..CoordinatorConfig::default()
        },
    );
    let mut request = request("capped", true);
    request.prompt = "x".repeat(64);
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("capped"));
    let _ = harness.finish.send(());
    // The spawn result and the queryable snapshot keep the full output…
    let result = spawn.await.unwrap().unwrap();
    assert_eq!(result.output.len(), 64);
    let _ = harness.completions.recv().await;

    // …only the buffered reminder copy is truncated.
    let buffered = drain_completions(&harness.backend, "parent").await;
    assert_eq!(buffered.len(), 1);
    assert!(
        buffered[0]
            .output
            .contains("[output truncated: 8 of 64 bytes shown]"),
        "buffered output must be capped, got: {}",
        buffered[0].output
    );
    harness.actor.abort();
}

/// A parent that never drains must not be able to grow the buffer without
/// bound; past the cap the oldest summaries are dropped.
#[tokio::test]
async fn buffered_completions_are_bounded_by_dropping_the_oldest() {
    let mut harness = harness_with_config(
        false,
        CoordinatorConfig {
            buffer_completions: true,
            ..CoordinatorConfig::default()
        },
    );
    for index in 0..=MAX_PENDING_COMPLETIONS {
        let id = format!("buffer-{index:04}");
        let spawn = tokio::spawn({
            let backend = harness.backend.clone();
            let request = request(&id, true);
            async move { backend.spawn(request).await }
        });
        assert_eq!(harness.started.recv().await.as_deref(), Some(id.as_str()));
        let _ = harness.finish.send(());
        assert!(spawn.await.unwrap().unwrap().is_success());
        let _ = harness.completions.recv().await;
    }

    let buffered = drain_completions(&harness.backend, "parent").await;
    assert_eq!(buffered.len(), MAX_PENDING_COMPLETIONS);
    let ids: Vec<&str> = buffered
        .iter()
        .map(|summary| summary.child_id.as_str())
        .collect();
    assert!(
        !ids.contains(&"buffer-0000"),
        "the oldest buffered summary must be the one dropped"
    );
    assert_eq!(
        ids.last().copied(),
        Some(format!("buffer-{MAX_PENDING_COMPLETIONS:04}").as_str())
    );
    harness.actor.abort();
}

#[tokio::test]
async fn discard_session_completions_drops_only_that_sessions_buffer() {
    let mut harness = harness_with_config(
        false,
        CoordinatorConfig {
            buffer_completions: true,
            ..CoordinatorConfig::default()
        },
    );
    for (id, parent) in [("child-a", "parent-a"), ("child-b", "parent-b")] {
        let mut request = request(id, true);
        request.parent_session_id = parent.to_owned();
        let spawn = tokio::spawn({
            let backend = harness.backend.clone();
            async move { backend.spawn(request).await }
        });
        assert_eq!(harness.started.recv().await.as_deref(), Some(id));
        let _ = harness.finish.send(());
        assert!(spawn.await.unwrap().unwrap().is_success());
        let _ = harness.completions.recv().await;
    }

    // Unloading parent-a discards its buffered completion...
    harness
        .backend
        .sender()
        .send(CoordinatorCommand::DiscardSessionCompletions {
            parent_session_id: "parent-a".to_owned(),
        })
        .expect("actor command channel open");

    assert!(
        drain_completions(&harness.backend, "parent-a")
            .await
            .is_empty()
    );
    // ...while parent-b's completion stays buffered for its own drain.
    let b = drain_completions(&harness.backend, "parent-b").await;
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].child_id, "child-b");
    harness.actor.abort();
}

#[tokio::test]
async fn completion_drain_is_scoped_to_parent_session() {
    let mut harness = harness_with_config(
        false,
        CoordinatorConfig {
            buffer_completions: true,
            ..CoordinatorConfig::default()
        },
    );
    for (id, parent) in [("child-a", "parent-a"), ("child-b", "parent-b")] {
        let mut request = request(id, true);
        request.parent_session_id = parent.to_owned();
        let spawn = tokio::spawn({
            let backend = harness.backend.clone();
            async move { backend.spawn(request).await }
        });
        assert_eq!(harness.started.recv().await.as_deref(), Some(id));
        let _ = harness.finish.send(());
        assert!(spawn.await.unwrap().unwrap().is_success());
        let _ = harness.completions.recv().await;
    }

    for (parent, expected_id) in [("parent-a", "child-a"), ("parent-b", "child-b")] {
        let completions = drain_completions(&harness.backend, parent).await;
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].child_id, expected_id);
    }
    harness.actor.abort();
}

#[tokio::test]
async fn session_scoped_backend_cannot_query_or_cancel_foreign_child() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("scoped", true)).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("scoped"));

    let foreign = ChannelBackend::for_session(harness.backend.sender(), "foreign-parent");
    assert!(foreign.query("scoped", false, None).await.is_none());
    assert!(foreign.inspect("scoped").await.is_none());
    assert!(matches!(
        foreign.cancel("scoped").await,
        CancelOutcome::NotFound
    ));

    assert!(matches!(
        harness.backend.cancel("scoped").await,
        CancelOutcome::Cancelled
    ));
    assert_eq!(
        spawn.await.unwrap().unwrap().outcome,
        ChildOutcome::Cancelled
    );
    let _ = harness.completions.recv().await;
    harness.actor.abort();
}

#[tokio::test]
async fn list_active_is_scoped_to_the_parent_session() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("listed", true)).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("listed"));

    let (respond_to, response_rx) = oneshot::channel();
    harness
        .backend
        .sender()
        .send(CoordinatorCommand::ListActive(ListActiveCommand {
            parent_session_id: "parent".to_owned(),
            respond_to,
        }))
        .expect("actor command channel open");
    let active = response_rx.await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].child_id, "listed");
    assert_eq!(active[0].description, "test child");

    let (respond_to, response_rx) = oneshot::channel();
    harness
        .backend
        .sender()
        .send(CoordinatorCommand::ListActive(ListActiveCommand {
            parent_session_id: "someone-else".to_owned(),
            respond_to,
        }))
        .expect("actor command channel open");
    assert!(response_rx.await.unwrap().is_empty());

    let running = harness.backend.list_running("parent").await;
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].snapshot.child_id, "listed");
    assert!(running[0].snapshot.is_running());

    let _ = harness.finish.send(());
    assert!(spawn.await.unwrap().unwrap().is_success());
    harness.actor.abort();
}

#[tokio::test]
async fn completed_cache_evicts_oldest_entry_at_cap() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    for index in 0..=MAX_COMPLETED_ENTRIES {
        let id = format!("cache-{index:04}");
        let spawn = tokio::spawn({
            let backend = harness.backend.clone();
            let request = request(&id, true);
            async move { backend.spawn(request).await }
        });
        assert_eq!(harness.started.recv().await.as_deref(), Some(id.as_str()));
        let _ = harness.finish.send(());
        assert!(spawn.await.unwrap().unwrap().is_success());
    }

    assert!(
        harness
            .backend
            .query("cache-0000", false, None)
            .await
            .is_none()
    );
    assert!(
        harness
            .backend
            .query("cache-0001", false, None)
            .await
            .is_some()
    );
    assert!(
        harness
            .backend
            .query(&format!("cache-{MAX_COMPLETED_ENTRIES:04}"), false, None)
            .await
            .is_some()
    );
    harness.actor.abort();
}

// ── ChildPersistence wiring ─────────────────────────────────────────────

/// One observed call into a [`RecordingPersistence`].
#[derive(Debug, Clone, PartialEq)]
enum PersistenceCall {
    Spawn {
        child_id: String,
        parent_session_id: String,
        parent_alias: String,
    },
    Finish {
        child_id: String,
        outcome: ChildOutcome,
        delivered: bool,
    },
}

/// Records every call it receives; optionally errors on every call, to prove
/// the actor treats persistence as an observer, never a gate.
#[derive(Clone, Default)]
struct RecordingPersistence {
    calls: Arc<Mutex<Vec<PersistenceCall>>>,
    error_every_call: bool,
}

impl RecordingPersistence {
    fn calls(&self) -> Vec<PersistenceCall> {
        self.calls.lock().unwrap().clone()
    }
}

impl ChildPersistence for RecordingPersistence {
    fn record_spawn(&mut self, request: &ChildRequest) -> Result<(), PersistenceError> {
        self.calls.lock().unwrap().push(PersistenceCall::Spawn {
            child_id: request.child_id.clone(),
            parent_session_id: request.parent_session_id.clone(),
            parent_alias: request.parent_alias.clone(),
        });
        if self.error_every_call {
            return Err(PersistenceError("record_spawn: simulated failure".into()));
        }
        Ok(())
    }

    fn record_finish(
        &mut self,
        child_id: &str,
        result: &ChildResult,
        delivered: bool,
    ) -> Result<(), PersistenceError> {
        self.calls.lock().unwrap().push(PersistenceCall::Finish {
            child_id: child_id.to_owned(),
            outcome: result.outcome,
            delivered,
        });
        if self.error_every_call {
            return Err(PersistenceError("record_finish: simulated failure".into()));
        }
        Ok(())
    }
}

#[tokio::test]
async fn spawn_records_exactly_one_record_spawn_with_the_right_identity() {
    let persistence = RecordingPersistence::default();
    let mut harness = harness_with_persistence(
        RunnerOptions::default(),
        CoordinatorConfig::default(),
        persistence.clone(),
    );
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("persisted-spawn", false)).await }
    });
    assert_eq!(
        harness.started.recv().await.as_deref(),
        Some("persisted-spawn")
    );

    assert_eq!(
        persistence.calls(),
        vec![PersistenceCall::Spawn {
            child_id: "persisted-spawn".to_owned(),
            parent_session_id: "parent".to_owned(),
            parent_alias: "parent-alias".to_owned(),
        }],
        "record_spawn must fire exactly once, before the child has run at all"
    );

    let _ = harness.finish.send(());
    assert!(spawn.await.unwrap().unwrap().is_success());
    harness.actor.abort();
}

#[tokio::test]
async fn foreground_completion_records_one_finish_delivered_true() {
    let persistence = RecordingPersistence::default();
    let mut harness = harness_with_persistence(
        RunnerOptions::default(),
        CoordinatorConfig::default(),
        persistence.clone(),
    );
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("delivered-inline", false)).await }
    });
    tokio::task::yield_now().await;
    let _ = harness.finish.send(());
    assert!(spawn.await.unwrap().unwrap().is_success());
    let _ = harness.completions.recv().await;

    let finishes: Vec<_> = persistence
        .calls()
        .into_iter()
        .filter(|call| matches!(call, PersistenceCall::Finish { .. }))
        .collect();
    assert_eq!(
        finishes,
        vec![PersistenceCall::Finish {
            child_id: "delivered-inline".to_owned(),
            outcome: ChildOutcome::Completed,
            delivered: true,
        }],
        "record_finish must fire exactly once, delivered=true, when the \
         foreground spawn caller already got the result inline"
    );
    harness.actor.abort();
}

#[tokio::test]
async fn undelivered_completion_records_one_finish_delivered_false() {
    let persistence = RecordingPersistence::default();
    let mut harness = harness_with_persistence(
        RunnerOptions::default(),
        CoordinatorConfig::default(),
        persistence.clone(),
    );
    // `background`: nobody blocks on the spawn reply, and no waiter attaches
    // either, so the completion is nobody's to consume in-process.
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("nobody-consumed", true)).await }
    });
    assert_eq!(
        harness.started.recv().await.as_deref(),
        Some("nobody-consumed")
    );
    let _ = harness.finish.send(());
    let _ = spawn.await;
    let _ = harness.completions.recv().await;

    let finishes: Vec<_> = persistence
        .calls()
        .into_iter()
        .filter(|call| matches!(call, PersistenceCall::Finish { .. }))
        .collect();
    assert_eq!(
        finishes,
        vec![PersistenceCall::Finish {
            child_id: "nobody-consumed".to_owned(),
            outcome: ChildOutcome::Completed,
            delivered: false,
        }],
        "record_finish must fire exactly once, delivered=false, when no \
         foreground caller and no waiter consumed the result in-process"
    );
    harness.actor.abort();
}

#[tokio::test]
async fn cancelled_child_records_one_finish_with_cancelled_outcome() {
    let persistence = RecordingPersistence::default();
    let mut harness = harness_with_persistence(
        RunnerOptions::default(),
        CoordinatorConfig::default(),
        persistence.clone(),
    );
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("to-cancel", false)).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("to-cancel"));

    assert!(matches!(
        harness.backend.cancel("to-cancel").await,
        CancelOutcome::Cancelled
    ));
    assert_eq!(
        spawn.await.unwrap().unwrap().outcome,
        ChildOutcome::Cancelled
    );
    let _ = harness.completions.recv().await;

    let finishes: Vec<_> = persistence
        .calls()
        .into_iter()
        .filter(|call| matches!(call, PersistenceCall::Finish { .. }))
        .collect();
    assert_eq!(
        finishes,
        vec![PersistenceCall::Finish {
            child_id: "to-cancel".to_owned(),
            outcome: ChildOutcome::Cancelled,
            delivered: true,
        }],
        "a cancelled child must still reach record_finish exactly once, \
         carrying the Cancelled outcome"
    );
    harness.actor.abort();
}

#[tokio::test]
async fn panicking_child_records_one_finish_with_failed_outcome() {
    let persistence = RecordingPersistence::default();
    let mut harness = harness_with_persistence(
        RunnerOptions::default(),
        CoordinatorConfig::default(),
        persistence.clone(),
    );

    let result = harness
        .backend
        .spawn(request("boom-2", false))
        .await
        .expect("the actor must survive its child and still answer");
    assert_eq!(result.outcome, ChildOutcome::Failed);
    let _ = harness.completions.recv().await;

    let finishes: Vec<_> = persistence
        .calls()
        .into_iter()
        .filter(|call| matches!(call, PersistenceCall::Finish { .. }))
        .collect();
    assert_eq!(
        finishes,
        vec![PersistenceCall::Finish {
            child_id: "boom-2".to_owned(),
            // Donor-faithful ruling from `ChildPersistence`'s port: a
            // panicking child is finished through the ordinary path as an
            // outcome the store already knows how to accept, not a sixth
            // vocabulary entry.
            outcome: ChildOutcome::Failed,
            delivered: true,
        }],
        "a panicking child must still reach record_finish exactly once"
    );
    harness.actor.abort();
}

#[tokio::test]
async fn erroring_persistence_does_not_gate_delivery_or_take_down_the_actor() {
    let persistence = RecordingPersistence {
        error_every_call: true,
        ..RecordingPersistence::default()
    };
    let mut harness = harness_with_persistence(
        RunnerOptions::default(),
        CoordinatorConfig::default(),
        persistence.clone(),
    );

    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("errors-ok", false)).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("errors-ok"));
    let _ = harness.finish.send(());
    let result = spawn
        .await
        .unwrap()
        .expect("an erroring persistence must not stop delivery");
    assert!(result.is_success());
    let _ = harness.completions.recv().await;

    // Both the spawn-time write and the finish-time write were attempted
    // (and both errored) — persistence is an observer, not a gate.
    assert_eq!(
        persistence.calls(),
        vec![
            PersistenceCall::Spawn {
                child_id: "errors-ok".to_owned(),
                parent_session_id: "parent".to_owned(),
                parent_alias: "parent-alias".to_owned(),
            },
            PersistenceCall::Finish {
                child_id: "errors-ok".to_owned(),
                outcome: ChildOutcome::Completed,
                delivered: true,
            },
        ]
    );

    // The actor is still alive and answers normally after two persistence
    // failures.
    let survivor = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("survivor-after-errors", true)).await }
    });
    assert_eq!(
        harness.started.recv().await.as_deref(),
        Some("survivor-after-errors")
    );
    let _ = harness.finish.send(());
    assert!(survivor.await.unwrap().unwrap().is_success());
    harness.actor.abort();
}

// ── Drop: abandoned pending/active children ─────────────────────────────

/// One pending (never promoted) and one active (promoted, still running)
/// child both live when the coordinator is dropped: each must get exactly
/// one `record_finish`, `Lost`, `delivered = false` — never zero (the
/// unbounded-Running-row bug this test pins) and never more than one.
#[tokio::test]
async fn drop_with_pending_and_active_children_records_one_lost_finish_each() {
    let persistence = RecordingPersistence::default();
    let mut harness = harness_with_persistence(
        RunnerOptions {
            wait_before_start: true,
            ..RunnerOptions::default()
        },
        CoordinatorConfig::default(),
        persistence.clone(),
    );

    // First child: released past its `start` gate, so it promotes to active
    // before the second child even exists — a later `broadcast` subscriber
    // does not see an already-sent message, which is what keeps the second
    // child from also promoting.
    let active_spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("drop-active", false)).await }
    });
    tokio::task::yield_now().await;
    let _ = harness.start.send(());
    assert_eq!(harness.started.recv().await.as_deref(), Some("drop-active"));

    // Second child: subscribes fresh, after that `start` message already
    // went out, so it blocks — pending, never promoted.
    let pending_spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("drop-pending", false)).await }
    });
    tokio::task::yield_now().await;

    harness.actor.abort();
    // Awaiting the aborted handle is what makes the drop deterministic: the
    // task (and the `Coordinator` it owns) is fully torn down by the time
    // this resolves, not merely "requested to stop".
    let _ = harness.actor.await;

    let mut finishes: Vec<_> = persistence
        .calls()
        .into_iter()
        .filter(|call| matches!(call, PersistenceCall::Finish { .. }))
        .collect();
    finishes.sort_by(|a, b| match (a, b) {
        (
            PersistenceCall::Finish { child_id: x, .. },
            PersistenceCall::Finish { child_id: y, .. },
        ) => x.cmp(y),
        _ => unreachable!(),
    });
    assert_eq!(
        finishes,
        vec![
            PersistenceCall::Finish {
                child_id: "drop-active".to_owned(),
                outcome: ChildOutcome::Lost,
                delivered: false,
            },
            PersistenceCall::Finish {
                child_id: "drop-pending".to_owned(),
                outcome: ChildOutcome::Lost,
                delivered: false,
            },
        ],
        "every child still pending or active at Drop must get exactly one \
         Lost, undelivered record_finish"
    );

    let _ = active_spawn.await;
    let _ = pending_spawn.await;
}

/// A child that already finished normally before Drop must not get a second,
/// Drop-time write — `finish_child` already made the one write it gets.
#[tokio::test]
async fn drop_after_normal_completion_does_not_double_write() {
    let persistence = RecordingPersistence::default();
    let mut harness = harness_with_persistence(
        RunnerOptions::default(),
        CoordinatorConfig::default(),
        persistence.clone(),
    );
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("drop-after-finish", false)).await }
    });
    assert_eq!(
        harness.started.recv().await.as_deref(),
        Some("drop-after-finish")
    );
    let _ = harness.finish.send(());
    assert!(spawn.await.unwrap().unwrap().is_success());
    let _ = harness.completions.recv().await;

    harness.actor.abort();
    let _ = harness.actor.await;

    let finishes: Vec<_> = persistence
        .calls()
        .into_iter()
        .filter(|call| matches!(call, PersistenceCall::Finish { .. }))
        .collect();
    assert_eq!(
        finishes,
        vec![PersistenceCall::Finish {
            child_id: "drop-after-finish".to_owned(),
            outcome: ChildOutcome::Completed,
            delivered: true,
        }],
        "a child already moved into `completed` before Drop must not be \
         written again — no Lost row for work that already got a real ending"
    );
}

/// A persistence backend that errors on every call must not turn Drop's
/// abandoned-child write into a panic.
#[tokio::test]
async fn erroring_persistence_during_drop_does_not_panic() {
    let persistence = RecordingPersistence {
        error_every_call: true,
        ..RecordingPersistence::default()
    };
    let harness = harness_with_persistence(
        RunnerOptions {
            wait_before_start: true,
            ..RunnerOptions::default()
        },
        CoordinatorConfig::default(),
        persistence.clone(),
    );
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("drop-error", false)).await }
    });
    tokio::task::yield_now().await; // subscribed to `start`, blocked — pending

    harness.actor.abort();
    // If `record_finish` inside Drop unwound instead of logging, that panic
    // would surface here as `JoinError::is_panic()`, not `is_cancelled()`.
    let joined = harness.actor.await;
    assert!(
        joined.is_err_and(|error| error.is_cancelled()),
        "Drop's persistence write must log an error, never panic"
    );

    assert!(
        persistence
            .calls()
            .iter()
            .any(|call| matches!(call, PersistenceCall::Finish { child_id, .. } if child_id == "drop-error")),
        "record_finish must still have been attempted for the abandoned child"
    );

    let _ = spawn.await;
}

// ── Admission gates ──────────────────────────────────────────────────────

/// Await a spawn that must be refused at admission.
///
/// Bounded on purpose. A refusal is supposed to come straight back down the
/// caller's reply channel, so any wait at all means the request was admitted
/// instead — and an admitted background child answers only at its real
/// ending, which is to say never, for a test that has not released the
/// runner. Without the bound a regression in either gate hangs the test
/// binary rather than failing it.
/// Drive a spawn that must be refused, and hand back the structured reason.
///
/// The timeout is load-bearing: a refusal is answered synchronously with the
/// decision, so anything that blocks here means the request was admitted after
/// all — and without the bound that regression would wedge the test binary
/// instead of reddening it.
async fn refused_spawn(backend: &ChannelBackend, request: ChildRequest) -> SpawnRefusal {
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), backend.spawn(request))
        .await
        .expect("a refused spawn must answer immediately; blocking means the request was admitted")
        .expect_err("a refusal must not arrive as a child result — no child ran");
    match error {
        CoordinatorError::Refused(refusal) => refusal,
        other => panic!("expected a structured refusal, got: {other:?}"),
    }
}

/// The ordering guarantee that gives the admission channel its whole reason to
/// exist, pinned so no refactor can quietly lose it: on the *accepted* path the
/// admission answer arrives while the child is still running, strictly before
/// anything comes down the outcome channel.
///
/// Written against observable channel state rather than against how
/// `handle_spawn` happens to be laid out today, so moving the send around
/// inside the actor keeps this test meaningful — only actually deferring
/// admission until the child settles can redden it.
#[tokio::test]
async fn admission_answers_while_the_child_is_still_running() {
    // `wait_before_start: true` parks the admitted child in `pending`, so
    // "still running" is a state the test controls rather than races.
    let harness = harness(true, std::time::Duration::from_secs(60));
    let (admission_tx, admission_rx) = oneshot::channel();
    let (result_tx, mut result_rx) = oneshot::channel();
    harness
        .backend
        .sender()
        .send(CoordinatorCommand::Spawn(SpawnCommand {
            request: Box::new(request("ordered", false)),
            admission_tx,
            result_tx,
        }))
        .expect("the actor owns the receiver");

    // Bounded: an admission that never arrives is a regression, and an
    // unbounded await here would wedge the binary instead of reporting it.
    let admission = tokio::time::timeout(std::time::Duration::from_secs(5), admission_rx)
        .await
        .expect("admission must not wait for the child to finish")
        .expect("the actor answers admission on every path");
    assert_eq!(admission, SpawnAdmission::Admitted);
    assert!(
        result_rx.try_recv().is_err(),
        "admission must arrive BEFORE any outcome — the child has not even started"
    );

    let _ = harness.start.send(());
    let _ = harness.finish.send(());
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), result_rx)
        .await
        .expect("the admitted child must still deliver its outcome")
        .expect("an admitted child answers its outcome channel");
    assert!(
        result.is_success(),
        "the outcome channel still carries the real ending, got: {result:?}"
    );
    harness.actor.abort();
}

/// A spawn still sitting in the mailbox when the actor is torn down is
/// *refused*, not silently dropped.
///
/// An unanswered admission channel is the lost-refusal bug inverted: the caller
/// waits out its own timeout for an answer that can never come. The coordinator
/// is never polled here, so the command is provably still queued when `Drop`
/// runs.
#[tokio::test]
async fn spawns_still_queued_at_teardown_are_refused_rather_than_left_unanswered() {
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (start, _start_rx) = tokio::sync::broadcast::channel(4);
    let (finish, _finish_rx) = tokio::sync::broadcast::channel(4);
    let (promote_gate, _gate_rx) = tokio::sync::broadcast::channel(4);
    let (promote_reached, _reached_rx) = mpsc::unbounded_channel();
    let (promote_acks, _acks_rx) = mpsc::unbounded_channel();
    let (completions, _completions_rx) = mpsc::unbounded_channel();
    let (requests, _requests_rx) = mpsc::unbounded_channel();
    let (started, _started_rx) = mpsc::unbounded_channel();
    let coordinator = Coordinator::new(
        command_rx,
        TestRunner {
            options: RunnerOptions::default(),
            start,
            finish,
            promote_gate,
            promote_reached,
            promote_acks,
            completions,
            requests,
            started,
        },
        CoordinatorConfig::default(),
    );

    let (admission_tx, admission_rx) = oneshot::channel();
    let (result_tx, _result_rx) = oneshot::channel();
    command_tx
        .send(CoordinatorCommand::Spawn(SpawnCommand {
            request: Box::new(request("never-read", true)),
            admission_tx,
            result_tx,
        }))
        .expect("the coordinator owns the receiver");

    // `coordinator.run()` was never awaited, so this command has never been
    // dispatched — it dies in the mailbox with the actor.
    drop(coordinator);

    let admission = tokio::time::timeout(std::time::Duration::from_secs(5), admission_rx)
        .await
        .expect("a torn-down actor must resolve queued admissions, not leave them hanging")
        .expect("teardown answers rather than drops the sender");
    assert_eq!(
        admission,
        SpawnAdmission::Refused(SpawnRefusal::CoordinatorShuttingDown)
    );
}

#[test]
fn spawn_depth_predicate_admits_the_limit_and_disables_at_zero() {
    // The child at exactly the limit is legal; only the next generation is not.
    assert!(!crate::state::exceeds_spawn_depth(2, 3));
    assert!(!crate::state::exceeds_spawn_depth(3, 3));
    assert!(crate::state::exceeds_spawn_depth(4, 3));
    // `0` disables, the same escape delegation's own depth resolution has.
    assert!(!crate::state::exceeds_spawn_depth(9_999, 0));
    // A zero-depth child is admitted even by the tightest live limit.
    assert!(!crate::state::exceeds_spawn_depth(0, 1));
}

#[test]
fn child_capacity_predicate_admits_the_request_that_fills_the_cap() {
    // `in_flight` is the count before admitting, so filling the last slot is
    // still an admission and only the one after it is refused.
    assert!(!crate::state::at_child_capacity(1, 2));
    assert!(crate::state::at_child_capacity(2, 2));
    assert!(crate::state::at_child_capacity(3, 2));
    // `0` disables the backstop.
    assert!(!crate::state::at_child_capacity(9_999, 0));
}

/// The sentence a model reads must say how many are running *now* as well as
/// what the limit is. With the daemon default at 6 rather than delegate's 128
/// backstop, this refusal stops being a symptom of a broken deployment and
/// becomes an ordinary "the queue is full" — and a model that cannot tell
/// those apart will either give up on a working tool or retry a hopeless one.
///
/// The two numbers are deliberately different here. At the live gate they are
/// equal by construction, which is exactly what would hide a `Display` that
/// prints the limit twice and calls one of them the running count.
#[test]
fn child_capacity_refusal_reason_names_the_in_flight_count_and_the_limit() {
    let reason = SpawnRefusal::ChildCapacityReached {
        in_flight: 5,
        max: 6,
    }
    .to_string();
    assert!(
        reason.contains("5 running"),
        "the reason must state how many children are in flight, got: {reason}"
    );
    assert!(
        reason.contains("limit 6"),
        "the reason must state the configured limit, got: {reason}"
    );
    assert!(
        reason.contains("Nothing was started"),
        "a refusal must still say that nothing ran, got: {reason}"
    );
}

/// The compiled-in default is an operating limit, not the runaway backstop it
/// was copied from. Hosts that build the actor without reading a config file
/// land here, so this number has to agree with
/// `zeroclaw_config::subagents::DEFAULT_MAX_CONCURRENT_CHILDREN` — the two
/// literals are paired by hand because this crate is a leaf and does not
/// depend on the config crate.
#[test]
fn default_child_capacity_is_six_not_the_delegate_backstop() {
    assert_eq!(
        CoordinatorConfig::default().max_concurrent_children,
        6,
        "128 is `DelegateTool::MAX_CONCURRENT_BACKGROUND_DELEGATIONS`, a backstop meaning \
         'something is broken' — it is not an operating limit and must not be this default"
    );
}

#[tokio::test]
async fn declared_spawn_depth_is_admitted_at_the_limit_and_refused_one_deeper() {
    let mut harness = harness_with_config(
        false,
        CoordinatorConfig {
            max_spawn_depth: 2,
            ..CoordinatorConfig::default()
        },
    );

    // Exactly at the limit: admitted, and the runner really receives it.
    let mut at_limit = request("at-limit", true);
    at_limit.overrides.spawn_depth = Some(2);
    let admitted = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(at_limit).await }
    });
    let observed = harness.requests.recv().await.unwrap();
    assert_eq!(observed.child_id, "at-limit");
    assert_eq!(observed.overrides.spawn_depth, Some(2));

    // One deeper: refused structurally — a result carrying the reason, no
    // panic, no success, and nothing handed to the runner.
    let mut too_deep = request("too-deep", true);
    too_deep.overrides.spawn_depth = Some(3);
    let refused = refused_spawn(&harness.backend, too_deep).await;
    assert_eq!(
        refused,
        SpawnRefusal::SpawnDepthExceeded { depth: 3, max: 2 },
        "the refusal must carry the depth and the limit as data, not prose"
    );
    assert!(
        refused
            .to_string()
            .contains("spawn depth limit reached (3/2)"),
        "the printed reason must name the depth and the limit, got: {refused}"
    );
    assert!(
        harness.requests.try_recv().is_err(),
        "a refused spawn must never reach the runner"
    );

    let _ = harness.finish.send(());
    assert!(admitted.await.unwrap().unwrap().is_success());
    harness.actor.abort();
}

#[tokio::test]
async fn spawn_depth_descends_from_a_live_spawner_and_a_declaration_cannot_undercut_it() {
    let mut harness = harness_with_config(
        false,
        CoordinatorConfig {
            max_spawn_depth: 1,
            ..CoordinatorConfig::default()
        },
    );

    // Generation 0: no live spawner, so depth resolves to 0.
    let root = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("root", true)).await }
    });
    let observed_root = harness.requests.recv().await.unwrap();
    assert_eq!(observed_root.overrides.spawn_depth, Some(0));
    assert_eq!(harness.started.recv().await.as_deref(), Some("root"));

    // Generation 1: spawned by a live child, declaring nothing. Depth is
    // derived as parent + 1 and lands exactly on the limit, so it is admitted.
    let mut child = request("child", true);
    child.parent_session_id = "root".to_owned();
    let admitted = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(child).await }
    });
    let observed_child = harness.requests.recv().await.unwrap();
    assert_eq!(
        observed_child.overrides.spawn_depth,
        Some(1),
        "a child of a live child inherits parent depth + 1"
    );
    // Re-parenting has already flattened this away, which is exactly why the
    // depth must be carried on the record instead of recomputed from ancestry.
    assert_eq!(observed_child.parent_session_id, "parent");
    assert_eq!(harness.started.recv().await.as_deref(), Some("child"));

    // Generation 2, declaring depth 0 to try to escape: the declaration may
    // raise the derived floor, never lower it, so this is still generation 2.
    let mut grandchild = request("grandchild", true);
    grandchild.parent_session_id = "child".to_owned();
    grandchild.overrides.spawn_depth = Some(0);
    let refused = refused_spawn(&harness.backend, grandchild).await;
    assert_eq!(
        refused,
        SpawnRefusal::SpawnDepthExceeded { depth: 2, max: 1 },
        "declaring depth 0 must not buy a deeper generation admission, and the refusal must \
         report the derived depth, not the declared one"
    );
    assert!(
        harness.requests.try_recv().is_err(),
        "a refused spawn must never reach the runner"
    );

    let _ = harness.finish.send(());
    assert!(root.await.unwrap().unwrap().is_success());
    assert!(admitted.await.unwrap().unwrap().is_success());
    harness.actor.abort();
}

#[tokio::test]
async fn concurrency_backstop_fills_the_cap_then_refuses_structurally() {
    // `wait_before_start` keeps admitted children parked in `pending`, which
    // is half of the in-flight population the gate counts.
    let mut harness = harness_with_config(
        true,
        CoordinatorConfig {
            max_concurrent_children: 2,
            ..CoordinatorConfig::default()
        },
    );

    let first = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("first", true)).await }
    });
    assert_eq!(harness.requests.recv().await.unwrap().child_id, "first");

    // The request that brings the registry to exactly the cap is admitted.
    let second = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("second", true)).await }
    });
    assert_eq!(harness.requests.recv().await.unwrap().child_id, "second");

    // The one past it is refused, with a reason, and never runs.
    let refused = refused_spawn(&harness.backend, request("third", true)).await;
    assert_eq!(
        refused,
        SpawnRefusal::ChildCapacityReached {
            in_flight: 2,
            max: 2
        },
        "a spawn past the concurrency cap must be refused, carrying both the cap and \
         how many children were actually in flight when it was refused"
    );
    assert!(
        refused
            .to_string()
            .contains("too many children in flight (2 running, limit 2)"),
        "the printed reason must name the in-flight count and the limit, got: {refused}"
    );
    assert!(
        harness.requests.try_recv().is_err(),
        "a refused spawn must never reach the runner"
    );
    assert_eq!(
        harness.backend.registry_counts().await,
        RegistryCounts {
            pending: 2,
            active: 0,
            completed: 0,
        },
        "a refusal must not leave a record behind"
    );

    // Draining the in-flight set makes room again: the backstop throttles, it
    // does not latch.
    let _ = harness.start.send(());
    let _ = harness.finish.send(());
    assert!(first.await.unwrap().unwrap().is_success());
    assert!(second.await.unwrap().unwrap().is_success());
    let readmitted = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("fourth", true)).await }
    });
    assert_eq!(harness.requests.recv().await.unwrap().child_id, "fourth");
    let _ = harness.start.send(());
    let _ = harness.finish.send(());
    assert!(readmitted.await.unwrap().unwrap().is_success());
    harness.actor.abort();
}

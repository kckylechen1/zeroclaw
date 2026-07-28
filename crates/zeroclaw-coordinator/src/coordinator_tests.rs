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

use super::*;
use crate::backend::ChannelBackend;
use crate::cancel::CancelToken;
use crate::outcome::ChildOutcome;
use crate::persistence::PersistenceError;
use crate::state::{
    ChildProgress, MAX_COMPLETED_ENTRIES, MAX_PENDING_COMPLETIONS, SendBoxFuture, StartedChild,
};
use crate::types::{
    CancelOutcome, ChildCompletionSummary, ChildStatus, CompletionsCommand, ListActiveCommand,
    LoopUnitActiveCommand, OutstandingCommand, OutstandingReply, RegistryCounts,
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

    let duplicate = harness
        .backend
        .spawn(request("duplicate", false))
        .await
        .expect("duplicate rejection is a lifecycle result");
    assert_eq!(duplicate.outcome, ChildOutcome::Failed);
    assert!(
        duplicate
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("already exists"))
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

    assert!(drain_completions(&harness.backend, "parent-a").await.is_empty());
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

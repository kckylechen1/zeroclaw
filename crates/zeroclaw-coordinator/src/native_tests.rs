// Tests use bare `tokio::spawn` to drive coordinator actors — the fork's
// `disallowed_methods`/`disallowed_macros` lints target production code,
// not test harnesses.
#![allow(clippy::disallowed_methods, clippy::disallowed_macros)]

use super::*;
use crate::backend::ChannelBackend;
use crate::cancel::CancelToken;
use crate::coordinator::Coordinator;
use crate::driver::{
    AgentDriver, AgentRunHandle, AgentRunRequest, AgentRunSnapshot, AgentRunStatus, DriverError,
    HarnessCapabilities, HarnessKind, ResumeRequest,
};
use crate::outcome::ChildOutcome;
use crate::registry::DriverRegistry;
use crate::state::{
    ChildCompletion, ChildControl, ChildProgress, ChildRunOutput, ChildRunRequest, ChildRunner,
    CoordinatorConfig, SendBoxFuture, StartedChild,
};
use crate::types::{ChildRequest, ChildResult, ChildStatus, DescribeOutcome, ValidateTypeOutcome};
use tokio::sync::mpsc;

#[derive(Clone)]
struct TestControl {
    cancellation: CancelToken,
}

impl ChildControl for TestControl {
    type ProgressFuture = std::future::Ready<ChildProgress>;

    fn progress(&self) -> Self::ProgressFuture {
        std::future::ready(ChildProgress::default())
    }

    fn cancel(&self) {
        self.cancellation.cancel();
    }
}

struct TestRunner {
    wait_before_start: bool,
    start: tokio::sync::broadcast::Sender<()>,
    finish: tokio::sync::broadcast::Sender<()>,
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
        let wait_before_start = self.wait_before_start;
        let mut start = self.start.subscribe();
        let mut finish = self.finish.subscribe();
        let requests = self.requests.clone();
        let started = self.started.clone();
        Box::pin(async move {
            let ChildRunRequest {
                request,
                cancellation,
                reporter,
            } = run;
            let _ = requests.send(request.clone());
            if wait_before_start {
                tokio::select! {
                    () = cancellation.cancelled() => {
                        return cancelled_output(&request);
                    }
                    _ = start.recv() => {}
                }
            }
            let promoted = reporter
                .started(StartedChild {
                    child_session_id: request.child_id.clone(),
                    persona: None,
                    resumed_from: request.resume_from.clone(),
                    child_cwd: request.cwd.clone().unwrap_or_default(),
                    worktree_path: None,
                    effective_model_id: "test-model".to_owned(),
                    definition_background: false,
                    control: TestControl {
                        cancellation: cancellation.clone(),
                    },
                })
                .await;
            if !promoted {
                return cancelled_output(&request);
            }
            let _ = started.send(request.child_id.clone());
            let result = tokio::select! {
                () = cancellation.cancelled() => ChildResult {
                    outcome: ChildOutcome::Cancelled,
                    detail: Some("cancelled".to_owned()),
                    child_id: request.child_id.clone(),
                    child_session_id: request.child_id.clone(),
                    ..Default::default()
                },
                _ = finish.recv() => ChildResult {
                    outcome: ChildOutcome::Completed,
                    output: request.prompt.clone().into(),
                    child_id: request.child_id.clone(),
                    child_session_id: request.child_id.clone(),
                    tool_calls: 3,
                    turns: 2,
                    tokens_used: FIXTURE_TOKENS_USED,
                    output_tokens_used: FIXTURE_OUTPUT_TOKENS_USED,
                    total_tokens_used: FIXTURE_TOTAL_TOKENS_USED,
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

    fn on_completed(&self, _completion: ChildCompletion<Self::CompletionData>) {}
}

fn cancelled_output(request: &ChildRequest) -> ChildRunOutput<()> {
    ChildRunOutput {
        result: ChildResult {
            outcome: ChildOutcome::Cancelled,
            detail: Some("cancelled".to_owned()),
            child_id: request.child_id.clone(),
            child_session_id: request.child_id.clone(),
            ..Default::default()
        },
        completion_data: (),
        snapshot_ref: None,
    }
}

struct Harness {
    backend: ChannelBackend,
    start: tokio::sync::broadcast::Sender<()>,
    finish: tokio::sync::broadcast::Sender<()>,
    requests: mpsc::UnboundedReceiver<ChildRequest>,
    started: mpsc::UnboundedReceiver<String>,
    actor: tokio::task::JoinHandle<()>,
}

fn harness(wait_before_start: bool) -> Harness {
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (start, _) = tokio::sync::broadcast::channel(4);
    let (finish, _) = tokio::sync::broadcast::channel(4);
    let (request_tx, requests) = mpsc::unbounded_channel();
    let (started_tx, started) = mpsc::unbounded_channel();
    let actor = tokio::spawn(
        Coordinator::new(
            command_rx,
            TestRunner {
                wait_before_start,
                start: start.clone(),
                finish: finish.clone(),
                requests: request_tx,
                started: started_tx,
            },
            CoordinatorConfig::default(),
        )
        .run(),
    );
    Harness {
        backend: ChannelBackend::new(command_tx),
        start,
        finish,
        requests,
        started,
        actor,
    }
}

/// Non-zero usage the test runner stamps on a completed `ChildResult`.
/// Inspect after finish must report these exact numbers — not zeros.
const FIXTURE_TOKENS_USED: u64 = 120;
const FIXTURE_OUTPUT_TOKENS_USED: u64 = 45;
const FIXTURE_TOTAL_TOKENS_USED: u64 = 165;

fn run_request(id: &str) -> AgentRunRequest {
    AgentRunRequest {
        run_id: id.to_owned(),
        prompt: "do the work".to_owned(),
        agent: "explore".to_owned(),
        parent_alias: "parent-alias".to_owned(),
        cwd: Some(std::path::PathBuf::from("/tmp/native-run")),
        resume_from: Some("prior-session".to_owned()),
    }
}

async fn inspect_until_finished(
    driver: &dyn AgentDriver,
    handle: &AgentRunHandle,
) -> AgentRunSnapshot {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let snap = driver.inspect(handle).await.expect("run must remain known");
        if matches!(snap.status, AgentRunStatus::Finished(_)) {
            return snap;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "run did not reach Finished within 2s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

#[test]
fn native_card_declares_cancellable_not_resumable() {
    let card = NativeAgentDriver::card();
    assert_eq!(card.id.as_str(), NATIVE_HARNESS_ID);
    assert_eq!(card.kind, HarnessKind::Native);
    assert_eq!(
        card.capabilities,
        HarnessCapabilities {
            streaming_tools: false,
            resumable: false,
            cancellable: true,
        }
    );
}

#[test]
fn child_request_translation_copies_heterogeneous_fields_and_detaches() {
    let request = run_request("run-42");
    let child = child_request_from(request, Some("parent-session"));
    assert_eq!(child.child_id, "run-42");
    assert_eq!(child.prompt, "do the work");
    assert_eq!(child.agent_type, "explore");
    assert_eq!(child.cwd.as_deref(), Some("/tmp/native-run"));
    assert_eq!(child.resume_from.as_deref(), Some("prior-session"));
    assert_eq!(child.parent_session_id, "parent-session");
    assert_eq!(
        child.parent_alias, "parent-alias",
        "parent_alias is the owning agent of the parent session, not a blank default"
    );
    assert!(
        child.run_in_background,
        "spawn returns a handle; the child must not take the foreground budget"
    );
    assert!(!child.await_to_completion);
    assert!(child.surface_completion);
    assert!(!child.fork_context);
}

#[test]
fn child_request_translation_preserves_empty_parent_alias() {
    let mut request = run_request("run-empty-parent");
    request.parent_alias.clear();
    let child = child_request_from(request, Some("parent-session"));
    assert!(
        child.parent_alias.is_empty(),
        "empty parent_alias must pass through; the adapter must not invent an owner"
    );
}

#[test]
fn agent_run_status_maps_each_child_status_without_collapsing_outcomes() {
    assert_eq!(
        agent_run_status(&ChildStatus::Initializing),
        AgentRunStatus::Pending
    );
    assert_eq!(
        agent_run_status(&ChildStatus::Running {
            turn_count: 1,
            tool_call_count: 2,
            tokens_used: 3,
            context_window_tokens: 4,
            context_usage_pct: 5,
            tools_used: Vec::new(),
            error_count: 0,
        }),
        AgentRunStatus::Running
    );
    for outcome in [
        ChildOutcome::Completed,
        ChildOutcome::Failed,
        ChildOutcome::Cancelled,
        ChildOutcome::TimedOut,
        ChildOutcome::Lost,
    ] {
        assert_eq!(
            agent_run_status(&ChildStatus::Finished {
                outcome,
                output: String::new(),
                detail: None,
                tool_calls: 0,
                turns: 0,
                tokens_used: 0,
                output_tokens_used: 0,
                total_tokens_used: 0,
                worktree_path: None,
            }),
            AgentRunStatus::Finished(outcome),
            "Finished must carry {outcome:?} unchanged"
        );
    }
}

#[tokio::test]
async fn registry_dispatches_native_spawn_inspect_cancel_through_trait_object() {
    let mut harness = harness(true);
    let registry = DriverRegistry::with_native(harness.backend.clone());
    let driver: &dyn AgentDriver = registry
        .get(NATIVE_HARNESS_ID)
        .expect("with_native must register the native driver");
    assert_eq!(driver.id(), NATIVE_HARNESS_ID);
    assert_eq!(driver.kind(), HarnessKind::Native);

    let handle = driver
        .spawn(run_request("life-1"))
        .await
        .expect("detached spawn is admitted immediately");
    assert_eq!(handle.run_id, "life-1");

    let translated = harness.requests.recv().await.expect("runner saw the spawn");
    assert_eq!(translated.child_id, "life-1");
    assert_eq!(translated.prompt, "do the work");
    assert_eq!(translated.agent_type, "explore");
    assert_eq!(translated.parent_alias, "parent-alias");
    assert!(translated.run_in_background);

    let pending = driver.inspect(&handle).await.unwrap();
    assert_eq!(pending.status, AgentRunStatus::Pending);
    assert!(pending.result.is_none());

    let _ = harness.start.send(());
    assert_eq!(harness.started.recv().await.as_deref(), Some("life-1"));

    let running = driver.inspect(&handle).await.unwrap();
    assert_eq!(running.status, AgentRunStatus::Running);
    assert!(running.result.is_none());

    driver.cancel(&handle).await.unwrap();
    let finished = inspect_until_finished(driver, &handle).await;
    assert_eq!(
        finished.status,
        AgentRunStatus::Finished(ChildOutcome::Cancelled)
    );
    let result = finished.result.expect("Finished carries a ChildResult");
    assert_eq!(result.outcome, ChildOutcome::Cancelled);
    assert_eq!(result.child_id, "life-1");

    harness.actor.abort();
}

#[tokio::test]
async fn native_driver_inspect_after_completion_preserves_child_outcome() {
    let mut harness = harness(false);
    let driver: Box<dyn AgentDriver> = Box::new(NativeAgentDriver::new(harness.backend.clone()));

    let handle = driver.spawn(run_request("done-1")).await.unwrap();
    assert_eq!(harness.started.recv().await.as_deref(), Some("done-1"));
    let _ = harness.finish.send(());

    let finished = inspect_until_finished(driver.as_ref(), &handle).await;
    assert_eq!(
        finished.status,
        AgentRunStatus::Finished(ChildOutcome::Completed)
    );
    let result = finished.result.expect("Completed run carries a result");
    assert_eq!(result.outcome, ChildOutcome::Completed);
    assert_eq!(&*result.output, "do the work");

    harness.actor.abort();
}

#[tokio::test]
async fn native_driver_inspect_after_completion_preserves_child_result_usage() {
    let mut harness = harness(false);
    let driver: Box<dyn AgentDriver> = Box::new(NativeAgentDriver::new(harness.backend.clone()));

    let handle = driver.spawn(run_request("usage-1")).await.unwrap();
    assert_eq!(harness.started.recv().await.as_deref(), Some("usage-1"));
    let _ = harness.finish.send(());

    let finished = inspect_until_finished(driver.as_ref(), &handle).await;
    assert_eq!(
        finished.status,
        AgentRunStatus::Finished(ChildOutcome::Completed)
    );
    let result = finished.result.expect("Completed run carries a result");
    assert_eq!(result.tokens_used, FIXTURE_TOKENS_USED);
    assert_eq!(result.output_tokens_used, FIXTURE_OUTPUT_TOKENS_USED);
    assert_eq!(result.total_tokens_used, FIXTURE_TOTAL_TOKENS_USED);
    assert_ne!(
        result.tokens_used, 0,
        "fixture must be non-zero so a zeroed snapshot cannot pass"
    );
    assert_ne!(result.output_tokens_used, 0);
    assert_ne!(result.total_tokens_used, 0);

    harness.actor.abort();
}

#[tokio::test]
async fn native_driver_inspect_and_cancel_unknown_run_are_not_found() {
    let harness = harness(false);
    let driver = NativeAgentDriver::new(harness.backend.clone());
    let handle = AgentRunHandle {
        run_id: "ghost".into(),
        session_ref: None,
    };
    assert!(matches!(
        driver.inspect(&handle).await,
        Err(DriverError::NotFound(id)) if id == "ghost"
    ));
    assert!(matches!(
        driver.cancel(&handle).await,
        Err(DriverError::NotFound(id)) if id == "ghost"
    ));
    assert!(matches!(
        driver
            .resume(ResumeRequest {
                run_id: "ghost".into(),
                prompt: "again".into(),
            })
            .await,
        Err(DriverError::Unsupported(_))
    ));
    harness.actor.abort();
}

#[test]
fn with_native_refuses_a_second_native_registration() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut registry = DriverRegistry::with_native(ChannelBackend::new(tx.clone()));
    let err = registry
        .register(Box::new(NativeAgentDriver::new(ChannelBackend::new(tx))))
        .expect_err("native is already registered");
    assert_eq!(err.id().as_str(), NATIVE_HARNESS_ID);
}

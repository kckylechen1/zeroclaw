// Derived from grok-build (Apache-2.0), revision
// 1adcd1f477870e4a97bacbd6be78c8a3bfbac46d, from
// `.../grok_build/task/backend_tests.rs`.
// Copyright 2023-2026 SpaceXAI. Licensed under the Apache License, Version 2.0.
//
// This file was CHANGED by ZeroClaw Labs: assertions were moved onto this
// crate's vocabulary (`CoordinatorError`, `ChildOutcome`, `ChildStatus`); the
// workflow half of the drop-cancel test went with the workflow owner; the
// upstream test that asserted a `tracing` WARN on validation timeout was
// dropped because this crate has no logging dependency (the outcome half of
// that test is kept). See ../LICENSE and ../NOTICE.

use super::*;
use crate::cancel::CancelToken;
use crate::outcome::ChildOutcome;
use crate::types::{ChildStatus, CoordinatorCommand, SpawnAdmission, SpawnRefusal};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Receive the next command, match the expected variant, or panic.
macro_rules! recv_command {
    ($rx:expr, $variant:ident) => {{
        let command = $rx.recv().await.unwrap();
        match command {
            CoordinatorCommand::$variant(inner) => inner,
            _ => panic!(
                "Expected CoordinatorCommand::{}, got different variant",
                stringify!($variant)
            ),
        }
    }};
}

fn request(id: &str) -> ChildRequest {
    ChildRequest {
        child_id: id.to_owned(),
        prompt: "do something".to_owned(),
        description: "test".to_owned(),
        agent_type: "general-purpose".to_owned(),
        parent_session_id: "parent".to_owned(),
        parent_alias: "parent-alias".to_owned(),
        parent_prompt_id: None,
        resume_from: None,
        cwd: None,
        overrides: Default::default(),
        run_in_background: false,
        surface_completion: true,
        await_to_completion: false,
        fork_context: false,
        cancel_token: CancelToken::new(),
    }
}

#[tokio::test]
async fn channel_backend_spawn_success() {
    let (tx, mut rx) = mpsc::unbounded_channel::<CoordinatorCommand>();
    let backend = ChannelBackend::new(tx);

    let handle = tokio::spawn(async move {
        let command = recv_command!(rx, Spawn);
        assert_eq!(command.request.child_id, "test-id");
        assert_eq!(command.request.prompt, "do something");
        command
            .admission_tx
            .send(SpawnAdmission::Admitted)
            .expect("the caller awaits admission before the result");
        command
            .result_tx
            .send(ChildResult {
                outcome: ChildOutcome::Completed,
                output: Arc::from("done"),
                child_id: "test-id".to_owned(),
                child_session_id: "test-id".to_owned(),
                tool_calls: 3,
                turns: 1,
                duration_ms: 500,
                ..Default::default()
            })
            .unwrap();
    });

    let result = backend.spawn(request("test-id")).await.unwrap();
    assert!(result.is_success());
    assert_eq!(result.child_id, "test-id");
    assert_eq!(result.tool_calls, 3);

    handle.await.unwrap();
}

#[tokio::test]
async fn channel_backend_spawn_closed_channel() {
    let (tx, rx) = mpsc::unbounded_channel::<CoordinatorCommand>();
    drop(rx);
    let backend = ChannelBackend::new(tx);

    let err = backend.spawn(request("test-id")).await.unwrap_err();
    assert_eq!(err, CoordinatorError::ChannelClosed);
    assert!(err.to_string().contains("channel closed"), "error: {err}");
}

#[tokio::test]
async fn channel_backend_spawn_result_dropped() {
    let (tx, mut rx) = mpsc::unbounded_channel::<CoordinatorCommand>();
    let backend = ChannelBackend::new(tx);

    let handle = tokio::spawn(async move {
        let command = recv_command!(rx, Spawn);
        // Admitted first: this test is about losing the *outcome* of a child
        // that was allowed to run, which is a different failure from never
        // being decided on (`channel_backend_spawn_admission_dropped`).
        command
            .admission_tx
            .send(SpawnAdmission::Admitted)
            .expect("the caller awaits admission before the result");
        drop(command.result_tx);
    });

    let err = backend.spawn(request("drop-test")).await.unwrap_err();
    assert_eq!(err, CoordinatorError::ResultChannelDropped);
    assert!(
        err.to_string().contains("result channel dropped"),
        "error: {err}"
    );

    handle.await.unwrap();
}

/// A refusal answers the admission channel and never the result channel, so a
/// caller must learn *why* nothing ran without ever touching `result_tx`.
/// The `timeout` is the guard the previous seat learned to need: if `spawn`
/// ever went back to awaiting the result for a refused child, this would hang
/// the binary rather than go red.
#[tokio::test]
async fn channel_backend_spawn_refusal_arrives_without_a_child_result() {
    let (tx, mut rx) = mpsc::unbounded_channel::<CoordinatorCommand>();
    let backend = ChannelBackend::new(tx);

    let handle = tokio::spawn(async move {
        let command = recv_command!(rx, Spawn);
        command
            .admission_tx
            .send(SpawnAdmission::Refused(
                SpawnRefusal::ChildCapacityReached {
                    in_flight: 4,
                    max: 4,
                },
            ))
            .expect("the caller awaits admission");
        // Deliberately dropped, unanswered: there is no run to report.
        drop(command.result_tx);
    });

    let err = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        backend.spawn(request("refused-test")),
    )
    .await
    .expect("a refusal must answer immediately, without waiting on a child")
    .unwrap_err();
    assert_eq!(
        err,
        CoordinatorError::Refused(SpawnRefusal::ChildCapacityReached {
            in_flight: 4,
            max: 4
        }),
        "a refusal must keep its structure, not collapse into a lost-reply error"
    );
    assert!(
        err.to_string().contains("too many children in flight"),
        "error: {err}"
    );

    handle.await.unwrap();
}

/// An admission channel dropped without an answer is "not admitted / unknown",
/// and must not be reported as a lost child result — no child ever ran.
#[tokio::test]
async fn channel_backend_spawn_admission_dropped() {
    let (tx, mut rx) = mpsc::unbounded_channel::<CoordinatorCommand>();
    let backend = ChannelBackend::new(tx);

    let handle = tokio::spawn(async move {
        let command = recv_command!(rx, Spawn);
        drop(command);
    });

    let err = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        backend.spawn(request("undecided-test")),
    )
    .await
    .expect("a dropped admission channel must resolve, not hang")
    .unwrap_err();
    assert_eq!(err, CoordinatorError::AdmissionChannelDropped);
    assert!(err.to_string().contains("never decided"), "error: {err}");

    handle.await.unwrap();
}

/// A task-owned spawn whose caller goes away does NOT cancel the child: the
/// work keeps running and its ending is surfaced later. Upstream's workflow
/// half of this test went with the workflow owner.
#[tokio::test]
async fn task_spawn_future_drop_does_not_cancel_the_child() {
    let (tx, mut rx) = mpsc::unbounded_channel::<CoordinatorCommand>();
    let backend = Arc::new(ChannelBackend::new(tx));
    let request = request("drop-owner-test");
    let cancel_token = request.cancel_token.clone();

    let task = tokio::spawn({
        let backend = backend.clone();
        async move { backend.spawn(request).await }
    });
    let spawned = recv_command!(rx, Spawn);
    task.abort();
    let _ = task.await;

    assert!(
        !cancel_token.is_cancelled(),
        "dropping the spawn future must not cancel a task-owned child"
    );
    drop(spawned.result_tx);
}

#[tokio::test]
async fn channel_backend_query_found() {
    let (tx, mut rx) = mpsc::unbounded_channel::<CoordinatorCommand>();
    let backend = ChannelBackend::new(tx);

    let handle = tokio::spawn(async move {
        let command = recv_command!(rx, Query);
        assert_eq!(command.child_id, "sub-1");
        assert!(command.block);
        assert_eq!(command.timeout_ms, Some(5000));
        command
            .respond_to
            .send(Some(ChildSnapshot {
                child_id: "sub-1".to_owned(),
                description: "find bugs".to_owned(),
                agent_type: "explore".to_owned(),
                status: ChildStatus::Finished {
                    outcome: ChildOutcome::Completed,
                    output: "result".to_owned(),
                    detail: None,
                    tool_calls: 2,
                    turns: 1,
                    worktree_path: None,
                },
                started_at_epoch_ms: 1000,
                duration_ms: 200,
                persona: Some("reviewer".to_owned()),
            }))
            .unwrap();
    });

    let snapshot = backend
        .query("sub-1", true, Some(5000))
        .await
        .expect("snapshot should be present");
    assert_eq!(snapshot.child_id, "sub-1");
    assert_eq!(snapshot.description, "find bugs");
    assert_eq!(snapshot.agent_type, "explore");
    assert_eq!(snapshot.started_at_epoch_ms, 1000);
    assert_eq!(snapshot.duration_ms, 200);
    assert_eq!(snapshot.persona.as_deref(), Some("reviewer"));
    match &snapshot.status {
        ChildStatus::Finished {
            outcome,
            output,
            tool_calls,
            turns,
            worktree_path,
            ..
        } => {
            assert_eq!(*outcome, ChildOutcome::Completed);
            assert_eq!(output, "result");
            assert_eq!(*tool_calls, 2);
            assert_eq!(*turns, 1);
            assert!(worktree_path.is_none());
        }
        other => panic!("Expected Finished, got {other:?}"),
    }

    handle.await.unwrap();
}

#[tokio::test]
async fn channel_backend_query_non_blocking_passes_through() {
    let (tx, mut rx) = mpsc::unbounded_channel::<CoordinatorCommand>();
    let backend = ChannelBackend::new(tx);

    let handle = tokio::spawn(async move {
        let command = recv_command!(rx, Query);
        assert_eq!(command.child_id, "sub-nb");
        assert!(!command.block, "block should be false");
        assert_eq!(command.timeout_ms, None, "timeout_ms should be None");
        command.respond_to.send(None).unwrap();
    });

    assert!(backend.query("sub-nb", false, None).await.is_none());
    handle.await.unwrap();
}

#[tokio::test]
async fn channel_backend_query_not_found() {
    let (tx, mut rx) = mpsc::unbounded_channel::<CoordinatorCommand>();
    let backend = ChannelBackend::new(tx);

    let handle = tokio::spawn(async move {
        let command = recv_command!(rx, Query);
        command.respond_to.send(None).unwrap();
    });

    assert!(backend.query("nonexistent", false, None).await.is_none());
    handle.await.unwrap();
}

#[tokio::test]
async fn channel_backend_query_closed_channel() {
    let (tx, rx) = mpsc::unbounded_channel::<CoordinatorCommand>();
    drop(rx);
    let backend = ChannelBackend::new(tx);
    assert!(backend.query("sub-1", false, None).await.is_none());
}

#[tokio::test]
async fn channel_backend_cancel_success() {
    let (tx, mut rx) = mpsc::unbounded_channel::<CoordinatorCommand>();
    let backend = ChannelBackend::new(tx);

    let handle = tokio::spawn(async move {
        let command = recv_command!(rx, Cancel);
        match &command.target {
            CancelTarget::ChildId(id) => assert_eq!(id, "sub-cancel"),
            other => panic!("Expected ChildId, got {other:?}"),
        }
        command.respond_to.send(CancelOutcome::Cancelled).unwrap();
    });

    assert!(matches!(
        backend.cancel("sub-cancel").await,
        CancelOutcome::Cancelled
    ));
    handle.await.unwrap();
}

#[tokio::test]
async fn channel_backend_cancel_closed_channel() {
    let (tx, rx) = mpsc::unbounded_channel::<CoordinatorCommand>();
    drop(rx);
    let backend = ChannelBackend::new(tx);
    assert!(matches!(
        backend.cancel("sub-cancel").await,
        CancelOutcome::NotFound
    ));
}

// ── validate_type ────────────────────────────────────────────────

#[tokio::test]
async fn channel_backend_validate_type_round_trips_outcome() {
    let (tx, mut rx) = mpsc::unbounded_channel::<CoordinatorCommand>();
    let backend = ChannelBackend::new(tx);

    let handle = tokio::spawn(async move {
        let command = recv_command!(rx, ValidateType);
        assert_eq!(command.agent_type, "explore");
        assert_eq!(command.parent_session_id, "parent-1");
        command.respond_to.send(ValidateTypeOutcome::Ok).unwrap();
    });

    assert!(matches!(
        backend.validate_type("explore", "parent-1").await,
        ValidateTypeOutcome::Ok
    ));
    handle.await.unwrap();
}

#[tokio::test]
async fn channel_backend_validate_type_propagates_unknown_outcome() {
    let (tx, mut rx) = mpsc::unbounded_channel::<CoordinatorCommand>();
    let backend = ChannelBackend::new(tx);

    let handle = tokio::spawn(async move {
        let command = recv_command!(rx, ValidateType);
        command
            .respond_to
            .send(ValidateTypeOutcome::Unknown {
                available: vec!["explore".into(), "plan".into()],
            })
            .unwrap();
    });

    match backend.validate_type("invented", "p").await {
        ValidateTypeOutcome::Unknown { available } => {
            assert_eq!(available, vec!["explore".to_owned(), "plan".to_owned()]);
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
    handle.await.unwrap();
}

#[tokio::test]
async fn channel_backend_validate_type_returns_validation_unavailable_when_channel_closed() {
    let (tx, rx) = mpsc::unbounded_channel::<CoordinatorCommand>();
    drop(rx);
    let backend = ChannelBackend::new(tx);
    assert!(matches!(
        backend.validate_type("explore", "p").await,
        ValidateTypeOutcome::ValidationUnavailable
    ));
}

#[tokio::test]
async fn channel_backend_validate_type_returns_validation_unavailable_when_responder_dropped() {
    let (tx, mut rx) = mpsc::unbounded_channel::<CoordinatorCommand>();
    let backend = ChannelBackend::new(tx);
    let handle = tokio::spawn(async move {
        let command = recv_command!(rx, ValidateType);
        drop(command.respond_to);
    });
    assert!(matches!(
        backend.validate_type("explore", "p").await,
        ValidateTypeOutcome::ValidationUnavailable
    ));
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn channel_backend_validate_type_returns_validation_unavailable_on_timeout() {
    let (tx, mut rx) = mpsc::unbounded_channel::<CoordinatorCommand>();
    let backend = ChannelBackend::new(tx);

    // The coordinator receives but never replies, keeping the responder alive
    // so the timeout arm fires rather than the responder-dropped arm.
    let holder = tokio::spawn(async move {
        let command = recv_command!(rx, ValidateType);
        std::mem::forget(command.respond_to);
        std::future::pending::<()>().await;
    });

    let validate = tokio::spawn(async move { backend.validate_type("explore", "p").await });
    tokio::time::advance(VALIDATE_TYPE_TIMEOUT + std::time::Duration::from_millis(1)).await;
    assert!(matches!(
        validate.await.unwrap(),
        ValidateTypeOutcome::ValidationUnavailable
    ));
    holder.abort();
}

// ── describe_agent_type ──────────────────────────────────────────

#[tokio::test]
async fn channel_backend_describe_round_trips_summary() {
    use crate::types::{ChildTypeSummary, DescribeOutcome};

    let (tx, mut rx) = mpsc::unbounded_channel::<CoordinatorCommand>();
    let backend = ChannelBackend::new(tx);

    let handle = tokio::spawn(async move {
        let command = recv_command!(rx, DescribeType);
        assert_eq!(command.agent_type, "explore");
        assert_eq!(command.harness_agent_type.as_deref(), Some("cursor"));
        assert_eq!(command.parent_session_id, "parent-1");
        let mut summary = ChildTypeSummary {
            can_read: true,
            can_search: true,
            ..Default::default()
        };
        summary
            .tool_names
            .insert("read".to_owned(), "read_file".to_owned());
        command
            .respond_to
            .send(DescribeOutcome::Ok(summary))
            .unwrap();
    });

    match backend
        .describe_agent_type("explore", Some("cursor"), "parent-1")
        .await
    {
        DescribeOutcome::Ok(summary) => {
            assert!(summary.can_read && summary.can_search && !summary.can_execute);
            assert_eq!(summary.tool_names.get("read").unwrap(), "read_file");
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    handle.await.unwrap();
}

#[tokio::test]
async fn channel_backend_describe_propagates_not_allowed_outcome() {
    use crate::types::DescribeOutcome;

    let (tx, mut rx) = mpsc::unbounded_channel::<CoordinatorCommand>();
    let backend = ChannelBackend::new(tx);

    let handle = tokio::spawn(async move {
        let command = recv_command!(rx, DescribeType);
        command
            .respond_to
            .send(DescribeOutcome::NotAllowed {
                allowed: vec!["explore".into()],
            })
            .unwrap();
    });

    match backend.describe_agent_type("plan", None, "p").await {
        DescribeOutcome::NotAllowed { allowed } => {
            assert_eq!(allowed, vec!["explore".to_owned()]);
        }
        other => panic!("expected NotAllowed, got {other:?}"),
    }
    handle.await.unwrap();
}

#[tokio::test]
async fn channel_backend_describe_returns_unavailable_when_channel_closed() {
    use crate::types::DescribeOutcome;
    let (tx, rx) = mpsc::unbounded_channel::<CoordinatorCommand>();
    drop(rx);
    let backend = ChannelBackend::new(tx);
    assert!(matches!(
        backend.describe_agent_type("explore", None, "p").await,
        DescribeOutcome::Unavailable
    ));
}

#[tokio::test]
async fn channel_backend_describe_returns_unavailable_when_responder_dropped() {
    use crate::types::DescribeOutcome;
    let (tx, mut rx) = mpsc::unbounded_channel::<CoordinatorCommand>();
    let backend = ChannelBackend::new(tx);
    let handle = tokio::spawn(async move {
        let command = recv_command!(rx, DescribeType);
        drop(command.respond_to);
    });
    assert!(matches!(
        backend.describe_agent_type("explore", None, "p").await,
        DescribeOutcome::Unavailable
    ));
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn channel_backend_describe_returns_unavailable_on_timeout() {
    use crate::types::DescribeOutcome;
    let (tx, mut rx) = mpsc::unbounded_channel::<CoordinatorCommand>();
    let backend = ChannelBackend::new(tx);

    let holder = tokio::spawn(async move {
        let command = recv_command!(rx, DescribeType);
        std::mem::forget(command.respond_to);
        std::future::pending::<()>().await;
    });

    let describe =
        tokio::spawn(async move { backend.describe_agent_type("explore", None, "p").await });
    tokio::time::advance(VALIDATE_TYPE_TIMEOUT + std::time::Duration::from_millis(1)).await;
    assert!(matches!(
        describe.await.unwrap(),
        DescribeOutcome::Unavailable
    ));
    holder.abort();
}

// ── timeout parsing ──────────────────────────────────────────────

#[test]
fn parse_timeout_ms_returns_none_for_unset() {
    assert_eq!(parse_timeout_ms(None), None);
}

#[test]
fn parse_timeout_ms_returns_none_for_unparseable() {
    assert_eq!(parse_timeout_ms(Some("not-a-number")), None);
    assert_eq!(parse_timeout_ms(Some("")), None);
    assert_eq!(parse_timeout_ms(Some("3.14")), None);
    assert_eq!(parse_timeout_ms(Some("-100")), None);
}

#[test]
fn parse_timeout_ms_returns_none_for_zero() {
    assert_eq!(parse_timeout_ms(Some("0")), None);
}

#[test]
fn parse_timeout_ms_returns_value_for_positive_integer() {
    assert_eq!(parse_timeout_ms(Some("5000")), Some(5000));
    assert_eq!(parse_timeout_ms(Some("1")), Some(1));
}

#[test]
fn env_duration_falls_back_when_var_is_unset() {
    let default = std::time::Duration::from_millis(1234);
    assert_eq!(
        env_duration_or("ZEROCLAW_COORDINATOR_TEST_VAR_THAT_IS_UNSET", default),
        default
    );
}

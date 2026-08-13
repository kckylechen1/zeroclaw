//! Wiring-phase-2a implementation of `zeroclaw_coordinator::ChildPersistence`
//! against the control-plane's SQLite task store.
//!
//! `ChildPersistence` is defined but not implemented in `zeroclaw-coordinator`
//! (see that crate's `persistence` module doc) because the coordinator crate
//! is not allowed to know about `zeroclaw-runtime`'s `TaskStatus`/`TaskRecord`
//! vocabulary. This module is the other half: it lives downstream, where both
//! vocabularies are in scope, and does nothing but translate between them.
//!
//! ## Why this holds `Arc<SqliteTaskStore>`, not `ControlPlaneHandle`
//!
//! `ChildPersistence::record_spawn`/`record_finish` are synchronous `&mut
//! self` methods (the coordinator actor is a single-writer state machine, not
//! an async pipeline — see the trait's own doc for why). `ControlPlaneHandle`
//! only exposes `Arc<dyn TaskRegistry>`, whose methods are all `async fn`.
//! `SqliteTaskStore::create_sync`/`finish_task_sync` are the synchronous
//! entry points that lock the same `Mutex<Connection>` and call the same
//! record functions the async path does — so this type holds the concrete
//! store directly. The coordinator carries its port as a generic field
//! (`Coordinator::with_persistence`); plugging THIS implementation in at a
//! daemon boot site is the remaining wiring, owned by the phase that
//! instantiates the actor.
//!
//! ## The `agent` / `executor` / `parent_id` field choice
//!
//! `TaskRecord::agent` is the **owning** parent alias (alias-delete cascades).
//! `TaskRecord::executor` is who actually ran — `ChildRequest::agent_type`.
//! [`zeroclaw_api::announce::Announcement::agent`] is the executor
//! (`COALESCE(executor, agent)` at claim time). `parent_id` stays
//! `parent_session_id`: session identity is what
//! `claim_undelivered_children` keys children under.
//!
//! Background `delegate` children are `TaskKind::Delegate`; `spawn_subagent`
//! children stay `TaskKind::Subagent`. The discriminator is
//! `ChildOverrides::hosted_run`.
//!
//! ## Error posture
//!
//! Store failures are returned as `Err(PersistenceError)`, not logged here:
//! the coordinator logs a write failure once, at its own call sites, the
//! same way for every implementation. Loud in one place, not half-loud in
//! two.

use std::sync::Arc;

use zeroclaw_coordinator::{
    ChildOutcome, ChildPersistence, ChildRequest, ChildResult, PersistenceError,
};

use super::task_registry::{TaskKind, TaskRecord, TaskStatus};
use super::task_store_sqlite::SqliteTaskStore;

/// `ChildPersistence` backed by the control-plane's SQLite task store.
pub struct SubagentPersistence {
    store: Arc<SqliteTaskStore>,
    owner_boot_id: String,
}

impl SubagentPersistence {
    #[must_use]
    pub fn new(store: Arc<SqliteTaskStore>, owner_boot_id: String) -> Self {
        Self {
            store,
            owner_boot_id,
        }
    }
}

/// Total map from a child's outcome to the control plane's terminal
/// vocabulary.
///
/// Exhaustive, no wildcard arm — mirrors `zeroclaw_coordinator::outcome`'s
/// own `From<ChildOutcome> for AnnouncedOutcome`, and for the same reason: if
/// either enum gains a variant, this match stops compiling instead of
/// silently swallowing the new case into some existing arm. Every arm here
/// lands on a status inside `TaskStatus::TERMINAL` — checked by
/// `every_child_outcome_maps_to_a_status_the_store_accepts_as_terminal` below
/// — so the result is always a legal `finish_task`/`finish_task_sync` input;
/// there is no case where this function hands back a non-terminal status.
#[must_use]
pub fn child_outcome_to_task_status(outcome: ChildOutcome) -> TaskStatus {
    match outcome {
        ChildOutcome::Completed => TaskStatus::Completed,
        ChildOutcome::Failed => TaskStatus::Failed,
        ChildOutcome::Cancelled => TaskStatus::Cancelled,
        ChildOutcome::TimedOut => TaskStatus::TimedOut,
        ChildOutcome::Lost => TaskStatus::Lost,
    }
}

/// Inverse of [`child_outcome_to_task_status`] for rows loaded back out of
/// the store. `None` for a non-terminal status — those are not a finished
/// child.
#[must_use]
pub fn task_status_to_child_outcome(status: TaskStatus) -> Option<ChildOutcome> {
    Some(match status {
        TaskStatus::Completed => ChildOutcome::Completed,
        TaskStatus::Failed => ChildOutcome::Failed,
        TaskStatus::Cancelled => ChildOutcome::Cancelled,
        TaskStatus::TimedOut => ChildOutcome::TimedOut,
        TaskStatus::Lost => ChildOutcome::Lost,
        TaskStatus::Running | TaskStatus::Paused => return None,
    })
}

fn rfc3339_to_epoch_ms(value: &str) -> u64 {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| u64::try_from(dt.timestamp_millis()).unwrap_or(0))
        .unwrap_or(0)
}

impl ChildPersistence for SubagentPersistence {
    /// Create the row for a newly spawned child, in `Running` — the only
    /// non-terminal state a coordinator-spawned child is ever written in.
    ///
    /// This is the production writer of `parent_id`: coordinator-spawned
    /// children (`spawn_subagent`, background `delegate`) carry a
    /// caller-supplied parent identity on `ChildRequest`.
    fn record_spawn(&mut self, request: &ChildRequest) -> Result<(), PersistenceError> {
        let rec = TaskRecord {
            id: request.child_id.clone(),
            kind: if request.overrides.hosted_run {
                TaskKind::Delegate
            } else {
                TaskKind::Subagent
            },
            agent: request.parent_alias.clone(),
            status: TaskStatus::Running,
            owner_pid: std::process::id(),
            owner_boot_id: self.owner_boot_id.clone(),
            heartbeat_at: None,
            depth: request.overrides.spawn_depth.unwrap_or(0),
            parent_id: Some(request.parent_session_id.clone()),
            originator_route: None,
            delivered: false,
            idem_key: None,
            principal_id: None,
            executor: Some(request.agent_type.clone()),
            started_at: chrono::Utc::now().to_rfc3339(),
            finished_at: None,
        };
        self.store
            .create_sync(rec)
            .map_err(|e| PersistenceError(format!("{e:#}")))
    }

    /// Write a child's ending in one statement — `finish_task_sync` (the
    /// sync twin of [`super::task_registry::TaskRegistry::finish_task`])
    /// already carries terminal status, output, error and `delivered`
    /// together; this method only maps `ChildResult`'s shape onto its
    /// parameters, it does not perform a second write.
    fn record_finish(
        &mut self,
        child_id: &str,
        result: &ChildResult,
        delivered: bool,
    ) -> Result<(), PersistenceError> {
        let status = child_outcome_to_task_status(result.outcome);
        let output = result.output.as_ref();
        let error = result.detail.as_deref();
        // `finish_task_sync` returns whether a row actually transitioned.
        // For a coordinator child the spawn row always precedes the finish,
        // so "no non-terminal row matched" is an anomaly worth surfacing,
        // not a silent no-op — without it a lost spawn write would make the
        // finish vanish too and nobody would ever hear about either.
        match self
            .store
            .finish_task_sync(child_id, status, Some(output), error, delivered)
        {
            Ok(true) => Ok(()),
            Ok(false) => Err(PersistenceError(format!(
                "no non-terminal row for child {child_id:?}; spawn write missing or row already finished"
            ))),
            Err(e) => Err(PersistenceError(format!("{e:#}"))),
        }
    }

    fn load_finished(
        &self,
        child_id: &str,
    ) -> Option<zeroclaw_coordinator::PersistedFinishedChild> {
        let view = self
            .store
            .get_terminal_with_result(child_id)
            .ok()
            .flatten()?;
        let outcome = task_status_to_child_outcome(view.record.status)?;
        let started_at_epoch_ms = rfc3339_to_epoch_ms(&view.record.started_at);
        let finished_at_epoch_ms = view
            .record
            .finished_at
            .as_deref()
            .map(rfc3339_to_epoch_ms)
            .unwrap_or(started_at_epoch_ms);
        Some(zeroclaw_coordinator::PersistedFinishedChild {
            child_id: view.record.id,
            agent_type: view
                .record
                .executor
                .clone()
                .unwrap_or_else(|| view.record.agent.clone()),
            parent_session_id: view.record.parent_id.unwrap_or_default(),
            outcome,
            output: view.output.unwrap_or_default(),
            detail: view.error,
            started_at_epoch_ms,
            duration_ms: finished_at_epoch_ms.saturating_sub(started_at_epoch_ms),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zeroclaw_coordinator::{
        CancelToken, ChildOutcome, ChildOverrides, ChildRequest, ChildResult,
    };

    use super::*;
    use crate::control_plane::task_registry::TaskRegistry;

    fn request(child_id: &str, parent_session_id: &str) -> ChildRequest {
        ChildRequest {
            child_id: child_id.into(),
            prompt: "do it".into(),
            description: "d".into(),
            agent_type: "explore".into(),
            parent_alias: "parent-alias".into(),
            parent_session_id: parent_session_id.into(),
            parent_prompt_id: None,
            resume_from: None,
            cwd: None,
            overrides: ChildOverrides::default(),
            run_in_background: false,
            surface_completion: true,
            await_to_completion: false,
            fork_context: false,
            cancel_token: CancelToken::new(),
        }
    }

    fn harness() -> (SubagentPersistence, Arc<SqliteTaskStore>) {
        let store = Arc::new(SqliteTaskStore::new_in_memory().unwrap());
        (
            SubagentPersistence::new(Arc::clone(&store), "boot-1".into()),
            store,
        )
    }

    /// Pins the ordering contract `claim_undelivered_children`'s own doc
    /// describes: a non-terminal row (spawned, still running) is invisible
    /// to the claim query, and `record_spawn` is the write that finally puts
    /// `parent_id` on the row at all.
    #[tokio::test]
    async fn spawn_write_sets_parent_id_and_is_invisible_to_claim_while_running() {
        let (mut persistence, store) = harness();
        persistence
            .record_spawn(&request("kid", "mum"))
            .expect("spawn write");

        let rec = store.get("kid").await.unwrap().expect("row must exist");
        assert_eq!(rec.parent_id.as_deref(), Some("mum"));
        assert_eq!(
            rec.agent, "parent-alias",
            "agent column carries the owning parent alias"
        );
        assert_eq!(
            rec.executor.as_deref(),
            Some("explore"),
            "executor is the child that ran"
        );
        assert_eq!(rec.status, TaskStatus::Running);
        assert_eq!(rec.kind, TaskKind::Subagent);

        assert!(
            store
                .claim_undelivered_children("mum")
                .await
                .unwrap()
                .is_empty(),
            "a still-running child must not be claimable"
        );
    }

    #[tokio::test]
    async fn finish_delivered_false_is_claimed_exactly_once_with_output_and_error_intact() {
        let (mut persistence, store) = harness();
        persistence
            .record_spawn(&request("kid", "mum"))
            .expect("spawn write");

        let result = ChildResult {
            outcome: ChildOutcome::Failed,
            output: Arc::from("partial output"),
            detail: Some("boom".into()),
            child_id: "kid".into(),
            ..Default::default()
        };
        persistence
            .record_finish("kid", &result, false)
            .expect("finish write");

        let claimed = store.claim_undelivered_children("mum").await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].task_id, "kid");
        assert_eq!(
            claimed[0].agent, "explore",
            "announcement.agent is the executor, not the owning parent"
        );
        assert_eq!(claimed[0].output.as_deref(), Some("partial output"));
        assert_eq!(claimed[0].detail.as_deref(), Some("boom"));

        assert!(
            store
                .claim_undelivered_children("mum")
                .await
                .unwrap()
                .is_empty(),
            "a second claim must not re-announce an already-claimed completion"
        );
    }

    #[tokio::test]
    async fn finish_delivered_true_is_never_claimed() {
        let (mut persistence, store) = harness();
        persistence
            .record_spawn(&request("kid", "mum"))
            .expect("spawn write");

        let result = ChildResult {
            outcome: ChildOutcome::Completed,
            output: Arc::from("done"),
            child_id: "kid".into(),
            ..Default::default()
        };
        persistence
            .record_finish("kid", &result, true)
            .expect("finish write");

        assert!(
            store
                .claim_undelivered_children("mum")
                .await
                .unwrap()
                .is_empty(),
            "a child finished with delivered = true must never be claimable"
        );
    }

    /// Every `ChildOutcome` variant must round-trip into a `TaskStatus` the
    /// store accepts as terminal. The call's own `Result` now surfaces a
    /// rejected write, but this test still does not trust the call alone —
    /// it reads the row back and asserts the status actually changed, which
    /// is what would go red if `finish_task_sync` ever rejected one of these
    /// as non-terminal (see `finish_task_record`'s `anyhow::ensure!`).
    #[tokio::test]
    async fn every_child_outcome_maps_to_a_status_the_store_accepts_as_terminal() {
        for outcome in [
            ChildOutcome::Completed,
            ChildOutcome::Failed,
            ChildOutcome::Cancelled,
            ChildOutcome::TimedOut,
            ChildOutcome::Lost,
        ] {
            let (mut persistence, store) = harness();
            let child_id = format!("kid-{outcome:?}");
            persistence
                .record_spawn(&request(&child_id, "mum"))
                .expect("spawn write");

            let status = child_outcome_to_task_status(outcome);
            assert!(
                status.is_terminal(),
                "{outcome:?} mapped to non-terminal status {status:?}"
            );

            let result = ChildResult {
                outcome,
                output: Arc::from("x"),
                child_id: child_id.clone(),
                ..Default::default()
            };
            persistence
                .record_finish(&child_id, &result, false)
                .expect("finish write");

            let rec = store.get(&child_id).await.unwrap().unwrap();
            assert_eq!(
                rec.status, status,
                "{outcome:?} did not actually transition the row to the mapped status"
            );
        }
    }

    #[tokio::test]
    async fn hosted_run_is_delegate_kind_and_claim_names_the_executor() {
        let (mut persistence, store) = harness();
        let mut req = request("kid", "mum");
        req.overrides.hosted_run = true;
        req.agent_type = "researcher".into();
        persistence.record_spawn(&req).expect("spawn write");

        let rec = store.get("kid").await.unwrap().expect("row");
        assert_eq!(rec.kind, TaskKind::Delegate);
        assert_eq!(rec.agent, "parent-alias");
        assert_eq!(rec.executor.as_deref(), Some("researcher"));

        persistence
            .record_finish(
                "kid",
                &ChildResult {
                    outcome: ChildOutcome::Completed,
                    output: Arc::from("done"),
                    child_id: "kid".into(),
                    ..Default::default()
                },
                false,
            )
            .expect("finish");

        let claimed = store.claim_undelivered_children("mum").await.unwrap();
        assert_eq!(claimed[0].agent, "researcher");
    }
}

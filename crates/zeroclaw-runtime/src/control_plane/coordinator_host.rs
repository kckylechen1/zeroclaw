//! Boots the `zeroclaw_coordinator::Coordinator` actor onto the daemon's
//! runtime and hands back the command channel tools reach it through.
//!
//! Everything the actor needs already exists — `Coordinator`, `SubagentPersistence`
//! (this crate's `ChildPersistence` impl over the control-plane's SQLite store),
//! and `NativeChildRunner` (`crate::subagent_host`, the `ChildRunner` impl that
//! runs a child as a native in-process agent turn) — but nothing in production
//! code constructs them together. This module is that construction.
//!
//! ## Boot-order contract
//!
//! [`start`] must run strictly AFTER [`super::boot::ControlPlaneHandle::start_with_boot_id`]
//! has returned — i.e. after the recovery pass has already reclaimed prior-boot
//! orphan rows for this `boot_id`. A spawn accepted before recovery finishes could
//! write a fresh `Running` row for a child that races the reaper's prior-boot sweep
//! over the very table recovery is still reconciling (`reaper::recovery_pass`
//! reclaims every `Running` row NOT owned by the current `boot_id`; a child this
//! actor spawns is stamped with the current `boot_id` from the moment
//! `record_spawn` writes it, so once recovery has run there is no longer a row for
//! it to collide with). This module does not call `start_with_boot_id` itself —
//! see that function's doc for why the two are kept separate — so the ordering is
//! the caller's responsibility: the daemon boot path (`daemon::run`) calls
//! `ControlPlaneHandle::start` to completion, THEN calls [`start`] against the
//! returned handle's `sqlite_store`/`boot_id`.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use zeroclaw_config::schema::Config;
use zeroclaw_coordinator::{CommandSender, Coordinator, CoordinatorConfig};

use super::subagent_persistence::SubagentPersistence;
use super::task_store_sqlite::SqliteTaskStore;
use crate::subagent_host::NativeChildRunner;

/// The live coordinator actor, freshly spawned onto the runtime.
pub struct CoordinatorHost {
    /// Clonable handle tools dispatch `Spawn`/`Query`/... commands through —
    /// this is what gets attached to [`super::boot::ControlPlaneHandle::commands`].
    pub commands: CommandSender,
    /// The actor's own task. Held so the caller can abort it at shutdown:
    /// `Coordinator`'s `Drop` impl ledgers every child still `pending` or
    /// `active` as `Lost` when the actor is torn down (see that type's doc),
    /// and that only fires once this task is actually dropped — aborting the
    /// handle and awaiting it is what makes that deterministic (the same
    /// pattern `zeroclaw-coordinator`'s own Drop tests use: see
    /// `coordinator_tests.rs`'s `drop_with_pending_and_active_children_records_one_lost_finish_each`).
    /// A caller that only reads `commands` and drops this handle without
    /// awaiting it still gets the sweep eventually (Rust runs `Drop` when the
    /// task's future is actually dropped, which happens once the runtime gets
    /// around to tearing down a cancelled task) — but not deterministically,
    /// which is why shutdown paths should abort AND await it.
    pub actor: JoinHandle<()>,
}

/// Build and spawn a coordinator actor backed by the control-plane's own
/// SQLite store, running native (`crate::agent::run`) children.
///
/// `sqlite_store` and `boot_id` should be the ones a
/// [`super::boot::ControlPlaneHandle`] already carries — see this module's
/// doc for the ordering requirement.
#[must_use]
pub fn start(config: Arc<Config>, sqlite_store: Arc<SqliteTaskStore>, boot_id: String) -> CoordinatorHost {
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let persistence = SubagentPersistence::new(sqlite_store, boot_id);
    let runner = NativeChildRunner::new(config);
    let coordinator =
        Coordinator::with_persistence(command_rx, runner, CoordinatorConfig::default(), persistence);
    let actor = zeroclaw_spawn::spawn!(coordinator.run());
    CoordinatorHost {
        commands: CommandSender(command_tx),
        actor,
    }
}

#[cfg(test)]
mod tests {
    use std::future::{Pending, Ready, ready};
    use std::pin::Pin;

    use tokio::sync::oneshot;
    use zeroclaw_config::schema::Config;
    use zeroclaw_coordinator::{
        CancelToken, ChildCompletion, ChildControl, ChildOutcome, ChildOverrides, ChildProgress,
        ChildRequest, ChildRunOutput, ChildRunRequest, ChildRunner, Coordinator,
        CoordinatorCommand, CoordinatorConfig, DescribeOutcome, SpawnCommand, ValidateTypeOutcome,
    };

    use super::*;
    use crate::control_plane::boot::ControlPlaneHandle;
    use crate::control_plane::task_registry::{TaskRegistry, TaskStatus};

    fn request(child_id: &str, agent_type: &str) -> ChildRequest {
        ChildRequest {
            child_id: child_id.into(),
            prompt: "do it".into(),
            description: "test child".into(),
            agent_type: agent_type.into(),
            parent_session_id: "parent-session".into(),
            parent_alias: "parent".into(),
            parent_prompt_id: None,
            resume_from: None,
            cwd: None,
            overrides: ChildOverrides::default(),
            run_in_background: false,
            surface_completion: true,
            await_to_completion: true,
            fork_context: false,
            cancel_token: CancelToken::new(),
        }
    }

    /// A booted control-plane's actor runs `NativeChildRunner` for real: a
    /// spawn naming an agent type nobody configured comes back as the
    /// runner's own validation failure, not a hang, a panic, or a generic
    /// coordinator error — proving the actor is alive, the runner answers,
    /// and [`start`]'s wiring reaches all the way from `CommandSender` to
    /// `NativeChildRunner::resolve_agent_type` end to end.
    #[tokio::test]
    async fn booted_coordinator_answers_a_spawn_for_an_unconfigured_agent_type() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            data_dir: dir.path().to_path_buf(),
            config_path: dir.path().join("config.toml"),
            ..Config::default()
        };
        // Same ordering as the real boot site (`daemon::run`): the plane
        // boots — and its recovery pass runs — against `config.data_dir`
        // BEFORE the coordinator is started against the resulting handle.
        let mut handle = ControlPlaneHandle::start(&config.data_dir).await.unwrap();
        let host = start(
            Arc::new(config),
            Arc::clone(&handle.sqlite_store),
            handle.boot_id.clone(),
        );
        handle.commands = Some(host.commands);

        let (result_tx, result_rx) = oneshot::channel();
        handle
            .commands
            .as_ref()
            .expect("attached above")
            .0
            .send(CoordinatorCommand::Spawn(SpawnCommand {
                request: Box::new(request("kid-1", "no-such-agent")),
                result_tx,
            }))
            .expect("the actor owns the receiver");

        let result = result_rx.await.expect("the actor replies to every spawn");
        assert_eq!(
            result.outcome,
            ChildOutcome::Failed,
            "an unconfigured agent type must fail validation, not run, got: {result:?}"
        );
        assert!(
            result
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("not a configured agent")),
            "detail must name the resolution failure, got: {:?}",
            result.detail
        );

        host.actor.abort();
        let _ = host.actor.await;
    }

    // ── Drop-sweep, against the real store ──────────────────────────────
    //
    // `NativeChildRunner` has no gate that holds a child `pending`/`active`
    // on demand — every path through `NativeChildRunner::run` either fails
    // before promotion (fast, synchronous) or promotes and then drives a
    // real agent turn (which this crate cannot make hang deterministically
    // without a live model provider). So the Drop-sweep test below uses a
    // minimal local `ChildRunner` whose run future never resolves, wired
    // through the SAME `Coordinator::with_persistence` + real
    // `SubagentPersistence` + real `SqliteTaskStore` this module's `start`
    // uses — everything downstream of "which runner" is production code,
    // exercised against a real sqlite file, not a mock. What this test does
    // NOT prove is that `NativeChildRunner` itself, mid-turn, leaves a
    // recoverable row; that would need a live model provider.

    struct NeverControl;

    impl ChildControl for NeverControl {
        type ProgressFuture = Ready<ChildProgress>;

        fn progress(&self) -> Self::ProgressFuture {
            ready(ChildProgress::default())
        }

        fn cancel(&self) {}
    }

    /// A `ChildRunner` whose child never promotes and never finishes on its
    /// own — the request stays `pending` for as long as the actor lives,
    /// which is what makes "abort the actor while this child is mid-flight"
    /// deterministic instead of a timing race.
    struct HangingRunner;

    impl ChildRunner for HangingRunner {
        type Control = NeverControl;
        type CompletionData = ();
        type RunFuture = Pin<Box<Pending<ChildRunOutput<()>>>>;
        type ValidateFuture = Ready<ValidateTypeOutcome>;
        type DescribeFuture = Ready<DescribeOutcome>;

        fn run(&self, _request: ChildRunRequest<Self::Control>) -> Self::RunFuture {
            Box::pin(std::future::pending())
        }

        fn validate_type(&self, _agent_type: String, _parent_session_id: String) -> Self::ValidateFuture {
            ready(ValidateTypeOutcome::Ok)
        }

        fn describe_type(
            &self,
            _agent_type: String,
            _harness_agent_type: Option<String>,
            _parent_session_id: String,
        ) -> Self::DescribeFuture {
            ready(DescribeOutcome::Unavailable)
        }

        fn on_completed(&self, _completion: ChildCompletion<Self::CompletionData>) {}
    }

    /// The whole chain end to end for the failure mode this phase exists to
    /// fix: a child mid-flight when the actor is torn down must not be left
    /// `Running` in the store forever. Spawn one child (never promoted,
    /// because `HangingRunner` never calls back), abort the actor, and read
    /// the row back from the SAME real `SqliteTaskStore` the control-plane
    /// handle carries — not a mock, not the in-process `RecordingPersistence`
    /// `zeroclaw-coordinator`'s own tests use.
    #[tokio::test]
    async fn drop_after_abort_marks_a_mid_flight_child_lost_in_the_real_store() {
        let dir = tempfile::tempdir().unwrap();
        let handle = ControlPlaneHandle::start(dir.path()).await.unwrap();

        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let persistence =
            SubagentPersistence::new(Arc::clone(&handle.sqlite_store), handle.boot_id.clone());
        let coordinator = Coordinator::with_persistence(
            command_rx,
            HangingRunner,
            CoordinatorConfig::default(),
            persistence,
        );
        let actor = zeroclaw_spawn::spawn!(coordinator.run());

        let (result_tx, _result_rx) = oneshot::channel();
        command_tx
            .send(CoordinatorCommand::Spawn(SpawnCommand {
                request: Box::new(request("mid-flight", "explore")),
                result_tx,
            }))
            .expect("the actor owns the receiver");
        // One yield is enough: `handle_spawn` inserts into `pending` and
        // calls `record_spawn` synchronously, within the same command-branch
        // arm of the actor's select loop — no `.await` in that path waits on
        // anything `HangingRunner` controls, so the row is written the first
        // time the actor task is polled after the send.
        tokio::task::yield_now().await;

        let running = handle
            .sqlite_store
            .get("mid-flight")
            .await
            .unwrap()
            .expect("record_spawn must have written the row before abort");
        assert_eq!(
            running.status,
            TaskStatus::Running,
            "sanity check: the child must still be Running before the actor is torn down"
        );

        actor.abort();
        // Awaiting the aborted handle is what makes `Coordinator::drop` (and
        // its `record_abandoned_children` sweep) run before this assertion,
        // not merely "requested" — see `coordinator_tests.rs`'s identical
        // comment on the same pattern.
        let _ = actor.await;

        let lost = handle
            .sqlite_store
            .get("mid-flight")
            .await
            .unwrap()
            .expect("the row must still exist after Drop");
        assert_eq!(
            lost.status,
            TaskStatus::Lost,
            "a child still pending/active when the actor is dropped must be \
             ledgered Lost in the real store, not left Running forever"
        );
        assert!(
            !lost.delivered,
            "nobody in-process ever received this result — delivered must be false"
        );
    }
}

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

use zeroclaw_coordinator::{ChildControl, ChildRunner};

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
    let runner = NativeChildRunner::new(Arc::clone(&config));
    start_with_runner(&config, runner, sqlite_store, boot_id)
}

/// Translate the operator's configuration into the actor's policy.
///
/// The one place `[subagents]` becomes `CoordinatorConfig`. Kept as a named
/// function so "which knob reaches the actor, and which is still a compiled-in
/// default" is answerable by reading nine lines rather than by auditing a
/// struct literal buried in a boot path: every field NOT named here is, by
/// construction, not configurable yet.
fn coordinator_config(config: &Config) -> CoordinatorConfig {
    CoordinatorConfig {
        // Daemon-wide, because this actor is one per process — see
        // `zeroclaw_config::subagents` for why there is no per-agent form.
        max_concurrent_children: config.subagents.max_concurrent_children,
        // Not configurable (yet). Spelled as an explicit fallback rather than
        // `..Default::default()` so adding a knob to `[subagents]` means
        // replacing a line here, and forgetting to do so is visible.
        ..CoordinatorConfig::default()
    }
}

/// [`start`], parameterised over the runner.
///
/// `start` is this function with `NativeChildRunner` supplied; everything
/// downstream of "which runner" — the config translation above, the
/// persistence seam, the actor spawn — is shared, so a test that swaps the
/// runner still exercises the production wiring rather than a copy of it.
/// `NativeChildRunner` cannot be made to hold a child in flight without a live
/// model provider (see the note above the Drop-sweep test below), and holding
/// children in flight is the only way to observe an admission gate at all.
fn start_with_runner<R>(
    config: &Config,
    runner: R,
    sqlite_store: Arc<SqliteTaskStore>,
    boot_id: String,
) -> CoordinatorHost
where
    R: ChildRunner + Send + 'static,
    R::Control: Send,
    R::CompletionData: Send,
    R::RunFuture: Send,
    R::ValidateFuture: Send,
    R::DescribeFuture: Send,
    <R::Control as ChildControl>::ProgressFuture: Send,
{
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let persistence = SubagentPersistence::new(sqlite_store, boot_id);
    let coordinator =
        Coordinator::with_persistence(command_rx, runner, coordinator_config(config), persistence);
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
        ChildRequest, ChildRunOutput, ChildRunRequest, ChildRunner, CoordinatorCommand,
        DescribeOutcome, SpawnAdmission, SpawnCommand, ValidateTypeOutcome,
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

        let (admission_tx, admission_rx) = oneshot::channel();
        let (result_tx, result_rx) = oneshot::channel();
        handle
            .commands
            .as_ref()
            .expect("attached above")
            .0
            .send(CoordinatorCommand::Spawn(SpawnCommand {
                request: Box::new(request("kid-1", "no-such-agent")),
                admission_tx,
                result_tx,
            }))
            .expect("the actor owns the receiver");

        // An unconfigured agent type is a *runner* validation failure, not an
        // admission gate: the coordinator admits the child and the runner then
        // refuses it. Pinning that here keeps the two apart — if this ever
        // flipped to a refusal, the result assertion below would be about a
        // child that never ran.
        assert_eq!(
            admission_rx.await,
            Ok(SpawnAdmission::Admitted),
            "an unknown agent type must be admitted and then fail in the runner"
        );
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

        // Built through `start_with_runner` — the same function `start` calls —
        // so this exercises production wiring with only the runner swapped,
        // rather than a hand-rolled copy that can drift away from it.
        let host = start_with_runner(
            &Config::default(),
            HangingRunner,
            Arc::clone(&handle.sqlite_store),
            handle.boot_id.clone(),
        );
        let actor = host.actor;
        let command_tx = host.commands.0;

        let (admission_tx, admission_rx) = oneshot::channel();
        let (result_tx, _result_rx) = oneshot::channel();
        command_tx
            .send(CoordinatorCommand::Spawn(SpawnCommand {
                request: Box::new(request("mid-flight", "explore")),
                admission_tx,
                result_tx,
            }))
            .expect("the actor owns the receiver");
        // One yield is enough: `handle_spawn` inserts into `pending` and
        // calls `record_spawn` synchronously, within the same command-branch
        // arm of the actor's select loop — no `.await` in that path waits on
        // anything `HangingRunner` controls, so the row is written the first
        // time the actor task is polled after the send.
        tokio::task::yield_now().await;
        assert_eq!(
            admission_rx.await,
            Ok(SpawnAdmission::Admitted),
            "this child must be admitted — the mid-flight teardown below is only \
             meaningful for a child that was actually accepted and started"
        );

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

    // ── The configured cap is the cap in force ──────────────────────────
    //
    // These two are the point of `[subagents] max_concurrent_children`. A key
    // that parses, validates and serialises while the code it names reads a
    // compiled-in constant is indistinguishable from a working key right up
    // until an operator relies on it, so both tests observe the limit the way
    // a caller does — by being refused — rather than by reading the field
    // back out of a struct.
    //
    // `HangingRunner` is what makes that observable: its children never leave
    // `pending`, so the in-flight population is exactly the number of spawns
    // admitted so far, with no timing race.

    /// Send one spawn and wait for the actor's admission decision.
    ///
    /// Timeout-bounded on purpose: an admitted background child answers its
    /// *result* channel only when it really finishes, and `HangingRunner`'s
    /// children never do. An unbounded await on the wrong channel would wedge
    /// the test binary instead of failing it. `result_rx` is handed back so
    /// the caller can keep it alive for as long as the child is meant to
    /// count against the cap.
    async fn admit(
        commands: &mpsc::UnboundedSender<CoordinatorCommand>,
        child_id: &str,
    ) -> (SpawnAdmission, oneshot::Receiver<zeroclaw_coordinator::ChildResult>) {
        let (admission_tx, admission_rx) = oneshot::channel();
        let (result_tx, result_rx) = oneshot::channel();
        commands
            .send(CoordinatorCommand::Spawn(SpawnCommand {
                request: Box::new(request(child_id, "explore")),
                admission_tx,
                result_tx,
            }))
            .expect("the actor owns the receiver");
        let admission = tokio::time::timeout(std::time::Duration::from_secs(5), admission_rx)
            .await
            .unwrap_or_else(|_| panic!("the actor must decide on '{child_id}' promptly"))
            .expect("every spawn is answered on the admission channel");
        (admission, result_rx)
    }

    /// Set `[subagents] max_concurrent_children = 2` and prove **2** is the
    /// number the booted actor enforces — not `CoordinatorConfig`'s
    /// compiled-in default, which is 6 and would admit both of the spawns
    /// this test requires to be refused.
    ///
    /// Two chosen deliberately: it is below the default from either direction,
    /// so neither "the wiring was skipped" nor "the default happens to match"
    /// can pass this.
    #[tokio::test]
    async fn configured_concurrency_cap_is_the_one_the_booted_actor_enforces() {
        let dir = tempfile::tempdir().unwrap();
        let handle = ControlPlaneHandle::start(dir.path()).await.unwrap();
        let mut config = Config::default();
        config.subagents.max_concurrent_children = 2;

        let host = start_with_runner(
            &config,
            HangingRunner,
            Arc::clone(&handle.sqlite_store),
            handle.boot_id.clone(),
        );

        let (first, _first_rx) = admit(&host.commands.0, "kid-1").await;
        assert_eq!(first, SpawnAdmission::Admitted);
        // The request that brings the registry to exactly the cap is still
        // admitted: the gate refuses the one *past* the limit, not the one
        // that reaches it.
        let (second, _second_rx) = admit(&host.commands.0, "kid-2").await;
        assert_eq!(
            second,
            SpawnAdmission::Admitted,
            "the spawn that fills the last slot must be admitted"
        );

        let (third, _third_rx) = admit(&host.commands.0, "kid-3").await;
        assert_eq!(
            third,
            SpawnAdmission::Refused(zeroclaw_coordinator::SpawnRefusal::ChildCapacityReached {
                in_flight: 2,
                max: 2,
            }),
            "the configured limit of 2 must be the one enforced — an admission here means \
             the boot path is reading a constant and `[subagents]` is an inert key"
        );

        host.actor.abort();
        let _ = host.actor.await;
    }

    /// With no `[subagents]` section configured at all, the booted actor caps
    /// at **6**: six admitted, the seventh refused.
    ///
    /// 6 is an operating limit. The 128 it replaced was
    /// `DelegateTool::MAX_CONCURRENT_BACKGROUND_DELEGATIONS`, a runaway
    /// backstop meaning "if we are here, something is broken", copied into the
    /// slot where a limit belongs — under it, this test's seventh child (and
    /// its hundred-and-twenty-first) would be admitted.
    #[tokio::test]
    async fn absent_subagent_config_boots_the_actor_at_six() {
        let dir = tempfile::tempdir().unwrap();
        let handle = ControlPlaneHandle::start(dir.path()).await.unwrap();
        // Nothing sets `subagents`: this is the config an operator who never
        // heard of the section gets.
        let config = Config::default();

        let host = start_with_runner(
            &config,
            HangingRunner,
            Arc::clone(&handle.sqlite_store),
            handle.boot_id.clone(),
        );

        let mut held = Vec::new();
        for n in 1..=6 {
            let (admission, result_rx) = admit(&host.commands.0, &format!("kid-{n}")).await;
            assert_eq!(
                admission,
                SpawnAdmission::Admitted,
                "child {n} of 6 must be admitted under the default cap"
            );
            held.push(result_rx);
        }

        let (seventh, _seventh_rx) = admit(&host.commands.0, "kid-7").await;
        assert_eq!(
            seventh,
            SpawnAdmission::Refused(zeroclaw_coordinator::SpawnRefusal::ChildCapacityReached {
                in_flight: 6,
                max: 6,
            }),
            "the default must be 6, and the refusal must name what is running as well as the limit"
        );

        host.actor.abort();
        let _ = host.actor.await;
    }
}

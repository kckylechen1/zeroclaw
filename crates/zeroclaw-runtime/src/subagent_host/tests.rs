//! Tests for the native `ChildRunner`.
//!
//! Two harnesses, on purpose:
//!
//! - Anything involving [`ChildReporter`](zeroclaw_coordinator::ChildReporter)
//!   goes through a **real** [`Coordinator`]. That type's fields are
//!   `pub(crate)` in `zeroclaw-coordinator` and it has no public constructor,
//!   so a fake reporter is not merely undesirable here — it is impossible.
//!   The upside is that the promote handshake under test is the production
//!   one (`coordinator.rs:399-424`), not a re-statement of it.
//! - Everything that takes no reporter (`validate_type`, `describe_type`,
//!   [`child_context`], [`race_cancellation`]) is called directly.
//!
//! No test here reaches a model provider. The run-path tests drive a real
//! turn until it fails on config resolution, which is the last point before
//! the first HTTP call; assertions past that point would need a live provider
//! and are called out in the delivery report rather than faked.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use zeroclaw_config::card::{AgentCard, CardGrants, GrantClass, ToolGrant};
use zeroclaw_config::schema::{AliasedAgentConfig, Config, RiskProfileConfig};
use zeroclaw_coordinator::{
    CancelToken, ChildOutcome, ChildOverrides, ChildRequest, ChildResult, ChildRunner, Coordinator,
    CoordinatorCommand, CoordinatorConfig, DescribeOutcome, SpawnAdmission, SpawnCommand,
    ValidateTypeOutcome,
};

use super::{NativeChildRunner, TurnEnding, child_context, race_cancellation};

// ── Fixtures ─────────────────────────────────────────────────────────────

/// A config rooted in `root` so nothing under `$HOME/.zeroclaw` is touched:
/// `SecurityPolicy::for_agent` creates the agent workspace eagerly
/// (`zeroclaw-config/src/policy.rs:2225`) and the memory backend writes under
/// `data_dir`.
#[allow(clippy::field_reassign_with_default)] // Config has ~100 fields; partial override via Default is cleaner
fn base_config(root: &Path) -> Config {
    let mut config = Config::default();
    config.data_dir = root.join("data");
    config.config_path = root.join("config.toml");
    config
        .risk_profiles
        .insert("default".to_string(), RiskProfileConfig::default());
    config
}

/// One agent with a resolvable risk profile and **no** model provider, so a
/// real turn gets as far as provider resolution and then fails there
/// (`agent/loop_.rs:1394-1411`) without any network.
fn config_with_agent(root: &Path, alias: &str) -> Config {
    let mut config = base_config(root);
    config.agents.insert(
        alias.to_string(),
        AliasedAgentConfig {
            risk_profile: "default".into(),
            ..AliasedAgentConfig::default()
        },
    );
    config
}

/// A parent with an unrestricted profile and a **carded** child whose card
/// grants exactly one tool.
///
/// The two agents' policies differ in an assertable way, which is what makes
/// "the child's policy came from the child's alias" falsifiable: resolving
/// from `"parent"` would produce `allowed_tools: None`.
fn config_with_carded_child(root: &Path) -> Config {
    let mut config = base_config(root);
    config.agents.insert(
        "parent".to_string(),
        AliasedAgentConfig {
            risk_profile: "default".into(),
            ..AliasedAgentConfig::default()
        },
    );
    config.cards.insert(
        "reader".to_string(),
        AgentCard {
            risk_profile: "default".into(),
            grants: CardGrants {
                tools: vec![ToolGrant::new("file_read", GrantClass::LocalRead)],
                ..CardGrants::default()
            },
            ..AgentCard::default()
        },
    );
    config.agents.insert(
        "child".to_string(),
        AliasedAgentConfig {
            // Empty by construction for a carded agent: setting both is
            // refused at validation (`AliasedAgentConfig::card`'s doc).
            card: "reader".into(),
            ..AliasedAgentConfig::default()
        },
    );
    config
}

/// The one place a `ChildRequest` is built in this module, so a new field on
/// that struct is a single edit here.
fn request(child_id: &str, agent_type: &str) -> ChildRequest {
    ChildRequest {
        child_id: child_id.into(),
        prompt: "summarise the seam".into(),
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
        // No foreground budget: the reply this harness reads must be the real
        // ending, never a 45s background hand-off.
        await_to_completion: true,
        fork_context: false,
        cancel_token: CancelToken::new(),
    }
}

/// Drive one spawn through a real coordinator and return the caller's reply.
///
/// The command sender is dropped straight after the send so
/// `Coordinator::run` terminates once the child settles
/// (`coordinator.rs:131-140`).
async fn spawn_through_coordinator(config: Config, request: ChildRequest) -> ChildResult {
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let coordinator = Coordinator::new(
        command_rx,
        NativeChildRunner::new(Arc::new(config)),
        CoordinatorConfig::default(),
    );
    let (admission_tx, admission_rx) = oneshot::channel();
    let (result_tx, result_rx) = oneshot::channel();
    command_tx
        .send(CoordinatorCommand::Spawn(SpawnCommand {
            request: Box::new(request),
            admission_tx,
            result_tx,
        }))
        .expect("the coordinator owns the receiver");
    drop(command_tx);

    coordinator.run().await;
    assert_eq!(
        admission_rx.await,
        Ok(SpawnAdmission::Admitted),
        "these cases all reach the runner, so admission must be granted — a refusal here \
         would mean the child never ran and the result below is not about this request"
    );
    result_rx
        .await
        .expect("the coordinator replies to every spawn caller")
}

// ── validate_type ────────────────────────────────────────────────────────

#[tokio::test]
async fn validate_type_accepts_a_configured_alias() {
    let root = tempfile::tempdir().unwrap();
    let runner = NativeChildRunner::new(Arc::new(config_with_agent(root.path(), "alpha")));
    let outcome = runner
        .validate_type("alpha".into(), "parent-session".into())
        .await;
    assert!(
        matches!(outcome, ValidateTypeOutcome::Ok),
        "a configured, enabled alias must validate, got: {outcome:?}"
    );
}

#[tokio::test]
async fn validate_type_rejects_an_unknown_alias_without_panicking() {
    let root = tempfile::tempdir().unwrap();
    let mut config = config_with_agent(root.path(), "beta");
    config.agents.insert(
        "alpha".to_string(),
        AliasedAgentConfig {
            risk_profile: "default".into(),
            ..AliasedAgentConfig::default()
        },
    );
    let runner = NativeChildRunner::new(Arc::new(config));

    let outcome = runner
        .validate_type("nope".into(), "parent-session".into())
        .await;
    let ValidateTypeOutcome::Unknown { available } = outcome else {
        panic!("an unknown alias must be a validation failure, got: {outcome:?}");
    };
    assert_eq!(
        available,
        vec!["alpha".to_string(), "beta".to_string()],
        "`available` is documented as sorted"
    );
}

#[tokio::test]
async fn validate_type_reports_a_disabled_agent_as_disabled_not_unknown() {
    let root = tempfile::tempdir().unwrap();
    let mut config = config_with_agent(root.path(), "alpha");
    config
        .agents
        .get_mut("alpha")
        .expect("fixture inserts alpha")
        .enabled = false;
    let runner = NativeChildRunner::new(Arc::new(config));

    let outcome = runner
        .validate_type("alpha".into(), "parent-session".into())
        .await;
    assert!(
        matches!(outcome, ValidateTypeOutcome::Disabled),
        "a configured-but-disabled alias is Disabled, not Unknown, got: {outcome:?}"
    );
}

// ── describe_type / capability discipline ────────────────────────────────

/// The child's reach is the child's card's reach.
///
/// `describe_type` is the public surface of the same policy resolution
/// [`NativeChildRunner::run`] performs, so this pins card-awareness through
/// the trait rather than through an internal helper.
#[tokio::test]
async fn describe_type_reports_the_childs_own_card_not_the_parents_profile() {
    let root = tempfile::tempdir().unwrap();
    let runner = NativeChildRunner::new(Arc::new(config_with_carded_child(root.path())));

    let DescribeOutcome::Ok(child) = runner
        .describe_type("child".into(), None, "parent-session".into())
        .await
    else {
        panic!("a configured carded agent must describe");
    };
    assert_eq!(
        child.tool_names.get("read").map(String::as_str),
        Some("file_read"),
        "the card grants file_read"
    );
    assert!(child.can_read, "card grants a read tool");
    assert!(
        !child.can_execute,
        "the card grants no shell; a carded agent's grants are the whole world"
    );
    assert!(!child.can_search, "the card grants no search tool");

    let DescribeOutcome::Ok(parent) = runner
        .describe_type("parent".into(), None, "parent-session".into())
        .await
    else {
        panic!("a configured profile-governed agent must describe");
    };
    assert!(
        parent.can_execute,
        "test precondition: the parent's profile is unrestricted, so describing the child \
         with the parent's alias would have reported can_execute = true"
    );
}

#[tokio::test]
async fn describe_type_rejects_an_unknown_alias() {
    let root = tempfile::tempdir().unwrap();
    let runner = NativeChildRunner::new(Arc::new(config_with_agent(root.path(), "alpha")));
    let outcome = runner
        .describe_type("nope".into(), None, "parent-session".into())
        .await;
    let DescribeOutcome::Unknown { available } = outcome else {
        panic!("an unknown alias must not describe, got: {outcome:?}");
    };
    assert_eq!(available, vec!["alpha".to_string()]);
}

/// The resolver the run path uses, asserted directly: the policy is built
/// from the child's alias, and it is the card-aware
/// `SecurityPolicy::for_agent` that builds it.
#[test]
fn child_policy_is_resolved_from_the_childs_own_alias() {
    let root = tempfile::tempdir().unwrap();
    let config = config_with_carded_child(root.path());

    let child = child_context(&config, "child").expect("carded child resolves");
    assert_eq!(
        child.policy.allowed_tools,
        Some(vec!["file_read".to_string()]),
        "a carded agent's allowed_tools is exactly its card's grants"
    );
    assert!(!child.policy.is_tool_allowed("shell"));

    let parent = child_context(&config, "parent").expect("parent resolves");
    assert!(
        parent.policy.allowed_tools.is_none(),
        "test precondition: the parent is unrestricted, so a runner that resolved from the \
         parent's alias would hand the child an unrestricted policy"
    );
}

#[test]
fn child_context_errors_on_an_unknown_alias_instead_of_panicking() {
    let root = tempfile::tempdir().unwrap();
    let config = config_with_agent(root.path(), "alpha");
    let error = child_context(&config, "ghost").expect_err("unknown alias must error");
    assert!(
        error.to_string().contains("ghost"),
        "the failing alias must be named, got: {error}"
    );
}

// ── run ──────────────────────────────────────────────────────────────────

/// A child whose model provider does not resolve fails structurally: a
/// `Failed` result carrying the resolver's message, delivered to the spawn
/// caller. Not a panic (the coordinator's panic guard would have produced
/// `Lost` instead) and not a hang (this test would never return).
#[tokio::test]
async fn run_reports_failed_with_detail_when_the_provider_does_not_resolve() {
    let root = tempfile::tempdir().unwrap();
    let config = config_with_agent(root.path(), "alpha");

    let result = spawn_through_coordinator(config, request("kid", "alpha")).await;

    assert_eq!(result.outcome, ChildOutcome::Failed);
    assert_eq!(result.child_id, "kid");
    let detail = result.detail.as_deref().unwrap_or_default();
    assert!(
        detail.contains("child turn failed") && detail.contains("model_provider"),
        "the turn's own error must survive into the detail, got: {detail:?}"
    );
    assert_eq!(
        result.turns, 1,
        "the turn was entered; a pre-flight refusal would have reported 0"
    );
    assert!(!result.backgrounded);
}

/// An unknown agent type is a failure the runner reports, not a panic and not
/// a turn.
#[tokio::test]
async fn run_refuses_an_unknown_agent_type_before_starting_a_turn() {
    let root = tempfile::tempdir().unwrap();
    let config = config_with_agent(root.path(), "alpha");

    let result = spawn_through_coordinator(config, request("kid", "ghost")).await;

    assert_eq!(result.outcome, ChildOutcome::Failed);
    let detail = result.detail.as_deref().unwrap_or_default();
    assert!(
        detail.contains("ghost") && detail.contains("alpha"),
        "the refusal names the bad type and what was available, got: {detail:?}"
    );
    assert_eq!(
        result.turns, 0,
        "no turn may be entered for an unknown type"
    );
}

/// A request carrying semantics this phase cannot honour is refused by name,
/// rather than run as a different child than the one asked for.
#[tokio::test]
async fn run_refuses_request_fields_it_cannot_honour_and_names_them() {
    let root = tempfile::tempdir().unwrap();
    let config = config_with_agent(root.path(), "alpha");
    let mut req = request("kid", "alpha");
    req.cwd = Some("/somewhere/else".into());
    req.fork_context = true;

    let result = spawn_through_coordinator(config, req).await;

    assert_eq!(result.outcome, ChildOutcome::Failed);
    let detail = result.detail.as_deref().unwrap_or_default();
    assert!(
        detail.contains("cwd") && detail.contains("fork_context"),
        "every unhonourable field must be named, got: {detail:?}"
    );
    assert_eq!(result.turns, 0);
}

/// Cancel-at-promote: the token is already cancelled when the child is
/// spawned, so the coordinator refuses the promotion
/// (`coordinator.rs:409-417`) and the runner must tear down instead of
/// running the turn.
///
/// A runner that ignored the `false` acknowledgement would still resolve
/// `alpha` and run its turn, and this child would come back `Failed` on the
/// unresolved provider with `turns == 1` — which is exactly what the two
/// assertions below rule out.
#[tokio::test]
async fn cancel_at_promote_reports_cancelled_and_never_runs_the_turn() {
    let root = tempfile::tempdir().unwrap();
    let config = config_with_agent(root.path(), "alpha");
    let req = request("kid", "alpha");
    req.cancel_token.cancel();

    let result = spawn_through_coordinator(config, req).await;

    assert_eq!(result.outcome, ChildOutcome::Cancelled);
    assert!(
        result
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("promotion refused"),
        "the detail must say the promote handshake refused, got: {:?}",
        result.detail
    );
    assert_eq!(
        result.turns, 0,
        "a refused promotion must not have run a turn"
    );
    assert!(
        !result.child_session_id.is_empty(),
        "the session id minted before promotion is still reported"
    );
}

// ── cancellation race ────────────────────────────────────────────────────

#[tokio::test]
async fn race_cancellation_drops_a_pending_turn_when_the_token_fires() {
    let cancellation = CancelToken::new();
    cancellation.cancel();

    let ending = race_cancellation(
        &cancellation,
        std::future::pending::<anyhow::Result<String>>(),
    )
    .await;

    assert!(
        matches!(ending, TurnEnding::Cancelled),
        "a pending turn under a cancelled token must be dropped, got: {ending:?}"
    );
}

/// The tie-break: a turn that is already ready wins over a token that is
/// already cancelled. Reversing the two `select!` arms turns this red, and
/// would mean a child that produced an answer reports `Cancelled` and has its
/// answer discarded.
#[tokio::test]
async fn race_cancellation_prefers_a_finished_turn_over_a_cancelled_token() {
    let cancellation = CancelToken::new();
    cancellation.cancel();

    let ending = race_cancellation(&cancellation, std::future::ready(Ok("done".to_string()))).await;

    let TurnEnding::Finished(Ok(text)) = ending else {
        panic!("a ready turn must not be thrown away by a racing cancel, got: {ending:?}");
    };
    assert_eq!(text, "done");
}

#[tokio::test]
async fn race_cancellation_returns_the_turns_error_verbatim() {
    let cancellation = CancelToken::new();

    let ending = race_cancellation(
        &cancellation,
        std::future::ready(Err(anyhow::Error::msg("provider exploded"))),
    )
    .await;

    let TurnEnding::Finished(Err(error)) = ending else {
        panic!("an erroring turn is Finished(Err), not Cancelled, got: {ending:?}");
    };
    assert!(error.to_string().contains("provider exploded"));
}

// ── unsupported-field inventory ──────────────────────────────────────────

#[test]
fn a_plain_request_has_nothing_the_runner_cannot_honour() {
    assert!(
        super::unsupported_request_fields(&request("kid", "alpha")).is_empty(),
        "the ordinary shape must not be refused"
    );
}

#[test]
fn every_unhonourable_field_is_reported_individually() {
    let with = |mutate: fn(&mut ChildRequest)| {
        let mut req = request("kid", "alpha");
        mutate(&mut req);
        super::unsupported_request_fields(&req)
    };

    assert_eq!(
        with(|r| r.resume_from = Some("prior".into())),
        vec!["resume_from"]
    );
    assert_eq!(with(|r| r.fork_context = true), vec!["fork_context"]);
    assert_eq!(with(|r| r.cwd = Some("/elsewhere".into())), vec!["cwd"]);
    assert_eq!(
        with(|r| r.overrides.persona = Some("bard".into())),
        vec!["overrides.persona"]
    );
    assert_eq!(
        with(|r| r.overrides.reasoning_effort = Some("high".into())),
        vec!["overrides.reasoning_effort"]
    );
    assert_eq!(
        with(|r| r.overrides.output_token_budget = Some(10)),
        vec!["overrides.output_token_budget"]
    );

    // Coordinator-owned knobs are not the runner's to refuse.
    assert!(with(|r| r.overrides.completion_output_cap = Some(64)).is_empty());
    assert!(with(|r| r.overrides.spawn_depth = Some(1)).is_empty());
    assert!(with(|r| r.overrides.loop_task_id = Some("unit".into())).is_empty());
    assert!(with(|r| r.overrides.model = Some("gpt-x".into())).is_empty());
}

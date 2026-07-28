//! Wiring-phase-2b implementation of [`zeroclaw_coordinator::ChildRunner`]:
//! a child runs as a ZeroClaw-native agent turn, in this process.
//!
//! `ChildRunner` is the coordinator's only host-specific seam (see that
//! crate's `state` module doc). Everything in here is translation: a
//! [`ChildRunRequest`] becomes one call to [`crate::agent::run`] — the same
//! entry `tools/spawn_subagent.rs` uses — and the turn's `Result<String>`
//! becomes a [`ChildResult`].
//!
//! ## Which execution entry, and why that one
//!
//! [`crate::agent::run`] (`agent/loop_.rs:1141`, re-exported at
//! `agent/mod.rs:68`) is the whole-turn entry: it resolves the agent config,
//! builds the tool registry through the one gated seam
//! (`ScopedToolRegistry::assemble`), builds memory for the alias, resolves the
//! model provider, and drives `run_tool_call_loop`. `spawn_subagent.rs:293`
//! calls exactly this. Wrapping it — rather than `run_tool_call_loop`
//! directly, the way `delegate.rs:2731` does — is what keeps a coordinator
//! child identical to a `spawn_subagent` child: same registry assembly, same
//! prompt build, same memory scope. `delegate.rs`'s path is deliberately
//! *not* reused: it starts from a caller's already-built registry (bounded
//! mode) or a target-specific assembly (independent mode) and is welded to
//! `DelegateTool`'s own fields (`parent_tools`, `workspace_dir`,
//! `multimodal_config`), none of which a coordinator child has.
//!
//! `agent::run` is `pub` and takes no tool-loop state, so nothing had to be
//! opened up for this module. What it does *not* offer is listed under
//! "Seams this phase did not open" below; those are the reason several
//! request fields are refused rather than silently dropped.
//!
//! ## Capability discipline
//!
//! A child runs under the **target** agent's own policy, resolved by
//! [`SecurityPolicy::for_agent`] via [`SubAgentSpawn::for_agent`]
//! (`subagent/mod.rs:46`) — never the parent's, never a hand-built one. That
//! resolver is card-aware (`zeroclaw-config/src/policy.rs:2250-2279`: a card
//! replaces `allowed_tools` wholesale and closes the MCP auto-admit escape
//! hatch), so a carded child gets exactly its card's grants. The resolved
//! policy is handed to `agent::run` through `AgentRunOverrides::security`;
//! passing `None` there would make `agent::run` resolve the same policy for
//! the same alias (`agent/loop_.rs:1233-1236`), so this is explicit rather
//! than load-bearing — but explicit is what makes it *testable*, and what
//! makes an accidental switch to the parent's alias a red test instead of a
//! silent privilege change.
//!
//! ## Seams this phase did not open
//!
//! - **Cancellation.** `agent::run` takes no cancellation token; the turn
//!   engine it drives is handed `cancellation_token: None`
//!   (`agent/loop_.rs:1933`). So cancellation here is *dropping the turn
//!   future* at its next await point ([`race_cancellation`]). Work the turn
//!   spawned onto the runtime with `tokio::spawn` is not dropped with it.
//! - **Live progress.** `agent::run` returns `Result<String>` and reports no
//!   intermediate state, so [`NativeChildControl::progress`] can only report
//!   the turn count. Tool-call counts, context usage and per-tool names need
//!   a progress sink threaded into `run_tool_call_loop`.
//! - **Completion delivery.** [`NativeChildRunner::on_completed`] logs and
//!   returns. Waking the parent session on a child's ending is the next
//!   phase; nothing here buffers, routes, or notifies.
//! - **Persisted output.** [`ChildRunner::persisted_output_ref`] and
//!   `load_persisted_output` keep their defaults (`None`). A native child
//!   returns its whole output in-band on [`ChildResult::output`], so there is
//!   no out-of-line blob to reference; the coordinator only blanks
//!   `result.output` when a reference exists (`coordinator.rs:567-569`), and
//!   blanking it with nothing to load in its place would lose the answer.

use std::future::{Future, Ready, ready};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use anyhow::Result;
use zeroclaw_config::policy::SecurityPolicy;
use zeroclaw_config::schema::Config;
use zeroclaw_coordinator::{
    CancelToken, ChildCompletion, ChildControl, ChildOutcome, ChildProgress, ChildRequest,
    ChildResult, ChildRunOutput, ChildRunRequest, ChildRunner, ChildTypeSummary, DescribeOutcome,
    SendBoxFuture, StartedChild, ValidateTypeOutcome,
};
use zeroclaw_log::scope;

use crate::agent::cost::{TOOL_LOOP_TURN_USAGE, TurnUsage};
use crate::agent::loop_::AgentRunOverrides;
use crate::subagent::{SubAgentContext, SubAgentOverrides, SubAgentSpawn};

/// Tool kinds a described child is reported against, mapped to the tool name
/// this host actually dispatches.
///
/// `ChildTypeSummary::tool_names` is keyed by kind and valued by the host's
/// own spelling (`zeroclaw-coordinator/src/types.rs:584`, pinned by that
/// crate's `backend_tests.rs:401`). This is a fixed table rather than a walk
/// of the assembled registry: assembling one costs a memory backend, a
/// platform runtime and an MCP connect pass (`agent/loop_.rs:1305-1345`),
/// none of which a description may pay for. The consequence is stated in
/// [`NativeChildRunner::describe_type`]'s doc: this reports the *admission*
/// half of resolution faithfully and says nothing about which tools were
/// built at all.
pub const CHILD_TOOL_KINDS: &[(&str, &str)] = &[
    ("read", "file_read"),
    ("write", "file_write"),
    ("edit", "file_edit"),
    ("search", "content_search"),
    ("glob", "glob_search"),
    ("execute", "shell"),
];

/// Resolve the child's own identity and policy from its alias.
///
/// This is the single place the child's capability envelope comes from, so
/// "the child runs under the child's policy" is one assertion away rather
/// than spread over the run path. [`SubAgentSpawn::for_agent`] checks the
/// alias first (so an unknown one fails with the alias in the message) and
/// then calls [`SecurityPolicy::for_agent`] on that same alias.
///
/// # Errors
///
/// Returns the resolver's error when the alias is not configured or its
/// risk profile / card does not resolve to a usable policy.
pub fn child_context(config: &Config, child_alias: &str) -> Result<SubAgentContext> {
    SubAgentSpawn::for_agent(config, child_alias)?.build(SubAgentOverrides::default())
}

/// Request fields this phase cannot honour, named so a refusal can say which.
///
/// Every entry is something [`crate::agent::run`] has no parameter for, and
/// dropping it silently would run a child that is not the child the caller
/// asked for:
///
/// - `resume_from` / `fork_context` seed the child's conversation. `agent::run`
///   starts from a fresh history built in `agent/loop_.rs:1849-1852`.
/// - `cwd` is documented as "validated by the runner". Honouring it means
///   moving `SecurityPolicy::workspace_dir`, which is the child's file-tool
///   jail root and the shell tool's spawn cwd
///   (`zeroclaw-config/src/policy.rs:2217-2231`) — a capability change, not a
///   convenience, and it needs a policy-scoped validation this phase does not
///   have.
/// - `overrides.persona` would have to override `Config::persona_for_agent`,
///   which `agent::run` reads directly (`agent/loop_.rs:1169-1171`).
/// - `overrides.reasoning_effort` has no parameter; the turn engine takes
///   thinking level from a directive parsed out of the message
///   (`agent/loop_.rs:1705`).
/// - `overrides.output_token_budget` has no parameter and no enforcement point
///   on this path.
///
/// Fields deliberately absent from this list are ones the *coordinator* owns,
/// not the runner: `completion_output_cap` (applied in
/// `coordinator.rs:545-548`), `loop_task_id`, `run_in_background`,
/// `await_to_completion`, `surface_completion`. `overrides.spawn_depth` is
/// also absent: depth is capped structurally instead, by running the child
/// with `AgentRunOverrides::is_subagent = true`, which makes the child's own
/// `spawn_subagent` tool refuse to recurse (`tools/spawn_subagent.rs:93-102`).
#[must_use]
pub fn unsupported_request_fields(request: &ChildRequest) -> Vec<&'static str> {
    let mut unsupported = Vec::new();
    if request.resume_from.is_some() {
        unsupported.push("resume_from");
    }
    if request.fork_context {
        unsupported.push("fork_context");
    }
    if request.cwd.is_some() {
        unsupported.push("cwd");
    }
    if request.overrides.persona.is_some() {
        unsupported.push("overrides.persona");
    }
    if request.overrides.reasoning_effort.is_some() {
        unsupported.push("overrides.reasoning_effort");
    }
    if request.overrides.output_token_budget.is_some() {
        unsupported.push("overrides.output_token_budget");
    }
    unsupported
}

/// How one native turn ended, before it is mapped onto a [`ChildOutcome`].
#[derive(Debug)]
pub enum TurnEnding {
    /// The turn ran to its own end — successfully or not.
    Finished(Result<String>),
    /// The turn was dropped because the child's token was cancelled.
    Cancelled,
}

/// Run `turn` until it ends, or drop it when `cancellation` fires.
///
/// The turn arm is polled first (`biased`), so a turn that is *already* ready
/// wins a tie with a cancellation arriving in the same poll: cancellation is
/// documented as "stop when you can" (`zeroclaw-coordinator/src/cancel.rs:9-12`)
/// and throwing away an answer that already exists is not that. Cancellation
/// is taken only while the turn is still pending — which is also the only
/// point at which dropping it is cheap.
pub async fn race_cancellation<F>(cancellation: &CancelToken, turn: F) -> TurnEnding
where
    F: Future<Output = Result<String>>,
{
    tokio::select! {
        biased;
        result = turn => TurnEnding::Finished(result),
        () = cancellation.cancelled() => TurnEnding::Cancelled,
    }
}

/// Live counters for one native child.
///
/// Only `turns` is real: it goes to 1 when the turn starts, because
/// `agent::run` with a message runs exactly one turn
/// (`agent/loop_.rs:1704` — one `if let Some(msg)` body, no turn loop). Every
/// other counter needs a sink inside `run_tool_call_loop`; see the module doc.
#[derive(Debug, Default)]
struct NativeProgress {
    turns: AtomicU32,
}

/// The coordinator's live handle on a running native child.
///
/// `cancel` holds the child's own token — the same one the coordinator
/// already cancels alongside this call (`coordinator.rs:650-651`), so the two
/// paths converge on one cooperative signal instead of two.
pub struct NativeChildControl {
    cancel: CancelToken,
    progress: Arc<NativeProgress>,
}

impl ChildControl for NativeChildControl {
    type ProgressFuture = Ready<ChildProgress>;

    fn progress(&self) -> Self::ProgressFuture {
        ready(ChildProgress {
            turn_count: self.progress.turns.load(Ordering::Relaxed),
            ..ChildProgress::default()
        })
    }

    fn cancel(&self) {
        self.cancel.cancel();
    }
}

/// Runs coordinator children as native in-process agent turns.
pub struct NativeChildRunner {
    config: Arc<Config>,
}

impl NativeChildRunner {
    #[must_use]
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }

    /// Membership half of both `validate_type` and `describe_type`: `Ok(())`
    /// when the alias names an enabled agent, otherwise the outcome shape both
    /// enums spell the same way.
    fn resolve_agent_type(config: &Config, agent_type: &str) -> Result<(), TypeLookupFailure> {
        match config.agents.get(agent_type) {
            Some(agent) if agent.enabled => Ok(()),
            Some(_) => Err(TypeLookupFailure::Disabled),
            None => Err(TypeLookupFailure::Unknown {
                available: available_agent_types(config),
            }),
        }
    }
}

/// Why an agent type did not resolve, shared by the two lookup methods so
/// they cannot drift apart on what "unknown" means.
enum TypeLookupFailure {
    Unknown { available: Vec<String> },
    Disabled,
}

/// Every enabled alias, sorted — the "what could I have asked for" list both
/// `Unknown` variants carry.
///
/// Disabled aliases are left out on purpose: they are not available, and an
/// operator reading the list would otherwise be told to retry with a name
/// that resolves straight back to `Disabled`.
fn available_agent_types(config: &Config) -> Vec<String> {
    let mut available: Vec<String> = config
        .agents
        .iter()
        .filter(|(_, agent)| agent.enabled)
        .map(|(alias, _)| alias.clone())
        .collect();
    available.sort();
    available
}

fn output(result: ChildResult) -> ChildRunOutput<()> {
    ChildRunOutput {
        result,
        completion_data: (),
        snapshot_ref: None,
    }
}

/// A terminal result for a child that never reached its turn.
///
/// `turns` stays 0 here and is set to 1 only once `agent::run` is actually
/// invoked, so the count distinguishes "refused before running" from "ran and
/// failed".
fn preflight_failure(child_id: &str, detail: String, started_at: Instant) -> ChildRunOutput<()> {
    output(ChildResult {
        outcome: ChildOutcome::Failed,
        detail: Some(detail),
        child_id: child_id.to_owned(),
        duration_ms: elapsed_ms(started_at),
        ..ChildResult::default()
    })
}

fn elapsed_ms(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

impl ChildRunner for NativeChildRunner {
    type Control = NativeChildControl;
    /// Nothing is carried from the run to `on_completed` beyond the
    /// `ChildResult` the coordinator already passes: a native child's output
    /// is in-band, and no snapshot or blob is produced. See the module doc's
    /// note on `persisted_output_ref`.
    type CompletionData = ();
    type RunFuture = SendBoxFuture<ChildRunOutput<()>>;
    type ValidateFuture = Ready<ValidateTypeOutcome>;
    type DescribeFuture = Ready<DescribeOutcome>;

    /// Resolve the child agent, promote it, run one native turn, and report
    /// how it ended.
    ///
    /// Order matters and is fixed by the trait: everything that can fail
    /// *before* a live child exists fails before `started()` is called (the
    /// coordinator then finishes a still-pending child, `coordinator.rs:470`),
    /// and `started()`'s acknowledgement is honoured — a `false` means
    /// cancellation won the promote race and the half-built child is torn
    /// down rather than run.
    fn run(&self, request: ChildRunRequest<Self::Control>) -> Self::RunFuture {
        let config = Arc::clone(&self.config);
        Box::pin(async move {
            let ChildRunRequest {
                request,
                cancellation,
                reporter,
            } = request;
            let started_at = Instant::now();
            let child_id = request.child_id.clone();

            let unsupported = unsupported_request_fields(&request);
            if !unsupported.is_empty() {
                return preflight_failure(
                    &child_id,
                    format!(
                        "native child runner cannot honour {}: refusing rather than running a \
                         child that differs from the one requested",
                        unsupported.join(", ")
                    ),
                    started_at,
                );
            }

            let agent_type = request.agent_type.clone();
            if let Err(failure) = NativeChildRunner::resolve_agent_type(&config, &agent_type) {
                let detail = match failure {
                    TypeLookupFailure::Disabled => {
                        format!("agent type {agent_type:?} is configured but disabled")
                    }
                    TypeLookupFailure::Unknown { available } => format!(
                        "agent type {agent_type:?} is not a configured agent (available: {})",
                        available.join(", ")
                    ),
                };
                return preflight_failure(&child_id, detail, started_at);
            }

            let context = match child_context(&config, &agent_type) {
                Ok(context) => context,
                Err(error) => {
                    return preflight_failure(
                        &child_id,
                        format!("could not resolve agent type {agent_type:?}: {error:#}"),
                        started_at,
                    );
                }
            };

            let run_id = uuid::Uuid::new_v4().to_string();
            let child_session_id = format!("subagent-{run_id}");
            let progress = Arc::new(NativeProgress::default());
            let effective_model_id = config
                .resolved_model_provider_for_agent(&agent_type)
                .and_then(|(_, _, entry)| entry.model.clone())
                .unwrap_or_default();

            let promoted = reporter
                .started(StartedChild {
                    child_session_id: child_session_id.clone(),
                    // Personas are refused up front by
                    // `unsupported_request_fields`, and the agent's own
                    // persona is config-resolved inside the turn — there is
                    // no per-run persona for this to report.
                    persona: None,
                    resumed_from: None,
                    child_cwd: context.policy.workspace_dir.display().to_string(),
                    // Native children share the process's checkout; worktree
                    // isolation is not part of this runner.
                    worktree_path: None,
                    effective_model_id,
                    // No agent definition in this config declares itself
                    // background work, so the request's own
                    // `run_in_background` is the only source of that
                    // property and the coordinator already has it.
                    definition_background: false,
                    control: NativeChildControl {
                        cancel: cancellation.clone(),
                        progress: Arc::clone(&progress),
                    },
                })
                .await;
            if !promoted {
                return output(ChildResult {
                    outcome: ChildOutcome::Cancelled,
                    detail: Some(
                        "cancelled while the child was being built; promotion refused".to_owned(),
                    ),
                    child_id,
                    child_session_id,
                    duration_ms: elapsed_ms(started_at),
                    ..ChildResult::default()
                });
            }

            progress.turns.store(1, Ordering::Relaxed);

            let temperature = config
                .model_provider_for_agent(&agent_type)
                .and_then(|entry| entry.temperature);
            let overrides = AgentRunOverrides {
                // The child's own policy, resolved from the child's own alias.
                security: Some(Arc::clone(&context.policy)),
                memory: None,
                // Caps child depth: the child's `spawn_subagent` refuses to
                // spawn further children.
                is_subagent: true,
                suppress_memory_inject: true,
                memory_free: false,
                // Same reasoning as `spawn_subagent.rs:236-240`: a child run
                // has no cross-turn reuse contract, so the per-call
                // `connect_all` path is the correct one.
                mcp_registry: None,
            };

            // `record_tool_loop_cost_usage` prefers a caller-scoped
            // `TOOL_LOOP_TURN_USAGE` over the turn's own context
            // (`agent/cost.rs:300-318`), which is how the gateway reads a
            // turn's tokens without reaching inside it
            // (`zeroclaw-gateway/src/lib.rs:2483-2499`). Same trick here.
            // It stays zero when no cost tracker is configured, because the
            // turn's cost context is `None` and nothing records at all
            // (`agent/cost.rs:37-43`).
            let usage_cell = Arc::new(parking_lot::Mutex::new(TurnUsage::default()));
            let scoped_alias = agent_type.clone();
            let scoped_session = child_session_id.clone();
            let turn = TOOL_LOOP_TURN_USAGE.scope(
                Some(Arc::clone(&usage_cell)),
                scope!(
                    agent_alias: scoped_alias,
                    session_key: scoped_session,
                    =>
                    crate::agent::run(
                        (*config).clone(),
                        &agent_type,
                        Some(request.prompt.clone()),
                        None,
                        request.overrides.model.clone(),
                        temperature,
                        vec![],
                        false,
                        Some(PathBuf::from(&child_session_id)),
                        None,
                        zeroclaw_api::ingress::TurnOrigin::SubTurn,
                        overrides,
                    )
                ),
            );

            let ending = race_cancellation(&cancellation, Box::pin(turn)).await;
            let usage = *usage_cell.lock();
            let duration_ms = elapsed_ms(started_at);

            let (outcome, text, detail) = match ending {
                TurnEnding::Finished(Ok(response)) => {
                    (ChildOutcome::Completed, response, None)
                }
                TurnEnding::Finished(Err(error)) => (
                    ChildOutcome::Failed,
                    String::new(),
                    Some(format!("child turn failed: {error:#}")),
                ),
                TurnEnding::Cancelled => (
                    ChildOutcome::Cancelled,
                    String::new(),
                    Some("cancelled while the child's turn was running".to_owned()),
                ),
            };

            output(ChildResult {
                outcome,
                output: Arc::from(text.as_str()),
                detail,
                child_id,
                child_session_id,
                // No count is available: the turn engine reports none out of
                // `agent::run`. See the module doc.
                tool_calls: 0,
                turns: 1,
                duration_ms,
                // Distinct meanings, so a reader gets three facts and not one
                // repeated: prompt tokens, completion tokens, and their sum.
                tokens_used: usage.input_tokens,
                output_tokens_used: usage.output_tokens,
                total_tokens_used: usage.input_tokens.saturating_add(usage.output_tokens),
                worktree_path: None,
                // Backgrounding is the coordinator's decision, never the
                // runner's: this reply is always a real ending.
                backgrounded: false,
            })
        })
    }

    /// Does this agent type resolve, against `config.agents`?
    ///
    /// Membership and `enabled` only. Two variants are deliberately never
    /// returned:
    ///
    /// - `NotAllowed` would need the *parent's* alias to ask whether it may
    ///   reach this target. `ChildRequest` now carries one
    ///   (`parent_alias`, `zeroclaw-coordinator/src/types.rs:45`) but this
    ///   method's signature does not: it gets `parent_session_id`, which that
    ///   same field's doc explicitly distinguishes from an alias. Guessing an
    ///   alias out of a session id would gate real spawns on a fabricated
    ///   identity, so parent-to-child admission belongs to the phase that
    ///   changes this signature — not to a runner improvising it.
    /// - `ValidationUnavailable` means "could not be checked, the type may be
    ///   fine". A configured alias whose risk profile does not resolve is not
    ///   fine, and it is not `Unknown` either — the enum has no variant for
    ///   it, so this method stays out of policy resolution entirely and
    ///   [`Self::run`] reports that failure with the resolver's own message.
    fn validate_type(&self, agent_type: String, _parent_session_id: String) -> Self::ValidateFuture {
        ready(
            match NativeChildRunner::resolve_agent_type(&self.config, &agent_type) {
                Ok(()) => ValidateTypeOutcome::Ok,
                Err(TypeLookupFailure::Disabled) => ValidateTypeOutcome::Disabled,
                Err(TypeLookupFailure::Unknown { available }) => {
                    ValidateTypeOutcome::Unknown { available }
                }
            },
        )
    }

    /// What a child of this type would be allowed to call.
    ///
    /// The reported tools are [`CHILD_TOOL_KINDS`] filtered through the
    /// child's own [`SecurityPolicy`] — the same `is_tool_allowed` gate the
    /// dispatch site applies, fed by the same card-aware
    /// [`SecurityPolicy::for_agent`] a real spawn uses. What it does *not*
    /// cover is registry construction: a tool this reports may still be
    /// absent because its feature is off or its MCP server never connected.
    /// A description is therefore an upper bound on reach, which is the safe
    /// direction for a caller deciding whether to delegate.
    ///
    /// `harness_agent_type` is ignored: it selects a harness flavour, and
    /// this host has exactly one.
    fn describe_type(
        &self,
        agent_type: String,
        _harness_agent_type: Option<String>,
        _parent_session_id: String,
    ) -> Self::DescribeFuture {
        ready(
            match NativeChildRunner::resolve_agent_type(&self.config, &agent_type) {
                Err(TypeLookupFailure::Disabled) => DescribeOutcome::Disabled,
                Err(TypeLookupFailure::Unknown { available }) => {
                    DescribeOutcome::Unknown { available }
                }
                Ok(()) => match SecurityPolicy::for_agent(&self.config, &agent_type) {
                    // Fail-open, as the variant's own doc directs: the agent
                    // exists, only its policy could not be built.
                    Err(_) => DescribeOutcome::Unavailable,
                    Ok(policy) => DescribeOutcome::Ok(ChildTypeSummary {
                        tool_names: CHILD_TOOL_KINDS
                            .iter()
                            .filter(|(_, tool)| policy.is_tool_allowed(tool))
                            .map(|(kind, tool)| ((*kind).to_owned(), (*tool).to_owned()))
                            .collect(),
                        can_read: policy.is_tool_allowed("file_read"),
                        can_search: policy.is_tool_allowed("content_search")
                            || policy.is_tool_allowed("glob_search"),
                        can_execute: policy.is_tool_allowed("shell"),
                    }),
                },
            },
        )
    }

    /// Record the ending. Delivery is the next phase's job.
    ///
    /// The coordinator has already committed state and decided who was told
    /// (`CompletionDisposition`) by the time this runs, so nothing here may
    /// change the outcome — it exists so an ending is observable before the
    /// waker that will route it lands. `should_surface` is logged rather than
    /// acted on precisely because acting on it is that phase's contract.
    fn on_completed(&self, completion: ChildCompletion<Self::CompletionData>) {
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Complete)
                .with_category(::zeroclaw_log::EventCategory::Agent)
                .with_duration(completion.result.duration_ms)
                .with_attrs(::serde_json::json!({
                    "child_id": completion.request.child_id,
                    "agent_type": completion.request.agent_type,
                    "parent_alias": completion.request.parent_alias,
                    "parent_session_id": completion.request.parent_session_id,
                    "outcome": format!("{:?}", completion.result.outcome),
                    "detail": completion.result.detail,
                    "turns": completion.result.turns,
                    "total_tokens_used": completion.result.total_tokens_used,
                    "foreground_delivered": completion.disposition.foreground_delivered,
                    "backgrounded": completion.disposition.backgrounded,
                    "waiter_delivered": completion.disposition.waiter_delivered,
                    "explicitly_killed": completion.disposition.explicitly_killed,
                    "should_surface": completion.disposition.should_surface,
                })),
            "coordinator child finished"
        );
    }
}

#[cfg(test)]
mod tests;

//! Agent-loop tool that spawns an ephemeral SubAgent inheriting the
//! parent's identity, security policy, and memory allowlist, runs a
//! focused prompt, and returns the response. Cron's `JobType::Agent`
//! dispatch is the other SubAgent spawn site; both funnel through

use crate::agent::loop_::AgentRunOverrides;
use crate::security::SecurityPolicy;
use crate::security::policy::ToolOperation;
use crate::subagent::{SubAgentOverrides, SubAgentSpawn};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_config::schema::Config;
use zeroclaw_coordinator::{
    CancelToken, ChildOverrides, ChildRequest, CommandSender, CoordinatorCommand, SpawnCommand,
};
use zeroclaw_log::scope;

/// Test seam for [`coordinator_commands`]: a per-test `CommandSender`, so a
/// background-spawn test can drive a real, locally-booted coordinator
/// (`control_plane::coordinator_host::start` against a throwaway
/// `ControlPlaneHandle`, the same way that module's own tests do) without
/// going through `control_plane::global`'s process-wide `OnceLock` — which
/// cannot be uninstalled between tests and would leak into every other test
/// in this binary (see that module's doc, and
/// `agent::loop_::CHILD_ANNOUNCEMENT_STORE_TEST_HOOK` for the same pattern
/// used for the same reason).
#[cfg(test)]
static COMMAND_SENDER_TEST_HOOK: std::sync::Mutex<Option<CommandSender>> =
    std::sync::Mutex::new(None);

/// Where the background path gets the live coordinator's command channel.
///
/// Production always reads the process-global control-plane
/// (`crate::control_plane::control_plane()`); tests may inject a per-test
/// sender through [`COMMAND_SENDER_TEST_HOOK`] instead. `None` either way
/// means "no coordinator is running in this process" — the caller's job is
/// to refuse a background spawn on that, not to guess.
fn coordinator_commands() -> Option<CommandSender> {
    #[cfg(test)]
    {
        if let Some(hooked) = COMMAND_SENDER_TEST_HOOK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Some(hooked);
        }
    }
    crate::control_plane::control_plane().and_then(|cp| cp.commands.clone())
}

/// Spawn an ephemeral SubAgent that inherits the parent agent's
/// identity and runs a focused prompt under the same alias.
pub struct SpawnSubagentTool {
    config: Arc<Config>,
    parent_alias: String,
    security: Arc<SecurityPolicy>,
    /// `true` when this tool is registered inside a run that is itself
    /// a SubAgent. Triggers a depth-1 cap refusal in `execute` before
    /// any spawn work happens. Set by the agent loop from
    /// `AgentRunOverrides.is_subagent` at registry construction time.
    is_subagent_caller: bool,
}

impl SpawnSubagentTool {
    /// Canonical tool name. Referenced by `REENTRANT_AGENT_TOOLS` so a
    /// rename cannot desync the two.
    pub const NAME: &'static str = "spawn_subagent";

    pub fn new(
        config: Arc<Config>,
        parent_alias: impl Into<String>,
        security: Arc<SecurityPolicy>,
    ) -> Self {
        Self {
            config,
            parent_alias: parent_alias.into(),
            security,
            is_subagent_caller: false,
        }
    }

    /// Mark this tool instance as belonging to a SubAgent's tool
    /// registry. Triggers the depth-1 refusal on `execute`. The agent
    /// loop sets this from `AgentRunOverrides.is_subagent`.
    #[must_use]
    pub fn with_subagent_caller(mut self, is_subagent_caller: bool) -> Self {
        self.is_subagent_caller = is_subagent_caller;
        self
    }

    /// The detached path: hand the child to the coordinator and return
    /// immediately, instead of driving `agent::run` in-turn.
    ///
    /// Every gate (depth-1 cap, card/risk-profile self-check, prompt
    /// validation, the rate-limit budget) has already run in `execute`
    /// before this is called — see the call site's comment.
    async fn execute_background(&self, prompt: String) -> Result<ToolResult> {
        let Some(commands) = coordinator_commands() else {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(
                    "spawn_subagent: background=true requires a coordinator, and none is \
                     running in this process (no daemon control-plane, or a control-plane \
                     started without one — see `ControlPlaneHandle::commands`). Retry without \
                     `background`, or run this under the daemon."
                        .into(),
                ),
            });
        };

        let child_id = uuid::Uuid::new_v4().to_string();

        // Identity convention (decided): `parent_alias` is this tool's own
        // parent alias — a detached child is still this agent, not a
        // different agent type (matches the synchronous path above, which
        // spawns via `SubAgentSpawn::for_agent_with_policy(&self.config,
        // &self.parent_alias, ...)`).
        //
        // `parent_session_id` MUST match the fallback `agent::run` adopts
        // for an unscoped turn byte-for-byte
        // (`agent::loop_::synthetic_session_key_for_run`,
        // `format!("agent:{agent_alias}")`, `crates/zeroclaw-runtime/src/agent/loop_.rs`):
        // that is the key `agent::loop_::claim_child_announcements_context`
        // claims under at the start of the parent's *next* turn, and
        // `SubagentPersistence::record_spawn`
        // (`control_plane/subagent_persistence.rs`) files this row's
        // `parent_id` under exactly what we put here. The fallback is the
        // SAME function `run()` uses to establish its synthetic key — one
        // copy, not two spellings that could drift; drift here does not
        // fail loudly, it files the child under a name no turn ever asks
        // about and the parent waits forever.
        let parent_session_id = crate::agent::loop_::current_session_key()
            .unwrap_or_else(|| {
                crate::agent::loop_::synthetic_session_key_for_run(&self.parent_alias)
            });

        const MAX_DESCRIPTION_CHARS: usize = 200;
        let description = if prompt.chars().count() > MAX_DESCRIPTION_CHARS {
            let truncated: String = prompt.chars().take(MAX_DESCRIPTION_CHARS).collect();
            format!("spawn_subagent (background): {truncated}…")
        } else {
            format!("spawn_subagent (background): {prompt}")
        };

        let request = ChildRequest {
            child_id: child_id.clone(),
            prompt,
            description,
            // Inherits the parent's own identity — same reasoning as
            // `parent_session_id` above: this is the parent agent running
            // unattended, not a different configured agent type.
            agent_type: self.parent_alias.clone(),
            parent_session_id,
            parent_alias: self.parent_alias.clone(),
            // `spawn_subagent.rs`'s synchronous path has no concept of "the
            // control-plane task id of the turn currently running me" either
            // (see the `parent_id: None` comment a few lines below in the
            // synchronous body) — same gap, same answer.
            parent_prompt_id: None,
            resume_from: None,
            cwd: None,
            overrides: ChildOverrides::default(),
            // Detached: `coordinator.rs::handle_spawn` sets
            // `handle_only = request.run_in_background`, so this child never
            // gets a foreground budget and its spawning turn never blocks on
            // it — the defining line for what "Detached" means on this
            // protocol.
            run_in_background: true,
            // The announce chain (`agent::loop_::claim_child_announcements_context`)
            // is how the parent ever learns this child's outcome; suppressing
            // completion surfacing here would make a detached child's ending
            // unreachable by design.
            surface_completion: true,
            // Moot once `run_in_background` is true: `coordinator.rs::handle_spawn`
            // only sets a `foreground_deadline` when
            // `!request.run_in_background && !request.await_to_completion`,
            // so this flag has no effect here. `false` for clarity.
            await_to_completion: false,
            fork_context: false,
            cancel_token: CancelToken::new(),
        };

        let (result_tx, _result_rx) = tokio::sync::oneshot::channel();
        // `_result_rx` is deliberately never awaited: this ticket's own
        // "does NOT poll, await, or fetch results for background children"
        // is one reason, and the protocol gives a second, independent one —
        // `coordinator.rs::finish_child` only ever answers this oneshot with
        // the child's *terminal* `ChildResult` (`let sent =
        // respond_to.send(output.result.clone())...`), sent whenever the
        // child actually finishes. There is no separate "accepted, here is
        // an interim handle" reply for an explicit `run_in_background: true`
        // spawn: that interim `backgrounded: true` reply exists only on the
        // *foreground-budget-elapsed* path
        // (`state.rs::background_at_deadline`), which requires
        // `foreground_deadline` to be `Some`, and `handle_spawn` never sets
        // that when `run_in_background` is true. Awaiting `_result_rx` here
        // would block this call until the child's real ending — exactly the
        // synchronous behaviour "background" is supposed to skip. Dropping
        // it is harmless: `finish_child`'s own delivered-bookkeeping
        // (`if !handle_only { foreground_delivered = sent; ... }`) never
        // consults whether anyone received that send while `handle_only` is
        // true, which it is here from the moment this spawns.
        if let Err(error) = commands.0.send(CoordinatorCommand::Spawn(SpawnCommand {
            request: Box::new(request),
            result_tx,
        })) {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!(
                    "spawn_subagent: background spawn failed — the coordinator actor is not \
                     accepting commands (channel closed): {error}"
                )),
            });
        }

        Ok(ToolResult {
            success: true,
            // `child_id=<id>` is a stable, parseable token (see this
            // module's own tests) — everything after it is prose for the
            // model, not for a caller trying to extract the id.
            output: format!(
                "subagent started detached (background), child_id={child_id}. It is running \
                 unattended; its outcome will be announced in a future turn, not returned here."
            )
            .into(),
            error: None,
        })
    }
}

#[async_trait]
impl Tool for SpawnSubagentTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Spawn an ephemeral SubAgent that inherits this agent's identity, \
         security policy, and memory allowlist. The SubAgent runs the supplied \
         prompt to completion under the parent's permissions envelope and \
         returns its response. Use for focused subtasks (research lookup, \
         multi-step reasoning, etc.) that should not pollute this agent's main \
         conversation history. Cost-aware: each SubAgent run is a full agent \
         loop and consumes provider tokens."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The task or question for the SubAgent. Be specific and self-contained — the SubAgent does not see this conversation's history."
                },
                "background": {
                    "type": "boolean",
                    "description": "Run the SubAgent detached instead of waiting for it in this turn. Requires a running coordinator (the daemon); returns immediately with the child's id, and its outcome is announced into a future turn rather than returned here. Defaults to false (wait for the SubAgent's response, as before)."
                }
            },
            "required": ["prompt"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
        // Depth-1 cap: a SubAgent may not spawn its own subagents.
        // The caller-side flag is set at registry construction time
        // from `AgentRunOverrides.is_subagent`, so the refusal fires
        // before any spawn work and before the risk_profile gate.
        if self.is_subagent_caller {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(
                    "spawn_subagent: a subagent may not spawn its own subagents (depth-1 cap)"
                        .into(),
                ),
            });
        }

        // The "allowed" half asks a different question depending on whether
        // a card governs this agent: for a carded agent, the registry's
        // `allowed_tools` came from `card.grants` (`SecurityPolicy::for_agent`,
        // `zeroclaw-config/src/policy.rs`), NOT from the named profile's own
        // `allowed_tools` — so consulting the profile's list here would gate
        // on a list the registry never used, and a card granting
        // `spawn_subagent` whose profile's `allowed_tools` omits it would be
        // admitted by the registry and then refuse itself here, blaming a
        // risk_profile that was never consulted. The "excluded" half is not
        // card-aware in either case: the named profile's `excluded_tools`
        // subtracts from a card's grants too (`AgentCard::risk_profile`'s
        // doc — deny wins), so it must still gate a carded agent.
        if let Some(card) = self.config.card_for_agent(&self.parent_alias) {
            let card_alias = self
                .config
                .agents
                .get(&self.parent_alias)
                // Trimmed to match how `card_for_agent` resolved it — a padded
                // alias in config must not print differently in the refusal
                // than the name that actually gated the branch (codex review
                // of ba37d54bd, finding 3c).
                .map(|a| a.card.as_str().trim())
                .unwrap_or_default();
            let card_grants_it = card.grants.tools.iter().any(|g| g.tool == Self::NAME);
            let profile_excludes_it = self
                .config
                .risk_profile_for_agent(&self.parent_alias)
                .is_some_and(|rp| rp.excluded_tools.iter().any(|t| t == Self::NAME));
            if !card_grants_it {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!(
                        "spawn_subagent: refused — card '{card_alias}' governing agent '{}' does not grant spawn_subagent",
                        self.parent_alias
                    )),
                });
            }
            if profile_excludes_it {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!(
                        "spawn_subagent: refused — agent '{}' risk_profile excludes spawn_subagent (deny wins over card '{card_alias}'s grant)",
                        self.parent_alias
                    )),
                });
            }
        } else if let Some(rp) = self.config.risk_profile_for_agent(&self.parent_alias) {
            let excluded = rp.excluded_tools.iter().any(|t| t == Self::NAME);
            let allowed_when_listed = match &rp.allowed_tools {
                None => true,
                Some(tools) => tools.iter().any(|t| t == Self::NAME),
            };
            if excluded || !allowed_when_listed {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!(
                        "spawn_subagent: refused — agent '{}' risk_profile does not list spawn_subagent in allowed_tools",
                        self.parent_alias
                    )),
                });
            }
        }

        // Argument validation surfaces as a structured `ToolResult`
        // failure (matching the unknown-parent and run-failure shapes
        // below) so the agent loop receives a uniform "tool reported
        // failure" signal regardless of which step rejected the call.
        let prompt = match args
            .get("prompt")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(p) => p.to_string(),
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some("Missing or empty 'prompt' parameter".into()),
                });
            }
        };

        // Additive, optional, default false: absent (or explicitly `false`)
        // takes the synchronous path below byte-identically. A present but
        // non-bool value is rejected here rather than coerced, matching the
        // uniform "structured argument-validation failure" shape `prompt`
        // above already established.
        let background = match args.get("background") {
            None => false,
            Some(serde_json::Value::Bool(b)) => *b,
            Some(_) => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(
                        "spawn_subagent: 'background' must be a boolean when present".into(),
                    ),
                });
            }
        };

        if let Err(error) = self
            .security
            .enforce_tool_operation(ToolOperation::Act, Self::NAME)
        {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(error),
            });
        }

        // Every gate above (depth-1 cap, card/risk-profile self-check,
        // prompt validation, the rate-limit budget) has already run and
        // already applies identically to a detached spawn — a background
        // request is not a way around any of them. Everything below this
        // point is the synchronous in-turn path; the detached path forks off
        // here instead.
        if background {
            return self.execute_background(prompt).await;
        }

        let subagent_ctx = match SubAgentSpawn::for_agent_with_policy(
            &self.config,
            &self.parent_alias,
            Arc::clone(&self.security),
        )
        .and_then(|spawn| spawn.build(SubAgentOverrides::default()))
        {
            Ok(ctx) => ctx,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!("subagent spawn failed: {e:#}")),
                });
            }
        };

        let run_id = uuid::Uuid::new_v4().to_string();

        let temperature: Option<f64> = self
            .config
            .model_provider_for_agent(&self.parent_alias)
            .and_then(|e| e.temperature);
        let session_path = std::path::PathBuf::from(format!("subagent-{run_id}"));

        let run_overrides = AgentRunOverrides {
            security: Some(subagent_ctx.policy.clone()),
            memory: None,
            is_subagent: true,
            // Sub-turn origin already skips memory injection; explicit for
            // the same future-proofing reason as `is_subagent` above.
            suppress_memory_inject: true,
            // Subagents keep a live memory backend and the memory tools; only
            // the injected context preamble is suppressed above.
            memory_free: false,
            // Subagent runs are short-lived; no cross-turn reuse contract,
            // so the per-call `connect_all` path inside `agent::run` is
            // the correct choice. The daemon heartbeat worker is the
            // only `mcp_registry` supplier.
            mcp_registry: None,
        };
        let parent_alias = subagent_ctx.parent_alias.clone();

        let cp_task_id = run_id.clone();
        if let Some(cp) = crate::control_plane::control_plane() {
            let _ = cp
                .store
                .create(crate::control_plane::TaskRecord {
                    id: cp_task_id.clone(),
                    kind: crate::control_plane::TaskKind::Subagent,
                    agent: self.parent_alias.clone(),
                    status: crate::control_plane::TaskStatus::Running,
                    owner_pid: std::process::id(),
                    owner_boot_id: cp.boot_id.clone(),
                    heartbeat_at: None,
                    depth: u32::from(self.is_subagent_caller),
                    // Same gap as `delegate.rs`'s background-delegation
                    // producer: `SpawnSubagentTool` is constructed once per
                    // registry build (`all_tools_with_runtime` in
                    // `crates/zeroclaw-runtime/src/tools/mod.rs`) with no
                    // concept of "the control-plane task id of the turn
                    // currently running me". `is_subagent_caller` already
                    // rules out a subagent spawning a subagent (the depth-1
                    // cap refuses before this point), so every `execute()`
                    // that reaches here belongs to a non-subagent turn — but
                    // that turn can itself be a tracked task (e.g. a
                    // background/parallel delegate's own sub-turn, which
                    // *does* carry this shared tool instance into a Bounded
                    // target's registry — `execute_agentic_with_admission`
                    // only filters the "delegate" tool name, not
                    // "spawn_subagent"). Because the tool instance is shared
                    // across calls, a struct field cannot disambiguate which
                    // task is calling; only ambient per-call context (e.g. a
                    // task-local threaded through the tool-call loop, the way
                    // `TOOL_LOOP_SESSION_KEY` already is) could. `None` is
                    // correct for a genuinely top-level spawn and the best
                    // available answer otherwise — see the dispatch report.
                    parent_id: None,
                    originator_route: None,
                    delivered: false,
                    idem_key: None,
                    principal_id: None,
                    started_at: chrono::Utc::now().to_rfc3339(),
                    finished_at: None,
                })
                .await;
        }

        let run_result = Box::pin(scope!(
            agent_alias: parent_alias,
            session_key: run_id,
            =>
            crate::agent::run(
                (*self.config).clone(),
                &self.parent_alias,
                Some(prompt),
                None,
                None,
                temperature,
                vec![],
                false,
                Some(session_path),
                None,
                zeroclaw_api::ingress::TurnOrigin::SubTurn,
                run_overrides,
            )
        ))
        .await;

        // EPIC-A supervision: mirror the subagent's terminal state into the control-plane.
        if let Some(cp) = crate::control_plane::control_plane() {
            let (status, output, error) = match &run_result {
                Ok(resp) => (
                    crate::control_plane::TaskStatus::Completed,
                    Some(resp.clone()),
                    None,
                ),
                Err(e) => (
                    crate::control_plane::TaskStatus::Failed,
                    None,
                    Some(format!("subagent run failed: {e}")),
                ),
            };
            let _ = cp
                .store
                .update_status(&cp_task_id, status, output, error)
                .await;
        }

        match run_result {
            Ok(response) => Ok(ToolResult {
                success: true,
                output: if response.trim().is_empty() {
                    "subagent completed without output".to_string().into()
                } else {
                    response.into()
                },
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("subagent run failed: {e}")),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_config::schema::{AliasedAgentConfig, Config, RiskProfileConfig};

    fn config_with_agent(alias: &str) -> Config {
        let mut config = Config::default();
        config
            .risk_profiles
            .insert("default".to_string(), RiskProfileConfig::default());
        config.agents.insert(
            alias.to_string(),
            AliasedAgentConfig {
                risk_profile: "default".into(),
                ..AliasedAgentConfig::default()
            },
        );
        config
    }

    #[tokio::test]
    async fn empty_or_missing_prompt_is_rejected() {
        let tool = SpawnSubagentTool::new(
            Arc::new(config_with_agent("alpha")),
            "alpha",
            Arc::new(SecurityPolicy::default()),
        );
        for args in [json!({}), json!({ "prompt": "   " })] {
            let result = tool
                .execute(args)
                .await
                .expect("execute returns Ok with structured failure");
            assert!(!result.success);
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("prompt"),
                "expected prompt-validation error, got: {:?}",
                result.error
            );
        }
    }

    #[tokio::test]
    async fn unknown_parent_alias_surfaces_spawn_failure() {
        // Parent alias that is not configured: SubAgentSpawn::for_agent_with_policy
        // returns Err, the tool reports a structured spawn failure
        // (no panic, no recursion attempt).
        let tool = SpawnSubagentTool::new(
            Arc::new(Config::default()),
            "missing-alpha",
            Arc::new(SecurityPolicy::default()),
        );
        let result = tool
            .execute(json!({ "prompt": "hello" }))
            .await
            .expect("execute returns Ok with structured failure");
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("subagent spawn failed"),
            "expected spawn-failure error, got: {:?}",
            result.error
        );
    }

    // ── Depth-1 cap: subagent may not spawn its own subagent ──

    #[tokio::test]
    async fn refuses_recursive_spawn_when_caller_is_subagent() {
        let tool = SpawnSubagentTool::new(
            Arc::new(config_with_agent("alpha")),
            "alpha",
            Arc::new(SecurityPolicy::default()),
        )
        .with_subagent_caller(true);
        let result = tool
            .execute(json!({ "prompt": "hello" }))
            .await
            .expect("execute returns Ok with structured failure");
        assert!(!result.success);
        let err = result.error.as_deref().unwrap_or_default();
        assert!(
            err.contains("subagent") && err.contains("depth"),
            "expected depth-cap refusal mentioning subagent + depth, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn allows_top_level_spawn_when_caller_is_not_subagent() {
        // The top-level path may still fail later for unrelated reasons
        // (e.g. no model provider configured in this minimal harness),
        // but it MUST NOT trip the depth-cap refusal. Pin that the
        // depth-cap error is absent.
        let tool = SpawnSubagentTool::new(
            Arc::new(config_with_agent("alpha")),
            "alpha",
            Arc::new(SecurityPolicy::default()),
        )
        .with_subagent_caller(false);
        let result = tool
            .execute(json!({ "prompt": "hello" }))
            .await
            .expect("execute returns Ok");
        let err = result.error.as_deref().unwrap_or_default();
        assert!(
            !(err.contains("subagent") && err.contains("depth")),
            "top-level caller must not see the depth-cap refusal, got: {err:?}"
        );
    }

    // ── risk-profile tool gates spawn_subagent ──

    fn config_with_allowed_tools(alias: &str, allowed_tools: Vec<String>) -> Config {
        let mut config = Config::default();
        config.risk_profiles.insert(
            "default".to_string(),
            RiskProfileConfig {
                allowed_tools: Some(allowed_tools),
                ..RiskProfileConfig::default()
            },
        );
        config.agents.insert(
            alias.to_string(),
            AliasedAgentConfig {
                risk_profile: "default".into(),
                ..AliasedAgentConfig::default()
            },
        );
        config
    }

    #[tokio::test]
    async fn refuses_when_risk_profile_excludes_spawn_subagent() {
        // Parent's non-empty risk_profile.allowed_tools omits
        // "spawn_subagent" — the tool itself refuses pre-spawn so the
        // dispatch-site filter doesn't have to be the only line of defense.
        let config = config_with_allowed_tools("alpha", vec!["shell".into()]);
        let tool = SpawnSubagentTool::new(
            Arc::new(config),
            "alpha",
            Arc::new(SecurityPolicy::default()),
        );
        let result = tool
            .execute(json!({ "prompt": "hello" }))
            .await
            .expect("execute returns Ok with structured failure");
        assert!(!result.success);
        let err = result.error.as_deref().unwrap_or_default();
        assert!(
            err.contains("risk_profile") && err.contains("spawn_subagent"),
            "expected risk_profile-gate refusal naming spawn_subagent, got: {err:?}"
        );
    }

    // ── card-aware gate: a carded parent's self-check consults the card's
    // grants, not the profile's own allowed_tools ──

    /// Builds a `Config` with one `[risk_profiles.<profile_alias>]` entry and
    /// one `[cards.<card_alias>]` entry naming it, wired to a single agent
    /// defined solely by `card = <card_alias>` (no `risk_profile` set,
    /// matching what `Config::validate()` requires for a carded agent).
    fn carded_config_with_agent(
        alias: &str,
        card_alias: &str,
        card_grants_spawn_subagent: bool,
        profile: RiskProfileConfig,
    ) -> Config {
        use zeroclaw_config::card::{AgentCard, CardGrants, GrantClass, ToolGrant};

        let mut config = Config::default();
        config
            .risk_profiles
            .insert("carded_profile".to_string(), profile);
        config.cards.insert(
            card_alias.to_string(),
            AgentCard {
                risk_profile: "carded_profile".into(),
                grants: CardGrants {
                    tools: if card_grants_spawn_subagent {
                        vec![ToolGrant::new(
                            SpawnSubagentTool::NAME,
                            GrantClass::LocalAct,
                        )]
                    } else {
                        vec![]
                    },
                    ..CardGrants::default()
                },
                ..AgentCard::default()
            },
        );
        config.agents.insert(
            alias.to_string(),
            AliasedAgentConfig {
                card: card_alias.into(),
                // risk_profile deliberately left empty — validation forbids
                // setting both card and risk_profile on the same agent.
                ..AliasedAgentConfig::default()
            },
        );
        config
    }

    #[tokio::test]
    async fn carded_parent_not_refused_when_profiles_allowed_tools_omits_it_but_card_grants_it() {
        // This is the registry/tool disagreement case the fix targets: the
        // registry's allowed_tools came from the card's grants (`for_agent`),
        // not from the profile's own `allowed_tools` — so a profile that
        // omits "spawn_subagent" from a non-empty `allowed_tools` must not
        // make the self-check refuse a tool the card actually granted.
        let config = carded_config_with_agent(
            "alpha",
            "trader_card",
            true,
            RiskProfileConfig {
                allowed_tools: Some(vec!["shell".into()]),
                ..RiskProfileConfig::default()
            },
        );
        let tool = SpawnSubagentTool::new(
            Arc::new(config),
            "alpha",
            Arc::new(SecurityPolicy::default()),
        );
        let result = tool
            .execute(json!({ "prompt": "hello" }))
            .await
            .expect("execute returns Ok");
        let err = result.error.as_deref().unwrap_or_default();
        assert!(
            !err.contains("does not grant spawn_subagent")
                && !err.contains("does not list spawn_subagent"),
            "a card-granted spawn_subagent must not be refused by the self-check \
             merely because the profile's own allowed_tools omits it, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn carded_parent_refused_when_card_does_not_grant_it_and_message_names_the_card() {
        let config = carded_config_with_agent(
            "alpha",
            "trader_card",
            false,
            RiskProfileConfig::default(),
        );
        let tool = SpawnSubagentTool::new(
            Arc::new(config),
            "alpha",
            Arc::new(SecurityPolicy::default()),
        );
        let result = tool
            .execute(json!({ "prompt": "hello" }))
            .await
            .expect("execute returns Ok with structured failure");
        assert!(!result.success);
        let err = result.error.as_deref().unwrap_or_default();
        assert!(
            err.contains("trader_card") && err.contains("does not grant spawn_subagent"),
            "refusal must name the card that governs this agent, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn carded_parent_refused_when_profile_excludes_it_even_though_card_grants_it() {
        // Deny wins: the named profile's own excluded_tools subtracts from
        // the card's grants regardless (documented ruling carried over from
        // the fail-closed fix this self-check must not undo).
        let config = carded_config_with_agent(
            "alpha",
            "trader_card",
            true,
            RiskProfileConfig {
                excluded_tools: vec![SpawnSubagentTool::NAME.to_string()],
                ..RiskProfileConfig::default()
            },
        );
        let tool = SpawnSubagentTool::new(
            Arc::new(config),
            "alpha",
            Arc::new(SecurityPolicy::default()),
        );
        let result = tool
            .execute(json!({ "prompt": "hello" }))
            .await
            .expect("execute returns Ok with structured failure");
        assert!(!result.success);
        let err = result.error.as_deref().unwrap_or_default();
        assert!(
            err.contains("excludes spawn_subagent"),
            "a profile exclusion must still refuse a carded agent, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn admits_when_risk_profile_lists_spawn_subagent() {
        // When the parent's risk_profile.allowed_tools explicitly lists
        // spawn_subagent, the tool does NOT short-circuit on the gate.
        // It may still fail later for unrelated reasons; pin only that
        // the gate refusal is absent.
        let config =
            config_with_allowed_tools("alpha", vec!["spawn_subagent".into(), "shell".into()]);
        let tool = SpawnSubagentTool::new(
            Arc::new(config),
            "alpha",
            Arc::new(SecurityPolicy::default()),
        );
        let result = tool
            .execute(json!({ "prompt": "hello" }))
            .await
            .expect("execute returns Ok");
        let err = result.error.as_deref().unwrap_or_default();
        assert!(
            !(err.contains("risk_profile") && err.contains("spawn_subagent")),
            "spawn_subagent in allowed_tools must not trigger the gate refusal, got: {err:?}"
        );
    }

    // ── Launch-side fan-out bound: shared action budget ──

    #[tokio::test]
    async fn repeated_spawns_blocked_once_action_budget_is_exhausted() {
        let security = Arc::new(SecurityPolicy {
            max_actions_per_hour: 2,
            ..SecurityPolicy::default()
        });
        let tool = SpawnSubagentTool::new(
            Arc::new(config_with_agent("alpha")),
            "alpha",
            Arc::clone(&security),
        );

        for attempt in 1..=2 {
            let result = tool
                .execute(json!({ "prompt": "same fan-out prompt" }))
                .await
                .expect("execute returns Ok");
            let err = result.error.as_deref().unwrap_or_default();
            assert!(
                !err.contains("Rate limit exceeded"),
                "attempt {attempt} within budget must not be rate-limited, got: {err:?}"
            );
        }

        let result = tool
            .execute(json!({ "prompt": "same fan-out prompt" }))
            .await
            .expect("execute returns Ok with structured failure");
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Rate limit exceeded"),
            "3rd launch attempt past a budget of 2 must be refused, got: {:?}",
            result.error
        );
    }

    #[tokio::test]
    async fn validation_failures_do_not_consume_launch_budget() {
        // The budget gate sits after prompt validation: malformed calls
        // must not burn launch slots (matching RateLimitedTool's
        // only-work-consumes-budget semantics).
        let security = Arc::new(SecurityPolicy {
            max_actions_per_hour: 1,
            ..SecurityPolicy::default()
        });
        let tool = SpawnSubagentTool::new(
            Arc::new(config_with_agent("alpha")),
            "alpha",
            Arc::clone(&security),
        );

        for _ in 0..3 {
            let result = tool
                .execute(json!({ "prompt": "   " }))
                .await
                .expect("execute returns Ok with structured failure");
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("prompt"),
                "invalid-prompt refusal expected, got: {:?}",
                result.error
            );
        }

        let result = tool
            .execute(json!({ "prompt": "valid" }))
            .await
            .expect("execute returns Ok");
        let err = result.error.as_deref().unwrap_or_default();
        assert!(
            !err.contains("Rate limit exceeded"),
            "validation failures must not have consumed the budget, got: {err:?}"
        );
    }

    #[test]
    fn agent_run_overrides_default_is_top_level() {
        use crate::agent::loop_::AgentRunOverrides;
        let overrides = AgentRunOverrides::default();
        assert!(
            !overrides.is_subagent,
            "AgentRunOverrides::default().is_subagent must be false so cron paths inherit a top-level shape"
        );
    }

    #[test]
    fn spawn_subagent_dyn_tool_implements_attributable() {
        use zeroclaw_api::attribution::{Attributable, Role, ToolKind};

        let tool: Box<dyn Tool> = Box::new(SpawnSubagentTool::new(
            Arc::new(config_with_agent("alpha")),
            "alpha",
            Arc::new(SecurityPolicy::default()),
        ));
        assert_eq!(
            Attributable::role(tool.as_ref()),
            Role::Tool(ToolKind::SpawnSubagent),
            "SpawnSubagentTool must surface its kind through the Tool trait object"
        );
        assert!(
            !Attributable::alias(tool.as_ref()).is_empty(),
            "Attributable::alias on a Tool must be non-empty so composite keys never produce `.<bare>`"
        );
    }

    // ── `background: true` — the detached path ──

    mod background {
        use super::*;
        use crate::control_plane::boot::ControlPlaneHandle;
        use crate::control_plane::coordinator_host;
        use crate::control_plane::task_registry::{TaskRegistry, TaskStatus};
        use std::sync::Mutex as StdMutex;
        use std::time::Duration;

        /// `COMMAND_SENDER_TEST_HOOK` is a single process-global slot: two of
        /// these tests installing it concurrently would clobber each other's
        /// sender. Same shape as `agent::loop_`'s own `SERIALIZE` guard
        /// around its process-global test hooks, for the same reason.
        static SERIALIZE: StdMutex<()> = StdMutex::new(());

        /// A live coordinator actor wired the same way
        /// `coordinator_host.rs`'s own tests boot one — a real
        /// `ControlPlaneHandle` over a tempdir-backed `SqliteTaskStore`, a
        /// real `Coordinator::with_persistence`, a real `NativeChildRunner`
        /// — with its `CommandSender` installed into
        /// [`COMMAND_SENDER_TEST_HOOK`] so [`coordinator_commands`] finds it
        /// without touching the process-wide `OnceLock`.
        struct BootedCoordinator {
            _serialize: std::sync::MutexGuard<'static, ()>,
            _dir: tempfile::TempDir,
            handle: ControlPlaneHandle,
            actor: Option<tokio::task::JoinHandle<()>>,
        }

        impl Drop for BootedCoordinator {
            fn drop(&mut self) {
                *COMMAND_SENDER_TEST_HOOK
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = None;
                if let Some(actor) = self.actor.take() {
                    actor.abort();
                }
            }
        }

        async fn boot(config: Config) -> BootedCoordinator {
            let serialize = SERIALIZE.lock().unwrap_or_else(|e| e.into_inner());
            let dir = tempfile::tempdir().expect("tempdir");
            let handle = ControlPlaneHandle::start(dir.path())
                .await
                .expect("start control plane");
            let host = coordinator_host::start(
                Arc::new(config),
                Arc::clone(&handle.sqlite_store),
                handle.boot_id.clone(),
            );
            *COMMAND_SENDER_TEST_HOOK
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(host.commands);
            BootedCoordinator {
                _serialize: serialize,
                _dir: dir,
                handle,
                actor: Some(host.actor),
            }
        }

        /// Pull the `child_id=<id>` token out of the tool's own success
        /// message (see `execute_background`'s doc on that format).
        fn extract_child_id(output: &str) -> &str {
            let after = output
                .split("child_id=")
                .nth(1)
                .expect("success output must carry child_id=");
            after
                .split(|c: char| c == ',' || c == '.' || c.is_whitespace())
                .next()
                .expect("child_id token must not be empty")
        }

        async fn wait_for_terminal(
            store: &crate::control_plane::SqliteTaskStore,
            id: &str,
            timeout: Duration,
        ) -> crate::control_plane::TaskRecord {
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                if let Some(rec) = store.get(id).await.expect("store read") {
                    if rec.status.is_terminal() {
                        return rec;
                    }
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "child {id} never reached a terminal status within {timeout:?}"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        /// Discriminating line: `assert!(result.success, ...)` returning
        /// before the child's own turn can possibly have finished (there is
        /// no live model provider in this harness — the turn fails fast, but
        /// the tool call itself must not have waited for that). A
        /// synchronous implementation that awaits the child would still make
        /// this assertion pass (the child fails fast) but would fail the
        /// row-exists-immediately-after-yield_now check below, which is the
        /// real discriminator between "returned immediately" and "awaited
        /// the child".
        #[tokio::test]
        async fn background_spawn_returns_immediately_with_a_child_id_and_row() {
            let alias = "bg-alpha";
            let config = config_with_agent(alias);
            let fixture = boot(config.clone()).await;

            let tool = SpawnSubagentTool::new(
                Arc::new(config),
                alias,
                Arc::new(SecurityPolicy::default()),
            );
            let result = tool
                .execute(json!({ "prompt": "do the background thing", "background": true }))
                .await
                .expect("execute returns Ok");
            assert!(
                result.success,
                "background spawn must report success immediately: {:?}",
                result.error
            );
            let child_id = extract_child_id(result.output.as_str()).to_string();

            // Let the actor's task get polled once — `handle_spawn` inserts
            // into `pending` and calls `record_spawn` synchronously within
            // the same command-branch arm, so one yield is enough (same
            // reasoning as `coordinator_host.rs`'s own
            // `drop_after_abort_marks_a_mid_flight_child_lost_in_the_real_store`).
            tokio::task::yield_now().await;

            let row = fixture
                .handle
                .sqlite_store
                .get(&child_id)
                .await
                .expect("store read")
                .expect("record_spawn must have written the row");
            assert_eq!(
                row.parent_id.as_deref(),
                Some(format!("agent:{alias}").as_str()),
                "parent_id must be the same key agent::run's fallback claims under"
            );
            assert_eq!(row.agent, alias, "agent column carries the parent alias");

            // The runner has no live model provider, so the child's own turn
            // fails fast — it still must land terminal, not linger Running
            // forever, and it must carry a detail (not silently "succeed").
            let finished = wait_for_terminal(
                &fixture.handle.sqlite_store,
                &child_id,
                Duration::from_secs(10),
            )
            .await;
            assert_eq!(
                finished.status,
                TaskStatus::Failed,
                "no live model provider in this harness — the child must fail, not succeed"
            );
        }

        /// Discriminating line: `assert!(err.contains("no coordinator") ||
        /// err.contains("coordinator"))` together with `!result.success` —
        /// a silent fallback to the synchronous path would instead try to
        /// run the child in-turn (and fail for an unrelated reason, or
        /// succeed), never naming "no coordinator" at all.
        #[tokio::test]
        async fn background_true_with_no_coordinator_is_a_structured_failure() {
            let alias = "bg-no-coordinator";
            let tool = SpawnSubagentTool::new(
                Arc::new(config_with_agent(alias)),
                alias,
                Arc::new(SecurityPolicy::default()),
            );
            let result = tool
                .execute(json!({ "prompt": "hello", "background": true }))
                .await
                .expect("execute returns Ok with structured failure");
            assert!(!result.success);
            let err = result.error.as_deref().unwrap_or_default();
            assert!(
                err.contains("coordinator"),
                "refusal must name the missing coordinator, got: {err:?}"
            );
        }

        /// Discriminating line: `assert_eq!(row.parent_id.as_deref(), Some(...))`
        /// — a hand-rolled fallback that drifts from `agent::run`'s
        /// (`agent::loop_::synthetic_session_key_for_run`) would still spawn
        /// successfully but file the row under a key the waker never claims,
        /// silently orphaning every detached child's announcement.
        #[tokio::test]
        async fn parent_key_fallback_is_agent_colon_alias_with_no_ambient_session_key() {
            let alias = "bg-fallback-alias";
            let config = config_with_agent(alias);
            let fixture = boot(config.clone()).await;

            let tool = SpawnSubagentTool::new(
                Arc::new(config),
                alias,
                Arc::new(SecurityPolicy::default()),
            );
            let result = tool
                .execute(json!({ "prompt": "hello", "background": true }))
                .await
                .expect("execute returns Ok");
            assert!(result.success, "unexpected failure: {:?}", result.error);
            let child_id = extract_child_id(result.output.as_str()).to_string();

            tokio::task::yield_now().await;
            let row = fixture
                .handle
                .sqlite_store
                .get(&child_id)
                .await
                .expect("store read")
                .expect("row must exist");
            assert_eq!(row.parent_id.as_deref(), Some(format!("agent:{alias}").as_str()));
        }

        /// Absent/`false` `background` must take the byte-identical
        /// synchronous path — every pre-existing test above this module
        /// already pins that behaviour by never setting `background` at
        /// all; this test only pins that an *explicit* `false` is the same
        /// as absent.
        #[tokio::test]
        async fn explicit_background_false_matches_the_default_synchronous_path() {
            let alias = "alpha";
            let tool = SpawnSubagentTool::new(
                Arc::new(config_with_agent(alias)),
                alias,
                Arc::new(SecurityPolicy::default()),
            );
            let with_false = tool
                .execute(json!({ "prompt": "hello", "background": false }))
                .await
                .expect("execute returns Ok");
            let without = tool
                .execute(json!({ "prompt": "hello" }))
                .await
                .expect("execute returns Ok");
            assert_eq!(
                with_false.success, without.success,
                "explicit background=false must behave like the field's absence"
            );
            assert_eq!(
                with_false.error.is_some(),
                without.error.is_some(),
                "explicit background=false must behave like the field's absence"
            );
        }

        #[tokio::test]
        async fn background_non_bool_is_a_structured_validation_failure() {
            let alias = "alpha";
            let tool = SpawnSubagentTool::new(
                Arc::new(config_with_agent(alias)),
                alias,
                Arc::new(SecurityPolicy::default()),
            );
            let result = tool
                .execute(json!({ "prompt": "hello", "background": "yes" }))
                .await
                .expect("execute returns Ok with structured failure");
            assert!(!result.success);
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("background"),
                "expected a background-validation error, got: {:?}",
                result.error
            );
        }
    }
}

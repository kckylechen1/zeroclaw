//! Interactive CLI `run` entry point for the agent loop.
//!
//! Extracted from `loop_/mod.rs` so the channel/gateway `process_message`
//! path and the interactive CLI assembly can evolve independently.

use crate::agent::TurnMeta;
use crate::approval::ApprovalManager;
use crate::observability::{self, Observer, ObserverEvent};
use crate::platform;
use crate::security::SecurityPolicy;
use crate::tools;
use crate::tools::scoped;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use zeroclaw_api::ingress::{IngressContext, TurnOrigin};
use zeroclaw_config::schema::Config;
use zeroclaw_memory::{self, Memory, MemoryCategory};
use zeroclaw_providers::{self, ChatMessage, ModelProvider};

use super::{
    AUTOSAVE_MIN_MESSAGE_CHARS, AgentRunOverrides, CLI_CHANNEL_FN, CappedLine, DraftEvent,
    LoopKnobs, MAX_INTERACTIVE_INPUT_BYTES, ResolvedAgentExecution, ResolvedIo,
    ResolvedModelAccess, ResolvedRuntimeKnobs, StreamDelta, TOOL_LOOP_COST_TRACKING_CONTEXT,
    ToolLoop, agent_provider_composite, api_key_and_uri_for_provider, autosave_memory_key,
    build_hardware_context, build_system_prompt_for_turn, claim_announcements_for_turn,
    compute_excluded_mcp_tools, format_tokens, is_model_switch_requested, is_tool_loop_cancelled,
    load_interactive_session_history, observe_turn_user_message, read_capped_line,
    resolved_agent_for_turn, retain_registered_tool_descriptions, run_tool_call_loop,
    save_interactive_session_history, scope_session_key, seed_channel_handles,
    session_key_is_scoped, settle_announcement_guards, synthetic_session_key_for_run, trim_history,
};

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub async fn run(
    config: Config,
    agent_alias: &str,
    message: Option<String>,
    provider_override: Option<String>,
    model_override: Option<String>,
    temperature: Option<f64>,
    peripheral_overrides: Vec<String>,
    interactive: bool,
    session_state_file: Option<PathBuf>,
    allowed_tools: Option<Vec<String>>,
    origin: TurnOrigin,
    overrides: AgentRunOverrides,
) -> Result<String> {
    use ::zeroclaw_log::Instrument;
    let agent = resolved_agent_for_turn(&config, agent_alias)?;
    crate::agent::thinking::validate_thinking_config(&agent.resolved.thinking);
    let risk_profile = config
        .risk_profile_for_agent(agent_alias)
        .with_context(|| {
            format!(
                "agents.{agent_alias}.risk_profile does not name a configured risk_profiles entry"
            )
        })?
        .clone();
    // Rendered once and reused across every prompt build in this turn, same
    // as `risk_profile` above — the persona resolution is stable for the
    // whole call, only the tool/skill surface changes per rebuild.
    let persona_section = config
        .persona_for_agent(agent_alias)
        .and_then(zeroclaw_config::persona::PersonaKnobs::to_prompt_section);
    let memory_composite = {
        use zeroclaw_config::multi_agent::MemoryBackendKind;
        match agent.memory.backend {
            MemoryBackendKind::Markdown => format!("markdown.{agent_alias}"),
            MemoryBackendKind::None => "none".to_string(),
            _ => {
                let raw = config.memory.backend.trim();
                if raw.is_empty() || raw.eq_ignore_ascii_case("none") {
                    "none".to_string()
                } else {
                    let (kind, alias) = raw.split_once('.').unwrap_or((raw, "default"));
                    format!("{kind}.{alias}")
                }
            }
        }
    };
    // ── Session key for this turn ─────────────────────────────────────
    // Every `run()`-based path (CLI, cron, heartbeat, SOP step, a detached
    // child run) reaches here with no session key in scope, because only the
    // four conversational entry points scope one (gateway-WS, the channel
    // orchestrator, ACP, RPC). Without a key this turn cannot claim the
    // children it dispatched — the announce chain has a parent id to file
    // children under, but no name to look them up by. So `run()` supplies one
    // when, and only when, nobody else did: a nested run inside a scoped turn
    // must keep seeing its caller's key, or the child's own dispatches would be
    // filed under a name the parent never asks about.
    //
    // See `synthetic_session_key_for_run` for why the fallback is per-alias
    // rather than per-run, and what that trades away.
    //
    // This flag also decides whether this turn *claims* (below): only the run
    // that named the conversation announces into it. Do not read an inherited
    // key as "this run is isolated, so claiming here is harmless" — a nested
    // `agent::run` genuinely shares its caller's task-local key. The wrapper
    // that looks like isolation is not one: `zeroclaw_log::scope!`
    // (`crates/zeroclaw-log/src/macro.rs:48-56`) expands to
    // `.instrument(info_span!(session_key = ...))`, a tracing span field, and
    // never touches `TOOL_LOOP_SESSION_KEY`. So the `scope!(session_key: ...)`
    // wrapped `crate::agent::run(...)` in `tools/spawn_subagent.rs`, awaited
    // inline inside the parent's tool-call loop and therefore on the parent's
    // task, runs under the parent's key. Claiming there would hand the
    // parent's finished children to the subagent's context and the parent
    // would never hear about them — the loss that
    // `claim_child_announcements_context`'s ordering rules exist to prevent.
    let __zc_session_key_scoped = session_key_is_scoped();
    let __zc_synthetic_session_key = synthetic_session_key_for_run(agent_alias);
    // Root-lineage fallback owned by the async block below without moving
    // `__zc_synthetic_session_key` (still needed after the block).
    let __zc_lineage_root_fallback = __zc_synthetic_session_key.clone();
    let __zc_alias = agent_alias.to_string();
    let __zc_attribution_span =
        ::zeroclaw_log::attribution_span!(&crate::agent::AgentAttribution(__zc_alias.as_str()));
    let __zc_scope_span = ::zeroclaw_log::info_span!(
        target: "zeroclaw_log_internal_scope",
        "zeroclaw_scope",
        risk_profile = %agent.risk_profile,
        runtime_profile = %agent.runtime_profile,
        memory_namespace = %memory_composite,
    );
    let __zc_body = async move {
        let agent_alias: &str = __zc_alias.as_str();
        // Whether this turn is the one that announces. True only when this
        // `run()` named the conversation itself; an inherited key means an
        // outer entry point owns this conversation's claims and will deliver
        // the children into its own next turn.
        let owns_session_key = !__zc_session_key_scoped;
        // ── Effective per-agent runtime tunables ──────────────────────
        // Profile values (when set) override the agent's inline fields.
        // See `Config::resolved_agent_config` for precedence rules.
        let eff_max_history_messages = agent.resolved.max_history_messages;
        let eff_compact_context = agent.resolved.compact_context;
        let eff_max_system_prompt_chars = agent.resolved.max_system_prompt_chars;
        let eff_model_context_window = agent.resolved.model_context_window;
        let eff_prompt_injection_mode = agent.resolved.prompt_injection_mode;
        let base_observer = observability::create_observer(&config.observability);
        let observer: Arc<dyn Observer> = Arc::from(base_observer);
        let turn_id = uuid::Uuid::new_v4().to_string();
        let channel_name = if interactive { "cli" } else { "daemon" };
        let _flush_guard = interactive.then(|| observability::FlushGuard::new(observer.clone()));
        if interactive
            && matches!(
                config.observability.backend,
                zeroclaw_config::schema::ObservabilityBackend::Prometheus
            )
        {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "Observability backend is Prometheus (pull/scrape model): a one-shot CLI process \
                 exits before any scraper can pull, so its telemetry will not be collected. \
                 Prometheus is intended for long-running (daemon) deployments."
            );
        }
        let runtime: Arc<dyn platform::RuntimeAdapter> =
            Arc::from(platform::create_runtime(&config.runtime)?);
        let is_subagent_caller = overrides.is_subagent;
        let suppress_memory_inject = overrides.suppress_memory_inject;
        let memory_free = overrides.memory_free;
        // Unified spawn lineage (SA-9/SA-11): the run's effective lineage
        // is the spawning context's lineage, or a fresh root minted from
        // this run's session key when nobody passed one (top-level turn,
        // cron job — a typed root transition, never a silent reset).
        // This value is what the registry below is built with, so a
        // registry rebuild inside a child inherits the child's lineage
        // and depth can never reset across a rebuild.
        let effective_lineage = overrides.lineage.clone().unwrap_or_else(|| {
            zeroclaw_api::subagent_v1::LineageRef::new_root(
                zeroclaw_api::subagent_v1::ParentRunRef::from_opaque(
                    crate::agent::loop_::current_session_key()
                        .unwrap_or_else(|| __zc_lineage_root_fallback.clone()),
                ),
            )
        });
        let security = match overrides.security {
            Some(sec) => sec,
            None => Arc::new(SecurityPolicy::for_agent(&config, agent_alias)?),
        };

        let agent_provider_resolved = config
            .resolved_model_provider_for_agent(agent_alias)
            .map(|(ty, alias, cfg)| (ty, alias.to_string(), cfg.clone()));
        let agent_model_provider = agent_provider_resolved.as_ref().map(|(_, _, cfg)| cfg);

        let mem: Arc<dyn Memory> = if memory_free {
            Arc::new(zeroclaw_memory::NoneMemory::new("none"))
        } else {
            match overrides.memory {
                Some(m) => m,
                None => {
                    zeroclaw_memory::create_memory_for_agent(
                        &config,
                        agent_alias,
                        agent_model_provider.and_then(|e| e.api_key.as_deref()),
                    )
                    .await?
                }
            }
        };
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Load)
                .with_category(::zeroclaw_log::EventCategory::Memory)
                .with_attrs(::serde_json::json!({"backend": mem.name()})),
            "Memory initialized"
        );

        // ── Peripherals (merge peripheral tools into registry) ─
        if !peripheral_overrides.is_empty() {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Load)
                    .with_category(::zeroclaw_log::EventCategory::Agent)
                    .with_attrs(::serde_json::json!({"peripherals": peripheral_overrides})),
                "Peripheral overrides from CLI (config boards take precedence)"
            );
        }

        // ── Tools (including memory tools and peripherals) ────────────
        let (composio_key, composio_entity_id) = if config.composio.enabled {
            (
                config.composio.api_key.as_deref(),
                Some(config.composio.entity_id.as_str()),
            )
        } else {
            (None, None)
        };


        let all_tools_result = tools::all_tools_with_runtime(
            Arc::new(config.clone()),
            &security,
            &risk_profile,
            agent_alias,
            runtime.clone(),
            mem.clone(),
            composio_key,
            composio_entity_id,
            &config.browser,
            &config.http_request,
            &config.web_fetch,
            &config.data_dir,
            &config.agents,
            agent_model_provider.and_then(|e| e.api_key.as_deref()),
            &config,
            None,
            is_subagent_caller,
            None,
            None,
            Some(effective_lineage.clone()),
        );
        let skills = crate::skills::load_skills_for_agent_from_config(&config, agent_alias);
        // Route the per-agent tool registry through the one gated seam
        // (peripherals -> built-in filter -> MCP scope+gate -> skills), identical
        // to the behavior this path hand-rolled. `caller_allowed` carries the
        // run() per-run allowlist; connect_peripherals is true (execution path).
        let assembled = scoped::ScopedToolRegistry::assemble(scoped::ScopedAssembly {
            config: &config,
            agent_alias,
            security: &security,
            built: all_tools_result,
            skills: &skills,
            runtime: runtime.clone(),
            caller_allowed: allowed_tools.as_deref(),
            connect_mcp: true,
            connect_peripherals: true,
            // A memory-free run drops the persistent memory tools so the model
            // cannot read or write memory even though the registry is otherwise
            // built identically.
            exclude_memory: memory_free,
            list_deferred_mcp_specs: false,
            emit_assembly_logs: true,
            // Honor the daemon worker's pre-built shared registry so stdio
            // MCP children live for the daemon's lifetime, not per
            // `agent::run` call. CLI/one-shot callers leave
            // `mcp_registry` at its default (`None`) and the seam
            // falls back to the per-call `connect_all`.
            mcp_registry: overrides.mcp_registry.as_ref().map(Arc::clone),
        })
        .await;
        // run injects one combined MCP prompt block: deferred tool-search listing +
        // pinned resources, composed by the harness.
        let deferred_section = assembled.combined_mcp_prompt_section();
        let scoped::ScopedAssembled {
            registry,
            delegate_handle: _,
            ask_user_handle,
            reaction_handle,
            poll_handle,
            escalate_handle,
            channel_room_handle,
            activated_handle,
            mcp_tool_names,
            ..
        } = assembled;
        let tools_registry = registry.into_inner();

        // Populate all channel-driven tool handles from the registered factory.
        let count = seed_channel_handles(
            &ask_user_handle,
            &channel_room_handle,
            &reaction_handle,
            &poll_handle,
            &escalate_handle,
        );
        if count > 0 {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Register)
                    .with_category(::zeroclaw_log::EventCategory::Channel)
                    .with_attrs(::serde_json::json!({"count": count})),
                &format!("Registered {} channel(s) for CLI agent", count),
            );
        }

        // ── Resolve model_provider ─────────────────────────────────────────
        let agent_provider_ref = agent_provider_composite(&config, agent_alias);
        let mut provider_name = provider_override
            .as_deref()
            .or(agent_provider_ref.as_deref())
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_category(::zeroclaw_log::EventCategory::Agent)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"agent_alias": agent_alias})),
                    "agent loop refused: agent.model_provider unresolved and no --provider override"
                );
                anyhow::Error::msg(format!(
                    "agents.{agent_alias}.model_provider does not resolve and no provider override \
                     was passed on the CLI. Either set `[agents.{agent_alias}] model_provider` or \
                     pass --provider."
                ))
            })?
            .to_string();

        let mut model_name = match model_override
            .as_deref()
            .or(agent_model_provider.and_then(|e| e.model.as_deref()))
        {
            Some(m) => m.to_string(),
            None => anyhow::bail!(
                "no model configured for agent {agent_alias}: \
             [providers.models.{provider_name}.<alias>].model is unset and --model was not passed"
            ),
        };

        {
            let span = zeroclaw_log::Span::current();
            let mp_composite = match agent_provider_resolved.as_ref() {
                Some((ty, alias, _)) => format!("{ty}.{alias}"),
                None => provider_name.clone(),
            };
            span.record("model_provider", mp_composite.as_str());
            span.record("model", model_name.as_str());
        }

        let agent_runtime_options = match agent_provider_resolved.as_ref() {
            Some((ty, alias, _)) => {
                zeroclaw_providers::provider_runtime_options_for_alias(&config, ty, alias)
            }
            None => zeroclaw_providers::provider_runtime_options_for_agent(&config, agent_alias),
        };
        // Resolve every alias-owned option, including vision, through the shared
        // provider-ref resolver. This keeps a --provider override isolated from
        // the agent alias without a second capability-specific lookup.
        let provider_runtime_options = zeroclaw_providers::options_for_provider_ref(
            &config,
            &provider_name,
            &agent_runtime_options,
        );

        // Resolve api_key and uri from the actual provider being constructed.
        // For dotted aliases (e.g. "openai.shartgpt"), look up the alias-specific
        // config so a -p override does not leak the agent's current provider key
        // (e.g. an xai key) to a different provider family that doesn't expect it.
        let (initial_api_key, initial_uri) =
            api_key_and_uri_for_provider(&config, &provider_name, agent_model_provider);
        let mut model_provider: Box<dyn ModelProvider> =
            zeroclaw_providers::create_routed_model_provider_with_options(
                &config,
                &provider_name,
                initial_api_key.as_deref(),
                initial_uri.as_deref(),
                &config.reliability,
                &config.model_routes,
                &model_name,
                &provider_runtime_options,
            )?;

        let mut turn_guard = crate::observability::AgentTurnGuard::start(
            observer.as_ref(),
            provider_name.to_string(),
            model_name.to_string(),
            Some(channel_name.to_string()),
            Some(agent_alias.to_string()),
            Some(turn_id.clone()),
        );

        // ── Hardware RAG (datasheet retrieval when peripherals + datasheet_dir) ──
        let hardware_rag: Option<crate::rag::HardwareRag> = config
            .peripherals
            .datasheet_dir
            .as_ref()
            .filter(|d| !d.trim().is_empty())
            .map(|dir| crate::rag::HardwareRag::load(&config.data_dir, dir.trim()))
            .and_then(Result::ok)
            .filter(|r: &crate::rag::HardwareRag| !r.is_empty());
        if let Some(ref rag) = hardware_rag {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Load)
                    .with_category(::zeroclaw_log::EventCategory::Agent)
                    .with_attrs(::serde_json::json!({"chunks": rag.len()})),
                "Hardware RAG loaded"
            );
        }

        let board_names: Vec<String> = config
            .peripherals
            .boards
            .iter()
            .map(|b| b.board.clone())
            .collect();

        // ── Initialize locale-aware tool descriptions ──────────────────
        let _i18n_locale = config
            .locale
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .unwrap_or_else(crate::i18n::detect_locale);

        let mut tool_descs: Vec<(&str, &str)> = vec![
            (
                "shell",
                "Execute terminal commands. Use when: running local checks, build/test commands, diagnostics. Don't use when: a safer dedicated tool exists, or command is destructive without approval.",
            ),
            (
                "file_read",
                "Read file contents. Use when: inspecting project files, configs, logs. Don't use when: a targeted search is enough.",
            ),
            (
                "file_write",
                "Write file contents. Use when: applying focused edits, scaffolding files, updating docs/code. Don't use when: side effects are unclear or file ownership is uncertain.",
            ),
            (
                "memory_store",
                "Save to memory. Use when: preserving durable preferences, decisions, key context. Don't use when: information is transient/noisy/sensitive without need.",
            ),
            (
                "memory_recall",
                "Search memory. Use when: retrieving prior decisions, user preferences, historical context. Don't use when: answer is already in current context.",
            ),
            (
                "memory_forget",
                "Delete a memory entry. Use when: memory is incorrect/stale or explicitly requested for removal. Don't use when: impact is uncertain.",
            ),
        ];
        if matches!(
            eff_prompt_injection_mode,
            zeroclaw_config::schema::SkillsPromptInjectionMode::Compact
        ) {
            tool_descs.push((
            "read_skill",
            "Load the full source for an available skill by name. Use when: compact mode only shows a summary and you need the complete skill instructions.",
        ));
        }
        tool_descs.push((
        "cron_add",
        "Create a cron job. Supports schedule kinds: cron, at, every; and job types: shell or agent.",
    ));
        tool_descs.push((
            "cron_list",
            "List all cron jobs with schedule, status, and metadata.",
        ));
        tool_descs.push(("cron_remove", "Remove a cron job by job_id."));
        tool_descs.push((
        "cron_update",
        "Patch a cron job (schedule, enabled, command/prompt, model, delivery, session_target).",
    ));
        tool_descs.push((
            "cron_run",
            "Force-run a cron job immediately and record a run history entry.",
        ));
        tool_descs.push(("cron_runs", "Show recent run history for a cron job."));
        tool_descs.push((
        "screenshot",
        "Capture a screenshot of the current screen. Returns file path and base64-encoded PNG. Use when: visual verification, UI inspection, debugging displays.",
    ));
        tool_descs.push((
        "image_info",
        "Read image file metadata (format, dimensions, size) and optionally base64-encode it. Use when: inspecting images, preparing visual data for analysis.",
    ));
        if config.browser.enabled {
            tool_descs.push((
                "browser_open",
                "Open approved HTTPS URLs in system browser (allowlist-only, no scraping)",
            ));
        }
        if config.composio.enabled {
            tool_descs.push((
            "composio",
            "Execute actions on 1000+ apps via Composio (Gmail, Notion, GitHub, Slack, etc.). Use action='list' to discover, 'execute' to run (optionally with connected_account_id), 'connect' to OAuth.",
        ));
        }
        tool_descs.push((
        "schedule",
        "Manage scheduled tasks (create/list/get/cancel/pause/resume). Supports recurring cron and one-shot delays.",
    ));
        tool_descs.push((
            "channel_room",
            "Create channel rooms and invite users through active channels. Use with Matrix channel keys such as matrix.default.",
        ));
        if !config.agents.is_empty() {
            tool_descs.push((
            "delegate",
            "Delegate a sub-task to a specialized agent. Use when: task needs different model/capability, or to parallelize work.",
        ));
        }
        if config.peripherals.enabled && !config.peripherals.boards.is_empty() {
            tool_descs.push((
            "gpio_read",
            "Read GPIO pin value (0 or 1) on connected hardware (STM32, Arduino). Use when: checking sensor/button state, LED status.",
        ));
            tool_descs.push((
            "gpio_write",
            "Set GPIO pin high (1) or low (0) on connected hardware. Use when: turning LED on/off, controlling actuators.",
        ));
            tool_descs.push((
            "arduino_upload",
            "Upload agent-generated Arduino sketch. Use when: user asks for 'make a heart', 'blink pattern', or custom LED behavior on Arduino. You write the full .ino code; ZeroClaw compiles and uploads it. Pin 13 = built-in LED on Uno.",
        ));
            tool_descs.push((
            "hardware_memory_map",
            "Return flash and RAM address ranges for connected hardware. Use when: user asks for 'upper and lower memory addresses', 'memory map', or 'readable addresses'.",
        ));
            tool_descs.push((
            "hardware_board_info",
            "Return full board info (chip, architecture, memory map) for connected hardware. Use when: user asks for 'board info', 'what board do I have', 'connected hardware', 'chip info', or 'what hardware'.",
        ));
            tool_descs.push((
            "hardware_memory_read",
            "Read actual memory/register values from Nucleo via USB. Use when: user asks to 'read register values', 'read memory', 'dump lower memory 0-126', 'give address and value'. Params: address (hex, default 0x20000000), length (bytes, default 128).",
        ));
            tool_descs.push((
            "hardware_capabilities",
            "Query connected hardware for reported GPIO pins and LED pin. Use when: user asks what pins are available.",
        ));
        }
        retain_registered_tool_descriptions(&mut tool_descs, &tools_registry);
        let bootstrap_max_chars = if eff_compact_context {
            Some(6000)
        } else {
            None
        };
        let prompt_excluded_tools = message
            .as_deref()
            .map(|msg| {
                compute_excluded_mcp_tools(
                    &tools_registry,
                    &agent.resolved.tool_filter_groups,
                    msg,
                    &mcp_tool_names,
                )
            })
            .unwrap_or_default();
        let agent_workspace = config.agent_workspace_dir(agent_alias);
        let mut system_prompt = build_system_prompt_for_turn(
            &agent_workspace,
            &model_name,
            &tool_descs,
            &deferred_section,
            &skills,
            Some(&agent.identity),
            bootstrap_max_chars,
            &risk_profile,
            model_provider.as_ref(),
            &tools_registry,
            &prompt_excluded_tools,
            activated_handle.as_ref(),
            agent.resolved.strict_tool_parsing,
            eff_prompt_injection_mode,
            eff_compact_context,
            eff_max_system_prompt_chars,
            true,
            config.channels.show_tool_calls,
            persona_section.as_deref(),
            None,
        )?;

        // ── Approval manager (supervised mode) ───────────────────────
        let approval_manager = if interactive {
            Some(ApprovalManager::from_risk_profile(&risk_profile).with_store_at(&config.data_dir))
        } else {
            None
        };
        let memory_session_id = session_state_file.as_deref().and_then(|path| {
            let raw = path.to_string_lossy().trim().to_string();
            if raw.is_empty() {
                None
            } else {
                // Match the sanitized form persisted by memory backend migrations.
                Some(zeroclaw_api::session_keys::sanitize_session_key(&format!(
                    "cli:{raw}"
                )))
            }
        });

        // ── Cost tracking context (scoped for CLI / cron / web agents) ──
        let cost_tracking_context =
            crate::agent::cost::tool_loop_cost_tracking_context_for_agent(&config, agent_alias);

        // ── Execute ──────────────────────────────────────────────────
        let mut final_output = String::new();

        // Save the base system prompt before any thinking modifications so
        // the interactive loop can restore it between turns.
        let base_system_prompt = system_prompt.clone();

        if let Some(msg) = message {
            // ── Parse thinking directive from user message ─────────
            let (thinking_directive, effective_msg) =
                match crate::agent::thinking::parse_thinking_directive(&msg) {
                    Some((level, remaining)) => {
                        ::zeroclaw_log::record!(
                            INFO,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_category(::zeroclaw_log::EventCategory::Agent)
                            .with_attrs(::serde_json::json!({"thinking_level": level})),
                            "Thinking directive parsed from message"
                        );
                        (Some(level), remaining)
                    }
                    None => (None, msg.clone()),
                };
            let thinking_level = crate::agent::thinking::resolve_thinking_level(
                thinking_directive,
                None,
                &agent.resolved.thinking,
            );
            let thinking_params = crate::agent::thinking::apply_thinking_level_with_config(
                thinking_level,
                &agent.resolved.thinking,
            );
            let effective_temperature: Option<f64> = temperature.map(|t| {
                crate::agent::thinking::clamp_temperature(
                    t + thinking_params.temperature_adjustment,
                )
            });

            // Compute per-turn excluded MCP tools from tool_filter_groups before
            // building the turn prompt so tool availability matches the specs
            // sent to the provider.
            let excluded_tools = compute_excluded_mcp_tools(
                &tools_registry,
                &agent.resolved.tool_filter_groups,
                &effective_msg,
                &mcp_tool_names,
            );
            system_prompt = build_system_prompt_for_turn(
                &agent_workspace,
                &model_name,
                &tool_descs,
                &deferred_section,
                &skills,
                Some(&agent.identity),
                bootstrap_max_chars,
                &risk_profile,
                model_provider.as_ref(),
                &tools_registry,
                &excluded_tools,
                activated_handle.as_ref(),
                agent.resolved.strict_tool_parsing,
                eff_prompt_injection_mode,
                eff_compact_context,
                eff_max_system_prompt_chars,
                true,
                config.channels.show_tool_calls,
                persona_section.as_deref(),
                thinking_params.system_prompt_prefix.as_deref(),
            )?;

            let excluded_tool_names: HashSet<&str> =
                excluded_tools.iter().map(String::as_str).collect();
            let runtime_capability_names = tools_registry
                .iter()
                .map(|tool| tool.name())
                .filter(|name| !excluded_tool_names.contains(*name))
                .collect::<Vec<_>>();
            if let Some(suggestion) = crate::skills::render_missing_skill_install_suggestion(
                &effective_msg,
                &skills,
                &runtime_capability_names,
                &config.data_dir,
                &config.skills.extra_registries,
                config.skills.install_suggestions.enabled,
            ) {
                final_output = suggestion;
                if interactive {
                    println!("{final_output}");
                }
                observer.record_event(&ObserverEvent::TurnComplete);
                return Ok(final_output);
            }

            // Auto-save user message to memory (skip short/trivial messages)
            if config.memory.auto_save
                && effective_msg.chars().count() >= AUTOSAVE_MIN_MESSAGE_CHARS
                && !zeroclaw_memory::should_skip_autosave_content(&effective_msg)
            {
                let user_key = autosave_memory_key("user_msg");
                let store_start = std::time::Instant::now();
                let store_result = mem
                    .store(
                        &user_key,
                        &effective_msg,
                        MemoryCategory::Conversation,
                        memory_session_id.as_deref(),
                    )
                    .await;
                observer.record_event(&ObserverEvent::MemoryStore {
                    category: MemoryCategory::Conversation.to_string(),
                    backend: mem.name().to_string(),
                    duration: store_start.elapsed(),
                    success: store_result.is_ok(),
                    channel: Some(channel_name.to_string()),
                    agent_alias: Some(agent_alias.to_string()),
                    turn_id: Some(turn_id.clone()),
                });
            }

            // Memory context is injected once in the engine, keyed on the
            // ingress origin (agent::memory_inject). Hardware RAG context
            // stays site-built; the engine prepends the memory block above
            // it, preserving the legacy mem -> hw -> [now] msg order.
            let rag_limit = if eff_compact_context { 2 } else { 5 };
            let hw_context = hardware_rag
                .as_ref()
                .map(|r| {
                    build_hardware_context(
                        r,
                        &*observer,
                        &effective_msg,
                        &board_names,
                        rag_limit,
                        TurnMeta {
                            parent_agent_alias: None,
                            agent_alias: Some(agent_alias),
                            turn_id: &turn_id,
                            channel_name,
                        },
                    )
                })
                .unwrap_or_default();
            // Finished background children, claimed once for this turn and
            // spliced in directly above the user message — the same site-built
            // context channel hardware RAG uses, so it lands in the turn's
            // conversation history and the model can refer back to it.
            // Only when this run owns the key: see `owns_session_key`.
            //
            // The guard lives until the retry loop below has produced this
            // turn's outcome, and is settled against it there. Until then the
            // block is only in a local `history` vec, and this turn can still
            // die before the provider is called (`agent/turn/mod.rs` lines
            // 528/535/566/584 all `?` ahead of the call at :628) — in which
            // case the announcements go back to the store rather than being
            // lost with the turn.
            let (announcements, announcement_guard) =
                claim_announcements_for_turn(owns_session_key).await;
            let context = format!("{hw_context}{announcements}");
            let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %Z");
            let enriched = if context.is_empty() {
                format!("[{now}] {effective_msg}")
            } else {
                format!("{context}[{now}] {effective_msg}")
            };
            observe_turn_user_message(&enriched);

            let mut history = vec![
                ChatMessage::system(&system_prompt),
                ChatMessage::user(&enriched),
            ];

            // Compute per-turn excluded MCP tools from tool_filter_groups.
            let excluded_tools = compute_excluded_mcp_tools(
                &tools_registry,
                &agent.resolved.tool_filter_groups,
                &effective_msg,
                &mcp_tool_names,
            );

            // The retry loop yields this turn's outcome instead of settling
            // inside itself: a model-switch retry `continue`s with the same
            // history, which the model has still not read, so it must not
            // settle — and the guard is settled exactly once below.
            let turn_result: Result<String> = loop {
                if let Some(sys_msg) = history.first_mut()
                    && sys_msg.role == "system"
                {
                    sys_msg.content = build_system_prompt_for_turn(
                        &agent_workspace,
                        &model_name,
                        &tool_descs,
                        &deferred_section,
                        &skills,
                        Some(&agent.identity),
                        bootstrap_max_chars,
                        &risk_profile,
                        model_provider.as_ref(),
                        &tools_registry,
                        &excluded_tools,
                        activated_handle.as_ref(),
                        agent.resolved.strict_tool_parsing,
                        eff_prompt_injection_mode,
                        eff_compact_context,
                        eff_max_system_prompt_chars,
                        true,
                        config.channels.show_tool_calls,
                        persona_section.as_deref(),
                        thinking_params.system_prompt_prefix.as_deref(),
                    )?;
                }
                match zeroclaw_api::NATIVE_THINKING_OVERRIDE
                    .scope(
                        thinking_params.native_thinking,
                        TOOL_LOOP_COST_TRACKING_CONTEXT.scope(
                            cost_tracking_context.clone(),
                            run_tool_call_loop(ToolLoop {
                                exec: ResolvedAgentExecution::resolve(
                                    ResolvedModelAccess {
                                        model_provider: model_provider.as_ref(),
                                        provider_name: &provider_name,
                                        model: &model_name,
                                        temperature: effective_temperature,
                                    },
                                    ResolvedIo {
                                        tools_registry: &tools_registry,
                                        observer: observer.as_ref(),
                                        silent: false,
                                        approval: approval_manager.as_ref(),
                                        multimodal_config: &config.multimodal,
                                        config: Some(&config),
                                        hooks: None,
                                        activated_tools: activated_handle.as_ref(),
                                        model_switch_callback: None,
                                        receipt_generator: None,
                                    },
                                    ResolvedRuntimeKnobs {
                                        max_tool_iterations: agent.resolved.max_tool_iterations,
                                        excluded_tools: &excluded_tools,
                                        dedup_exempt_tools: &agent.resolved.tool_call_dedup_exempt,
                                        pacing: &config.pacing,
                                        strict_tool_parsing: agent.resolved.strict_tool_parsing,
                                        parallel_tools: agent.resolved.parallel_tools,
                                        max_tool_result_chars: agent.resolved.max_tool_result_chars,
                                        context_token_budget: agent
                                            .resolved
                                            .effective_context_budget(),
                                        knobs: &LoopKnobs::default(),
                                    },
                                ),
                                history: &mut history,
                                channel_name,
                                channel_reply_target: None,
                                cancellation_token: None,
                                on_delta: None,
                                shared_budget: None,
                                channel: None,
                                collected_receipts: None,
                                event_tx: None,
                                steering: None,
                                new_messages_out: None,
                                image_cache: None,
                                // Origin is threaded from the entry point;
                                // source/transport/trust stay phase-1
                                // placeholders until per-transport stamping.
                                memory: Some(crate::agent::memory_inject::TurnMemory {
                                    handle: mem.as_ref(),
                                    query: effective_msg.clone(),
                                    sessions: vec![memory_session_id.clone()],
                                    suppress: suppress_memory_inject,
                                    cfg: crate::agent::memory_inject::MemoryInjectConfig::from_memory_config(
                                        &config.memory,
                                        crate::agent::memory_inject::DEFAULT_RECALL_LIMIT,
                                    ),
                                }),
                                ingress: IngressContext::from_origin(origin),
                                agent_alias: Some(agent_alias),
                                parent_agent_alias: None,
                                turn_id: &turn_id,
                                sop_reassembly: Some(crate::agent::turn::SopStepReassembly {
                                    config: &config,
                                }),
                            }),
                        ),
                    )
                    .await
                {
                    Ok(resp) => {
                        // Success point for this turn: the tool loop only
                        // returns `Ok` after the provider call, so the model
                        // has read the history containing the announcement
                        // block.
                        break Ok(resp);
                    }
                    Err(e) => {
                        if let Some((new_model_provider, new_model)) = is_model_switch_requested(&e)
                        {
                            ::zeroclaw_log::record!(
                                INFO,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Migrate
                                )
                                .with_category(::zeroclaw_log::EventCategory::Provider),
                                &format!(
                                    "Model switch requested, switching from {} {} to {} {}",
                                    provider_name, model_name, new_model_provider, new_model
                                )
                            );

                            let (switch_api_key, switch_uri) = api_key_and_uri_for_provider(
                                &config,
                                &new_model_provider,
                                agent_model_provider,
                            );
                            model_provider =
                                zeroclaw_providers::create_routed_model_provider_with_options(
                                    &config,
                                    &new_model_provider,
                                    switch_api_key.as_deref(),
                                    switch_uri.as_deref(),
                                    &config.reliability,
                                    &config.model_routes,
                                    &new_model,
                                    &zeroclaw_providers::options_for_provider_ref(
                                        &config,
                                        &new_model_provider,
                                        &zeroclaw_providers::provider_runtime_options_for_agent(
                                            &config,
                                            agent_alias,
                                        ),
                                    ),
                                )?;

                            provider_name = new_model_provider;
                            model_name = new_model;

                            turn_guard.set_model_route(provider_name.clone(), model_name.clone());

                            continue;
                        }
                        break Err(e);
                    }
                }
            };

            // Settle this turn's claim, once, against the outcome the retry
            // loop produced. `Err` propagates exactly as the in-loop `return`
            // did, with the guard dropping armed and the announcements going
            // back to the store.
            let response = settle_announcement_guards(announcement_guard, turn_result)?;

            // After successful multi-step execution, attempt autonomous skill creation.
            if config.skills.skill_creation.enabled {
                let tool_calls = crate::skills::creator::extract_tool_calls_from_history(&history);
                if tool_calls.len() >= 2 {
                    let creator = crate::skills::creator::SkillCreator::new(
                        config.data_dir.clone(),
                        config.skills.skill_creation.clone(),
                    );
                    // Opt-in reflection synthesizes a `SKILL.md` from a bounded
                    // slice of the execution; it falls back to `SKILL.toml`
                    // internally when the provider call or its output is
                    // unusable. Default path stays the deterministic generator.
                    let creation_result = if config.skills.skill_creation.reflection_enabled {
                        TOOL_LOOP_COST_TRACKING_CONTEXT
                            .scope(
                                cost_tracking_context.clone(),
                                creator.create_from_execution_reflected(
                                    &msg,
                                    &tool_calls,
                                    &response,
                                    None,
                                    &provider_name,
                                    model_provider.as_ref(),
                                    &model_name,
                                ),
                            )
                            .await
                    } else {
                        creator.create_from_execution(&msg, &tool_calls, None).await
                    };
                    match creation_result {
                        Ok(Some(slug)) => {
                            ::zeroclaw_log::record!(
                                INFO,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Register
                                )
                                .with_category(::zeroclaw_log::EventCategory::Agent)
                                .with_attrs(::serde_json::json!({"slug": slug})),
                                "Auto-created skill from execution"
                            );
                        }
                        Ok(None) => {
                            ::zeroclaw_log::record!(
                                DEBUG,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Skip
                                )
                                .with_category(::zeroclaw_log::EventCategory::Agent),
                                "Skill creation skipped (duplicate or disabled)"
                            );
                        }
                        Err(e) => ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Fail
                            )
                            .with_category(::zeroclaw_log::EventCategory::Agent)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                            "Skill creation failed"
                        ),
                    }
                }
            }
            // Emit the user-visible response before any background work so the
            // skill-review fork can never delay the user's answer.
            final_output = response;
            if interactive {
                println!("{final_output}");
            }
            observer.record_event(&ObserverEvent::TurnComplete);

            if config.skills.skill_improvement.enabled {
                let review_workspace = config.agent_workspace_dir(agent_alias);
                let review_config = config.skills.skill_improvement.clone();
                let failed_slugs: Vec<String> =
                    crate::skills::improver::extract_skill_executions_from_history(&history)
                        .into_iter()
                        .filter_map(|(slug, ok)| if ok { None } else { Some(slug) })
                        .collect();
                TOOL_LOOP_COST_TRACKING_CONTEXT
                    .scope(
                        cost_tracking_context.clone(),
                        crate::skills::review::maybe_run_skill_review(
                            Some(&config),
                            review_workspace,
                            review_config,
                            config.skills.allow_scripts,
                            history.clone(),
                            failed_slugs,
                            model_provider.as_ref(),
                            &provider_name,
                            &model_name,
                            observer.as_ref(),
                            &config.multimodal,
                            &config.pacing,
                            agent.resolved.max_tool_result_chars,
                            agent.resolved.max_context_tokens,
                            None, // cancellation_token — no parent token in single-shot run
                            Some(agent_alias),
                        ),
                    )
                    .await;
            }
        } else {
            println!("🦀 ZeroClaw Interactive Mode");
            println!("Type /help for commands.\n");
            let cli = CLI_CHANNEL_FN.get().expect(
                "CLI channel factory not registered — call register_cli_channel_fn at startup",
            )();

            // Persistent conversation history across turns
            let mut history = if let Some(path) = session_state_file.as_deref() {
                load_interactive_session_history(path, &system_prompt)?
            } else {
                vec![ChatMessage::system(&system_prompt)]
            };

            loop {
                print!("> ");
                let _ = std::io::stdout().flush();

                let input = {
                    let stdin = std::io::stdin().lock();
                    match read_capped_line(stdin, MAX_INTERACTIVE_INPUT_BYTES) {
                        Ok(CappedLine::Eof) => break,
                        Ok(CappedLine::Line(s)) => s,
                        Ok(CappedLine::Truncated) => {
                            eprintln!(
                                "\nWarning: input line exceeds {} bytes and was discarded.",
                                MAX_INTERACTIVE_INPUT_BYTES
                            );
                            continue;
                        }
                        Err(e) => {
                            eprintln!("\nError reading input: {e}\n");
                            break;
                        }
                    }
                };

                let user_input = input.trim().to_string();
                if user_input.is_empty() {
                    continue;
                }
                match user_input.as_str() {
                    "/quit" | "/exit" => break,
                    "/help" => {
                        println!("Available commands:");
                        println!("  /help             Show this help message");
                        println!("  /clear /new       Clear conversation history");
                        println!("  /quit /exit       Exit interactive mode");
                        println!(
                            "  /think:<level>    Set reasoning depth (off|minimal|low|medium|high|max)\n"
                        );
                        continue;
                    }
                    "/clear" | "/new" => {
                        println!(
                            "This will clear the current conversation and delete all session memory."
                        );
                        println!("Core memories (long-term facts/preferences) will be preserved.");
                        print!("Continue? [y/N] ");
                        let _ = std::io::stdout().flush();

                        let confirm = {
                            let stdin = std::io::stdin().lock();
                            match read_capped_line(stdin, MAX_INTERACTIVE_INPUT_BYTES) {
                                Ok(CappedLine::Line(s)) => s,
                                Ok(CappedLine::Truncated) | Ok(CappedLine::Eof) | Err(_) => {
                                    println!("Cancelled.\n");
                                    continue;
                                }
                            }
                        };
                        if !matches!(confirm.trim().to_lowercase().as_str(), "y" | "yes") {
                            println!("Cancelled.\n");
                            continue;
                        }

                        history.clear();
                        history.push(ChatMessage::system(&system_prompt));
                        // Clear conversation and daily memory
                        let mut cleared = 0;
                        for category in [MemoryCategory::Conversation, MemoryCategory::Daily] {
                            let entries = mem.list(Some(&category), None).await.unwrap_or_default();
                            for entry in entries {
                                if mem.forget(&entry.key).await.unwrap_or(false) {
                                    cleared += 1;
                                }
                            }
                        }
                        if cleared > 0 {
                            println!("Conversation cleared ({cleared} memory entries removed).\n");
                        } else {
                            println!("Conversation cleared.\n");
                        }
                        if let Some(path) = session_state_file.as_deref() {
                            save_interactive_session_history(path, &history)?;
                        }
                        continue;
                    }
                    _ => {}
                }

                // ── Parse thinking directive from interactive input ───
                let (thinking_directive, effective_input) =
                    match crate::agent::thinking::parse_thinking_directive(&user_input) {
                        Some((level, remaining)) => {
                            ::zeroclaw_log::record!(
                                INFO,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Note
                                )
                                .with_category(::zeroclaw_log::EventCategory::Agent)
                                .with_attrs(::serde_json::json!({"thinking_level": level})),
                                "Thinking directive parsed"
                            );
                            (Some(level), remaining)
                        }
                        None => (None, user_input.clone()),
                    };
                let thinking_level = crate::agent::thinking::resolve_thinking_level(
                    thinking_directive,
                    None,
                    &agent.resolved.thinking,
                );
                let thinking_params = crate::agent::thinking::apply_thinking_level_with_config(
                    thinking_level,
                    &agent.resolved.thinking,
                );
                let turn_temperature: Option<f64> = temperature.map(|t| {
                    crate::agent::thinking::clamp_temperature(
                        t + thinking_params.temperature_adjustment,
                    )
                });

                // Compute per-turn excluded MCP tools from tool_filter_groups
                // before the provider call; the system prompt is rebuilt from
                // this same set immediately before each attempt.
                let excluded_tools = compute_excluded_mcp_tools(
                    &tools_registry,
                    &agent.resolved.tool_filter_groups,
                    &effective_input,
                    &mcp_tool_names,
                );

                let excluded_tool_names: HashSet<&str> =
                    excluded_tools.iter().map(String::as_str).collect();
                let runtime_capability_names = tools_registry
                    .iter()
                    .map(|tool| tool.name())
                    .filter(|name| !excluded_tool_names.contains(*name))
                    .collect::<Vec<_>>();
                if let Some(suggestion) = crate::skills::render_missing_skill_install_suggestion(
                    &effective_input,
                    &skills,
                    &runtime_capability_names,
                    &config.data_dir,
                    &config.skills.extra_registries,
                    config.skills.install_suggestions.enabled,
                ) {
                    final_output = suggestion;
                    if let Err(e) = zeroclaw_api::channel::Channel::send(
                        &*cli,
                        &zeroclaw_api::channel::SendMessage::new(
                            format!("\n{final_output}\n"),
                            "user",
                        ),
                    )
                    .await
                    {
                        eprintln!("\nError sending CLI response: {e}\n");
                    }
                    observer.record_event(&ObserverEvent::TurnComplete);
                    if let Some(sys_msg) = history.first_mut()
                        && sys_msg.role == "system"
                    {
                        sys_msg.content.clone_from(&base_system_prompt);
                    }
                    continue;
                }

                // Auto-save conversation turns (skip short/trivial messages)
                if config.memory.auto_save
                    && effective_input.chars().count() >= AUTOSAVE_MIN_MESSAGE_CHARS
                    && !zeroclaw_memory::should_skip_autosave_content(&effective_input)
                {
                    let user_key = autosave_memory_key("user_msg");
                    let store_start = std::time::Instant::now();
                    let store_result = mem
                        .store(
                            &user_key,
                            &effective_input,
                            MemoryCategory::Conversation,
                            memory_session_id.as_deref(),
                        )
                        .await;
                    observer.record_event(&ObserverEvent::MemoryStore {
                        category: MemoryCategory::Conversation.to_string(),
                        backend: mem.name().to_string(),
                        duration: store_start.elapsed(),
                        success: store_result.is_ok(),
                        channel: Some(channel_name.to_string()),
                        agent_alias: Some(agent_alias.to_string()),
                        turn_id: Some(turn_id.clone()),
                    });
                }

                // Memory context is injected once in the engine, keyed on
                // the ingress origin (agent::memory_inject). Hardware RAG
                // stays site-built; the engine prepends the memory block
                // above it.
                let rag_limit = if eff_compact_context { 2 } else { 5 };
                let hw_context = hardware_rag
                    .as_ref()
                    .map(|r| {
                        build_hardware_context(
                            r,
                            &*observer,
                            &effective_input,
                            &board_names,
                            rag_limit,
                            TurnMeta {
                                parent_agent_alias: None,
                                agent_alias: Some(agent_alias),
                                turn_id: &turn_id,
                                channel_name,
                            },
                        )
                    })
                    .unwrap_or_default();
                // One claim per interactive turn (this is the per-turn body;
                // the prompt rebuilds below only touch the system message),
                // and only when this run owns the key: see `owns_session_key`.
                // The guard is settled against the outcome the retry loop
                // below yields; a turn that dies before the provider call
                // hands its announcements back for the next `>` prompt.
                let (announcements, announcement_guard) =
                    claim_announcements_for_turn(owns_session_key).await;
                let context = format!("{hw_context}{announcements}");
                let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %Z");
                let enriched = if context.is_empty() {
                    format!("[{now}] {effective_input}")
                } else {
                    format!("{context}[{now}] {effective_input}")
                };
                observe_turn_user_message(&enriched);

                history.push(ChatMessage::user(&enriched));

                // Set up streaming channel so tool progress and response
                // content are printed progressively instead of buffered.
                let (delta_tx, mut delta_rx) = tokio::sync::mpsc::channel::<DraftEvent>(64);
                let content_was_streamed =
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let content_streamed_flag = content_was_streamed.clone();
                let is_tty = std::io::IsTerminal::is_terminal(&std::io::stderr());

                let consumer_handle = zeroclaw_spawn::spawn!(async move {
                    use std::io::Write;
                    while let Some(event) = delta_rx.recv().await {
                        match event {
                            StreamDelta::Status(text) => {
                                if is_tty {
                                    let _ = write!(std::io::stderr(), "\x1b[2m{text}\x1b[0m");
                                } else {
                                    let _ = write!(std::io::stderr(), "{text}");
                                }
                                let _ = std::io::stderr().flush();
                            }
                            StreamDelta::Text(text) => {
                                content_streamed_flag
                                    .store(true, std::sync::atomic::Ordering::Relaxed);
                                print!("{text}");
                                let _ = std::io::stdout().flush();
                            }
                        }
                    }
                });

                // Ctrl+C cancels the in-flight turn instead of killing the process.
                let cancel_token = CancellationToken::new();
                let cancel_token_clone = cancel_token.clone();
                let ctrlc_handle = zeroclaw_spawn::spawn!(async move {
                    if tokio::signal::ctrl_c().await.is_ok() {
                        cancel_token_clone.cancel();
                    }
                });

                // The loop yields this turn's outcome rather than settling
                // inside itself: a model-switch or context-trim retry
                // `continue`s with the same history, which the model has still
                // not read. `Err` carries the text a failed turn still prints
                // (empty on every failure path here), so the printing below is
                // unchanged by which arm produced it.
                let turn_outcome: Result<String, String> = loop {
                    if let Some(sys_msg) = history.first_mut()
                        && sys_msg.role == "system"
                    {
                        sys_msg.content = build_system_prompt_for_turn(
                            &agent_workspace,
                            &model_name,
                            &tool_descs,
                            &deferred_section,
                            &skills,
                            Some(&agent.identity),
                            bootstrap_max_chars,
                            &risk_profile,
                            model_provider.as_ref(),
                            &tools_registry,
                            &excluded_tools,
                            activated_handle.as_ref(),
                            agent.resolved.strict_tool_parsing,
                            eff_prompt_injection_mode,
                            eff_compact_context,
                            eff_max_system_prompt_chars,
                            true,
                            config.channels.show_tool_calls,
                            persona_section.as_deref(),
                            thinking_params.system_prompt_prefix.as_deref(),
                        )?;
                    }
                    match zeroclaw_api::NATIVE_THINKING_OVERRIDE
                        .scope(
                            thinking_params.native_thinking,
                            TOOL_LOOP_COST_TRACKING_CONTEXT.scope(
                                cost_tracking_context.clone(),
                                run_tool_call_loop(ToolLoop {
                                    exec: ResolvedAgentExecution::resolve(
                                        ResolvedModelAccess {
                                            model_provider: model_provider.as_ref(),
                                            provider_name: &provider_name,
                                            model: &model_name,
                                            temperature: turn_temperature,
                                        },
                                        ResolvedIo {
                                            tools_registry: &tools_registry,
                                            observer: observer.as_ref(),
                                            silent: true,
                                            approval: approval_manager.as_ref(),
                                            multimodal_config: &config.multimodal,
                                            config: Some(&config),
                                            hooks: None,
                                            activated_tools: activated_handle.as_ref(),
                                            model_switch_callback: None,
                                            receipt_generator: None,
                                        },
                                        ResolvedRuntimeKnobs {
                                            max_tool_iterations: agent.resolved.max_tool_iterations,
                                            excluded_tools: &excluded_tools,
                                            dedup_exempt_tools: &agent
                                                .resolved
                                                .tool_call_dedup_exempt,
                                            pacing: &config.pacing,
                                            strict_tool_parsing: agent.resolved.strict_tool_parsing,
                                            parallel_tools: agent.resolved.parallel_tools,
                                            max_tool_result_chars: agent
                                                .resolved
                                                .max_tool_result_chars,
                                            context_token_budget: agent
                                                .resolved
                                                .effective_context_budget(),
                                            knobs: &LoopKnobs::default(),
                                        },
                                    ),
                                    history: &mut history,
                                    channel_name,
                                    channel_reply_target: None,
                                    cancellation_token: Some(cancel_token.clone()),
                                    on_delta: Some(delta_tx.clone()),
                                    shared_budget: None,
                                    channel: None,
                                    collected_receipts: None,
                                    event_tx: None,
                                    steering: None,
                                    new_messages_out: None,
                                    image_cache: None,
                                    // Origin is threaded from the entry point;
                                    // source/transport/trust stay phase-1
                                    // placeholders until per-transport stamping.
                                    memory: Some(crate::agent::memory_inject::TurnMemory {
                                        handle: mem.as_ref(),
                                        query: effective_input.clone(),
                                        sessions: vec![memory_session_id.clone()],
                                        suppress: suppress_memory_inject,
                                        cfg: crate::agent::memory_inject::MemoryInjectConfig::from_memory_config(
                                            &config.memory,
                                            crate::agent::memory_inject::DEFAULT_RECALL_LIMIT,
                                        ),
                                    }),
                                    ingress: IngressContext::from_origin(origin),
                                    agent_alias: Some(agent_alias),
                                    parent_agent_alias: None,
                                    turn_id: &turn_id,
                                    sop_reassembly: Some(crate::agent::turn::SopStepReassembly {
                                        config: &config,
                                    }),
                                }),
                            ),
                        )
                        .await
                    {
                        Ok(resp) => {
                            // Success point: the tool loop returns `Ok` only
                            // after the provider call, so the model has read
                            // this turn's history.
                            break Ok(resp);
                        }
                        Err(e) => {
                            if is_tool_loop_cancelled(&e) {
                                eprintln!("\n\x1b[2m(cancelled)\x1b[0m");
                                // Deliberately settled as a failure: a Ctrl+C
                                // can land before the provider call as easily
                                // as after, and re-announcing to the next
                                // prompt is the recoverable side of that
                                // ambiguity.
                                break Err(String::new());
                            }
                            if let Some((new_model_provider, new_model)) =
                                is_model_switch_requested(&e)
                            {
                                ::zeroclaw_log::record!(
                                    INFO,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Migrate
                                    )
                                    .with_category(::zeroclaw_log::EventCategory::Provider),
                                    &format!(
                                        "Model switch requested, switching from {} {} to {} {}",
                                        provider_name, model_name, new_model_provider, new_model
                                    )
                                );

                                let (switch_api_key2, switch_uri2) = api_key_and_uri_for_provider(
                                    &config,
                                    &new_model_provider,
                                    agent_model_provider,
                                );
                                model_provider =
                                    zeroclaw_providers::create_routed_model_provider_with_options(
                                        &config,
                                        &new_model_provider,
                                        switch_api_key2.as_deref(),
                                        switch_uri2.as_deref(),
                                        &config.reliability,
                                        &config.model_routes,
                                        &new_model,
                                        &zeroclaw_providers::options_for_provider_ref(
                                            &config,
                                            &new_model_provider,
                                            &zeroclaw_providers::provider_runtime_options_for_agent(
                                                &config,
                                                agent_alias,
                                            ),
                                        ),
                                    )?;

                                provider_name = new_model_provider;
                                model_name = new_model;

                                turn_guard
                                    .set_model_route(provider_name.clone(), model_name.clone());

                                continue;
                            }
                            // Context overflow recovery: drop oldest whole
                            // turns and retry. No summarization, no splicing.
                            if zeroclaw_providers::reliable::is_context_window_exceeded(&e) {
                                ::zeroclaw_log::record!(
                                    WARN,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Retry
                                    )
                                    .with_category(::zeroclaw_log::EventCategory::Agent),
                                    "Context overflow in interactive loop, attempting recovery"
                                );
                                let taken = std::mem::take(&mut history);
                                let recovery_budget = eff_model_context_window * 9 / 10;
                                let result = crate::agent::history_trim::trim_to_recent_turns(
                                    taken,
                                    recovery_budget,
                                );
                                if result.trimmed {
                                    let mut trimmed = result.history;
                                    let system_count =
                                        trimmed.iter().take_while(|m| m.role == "system").count();
                                    trimmed.insert(
                                        system_count,
                                        crate::agent::history_trim::breadcrumb(),
                                    );
                                    history = trimmed;
                                    {
                                        let __zc_trim_span = ::zeroclaw_log::info_span!(
                                            target: "zeroclaw_log_internal_scope",
                                            "zeroclaw_scope",
                                            model = %model_name,
                                            model_provider = %provider_name,
                                        );
                                        let _zc_trim_guard = __zc_trim_span.entered();
                                        ::zeroclaw_log::record!(
                                            INFO,
                                            ::zeroclaw_log::Event::new(
                                                module_path!(),
                                                ::zeroclaw_log::Action::Retry
                                            )
                                            .with_category(::zeroclaw_log::EventCategory::Agent)
                                            .with_outcome(::zeroclaw_log::EventOutcome::Success)
                                            .with_attrs(::serde_json::json!({
                                                "dropped_messages": result.dropped_messages,
                                                "dropped_turns": result.dropped_turns,
                                                "kept_turns": result.kept_turns,
                                            })),
                                            "Context recovered via whole-turn trim, retrying turn"
                                        );
                                    }
                                    continue;
                                }
                                history = result.history;
                                let system_floor =
                                    crate::agent::history::estimate_system_floor_tokens(&history);
                                let context_token_budget =
                                    agent.resolved.effective_context_budget();
                                let floor_exceeds_budget = system_floor >= context_token_budget;
                                {
                                    let __zc_trim_span = ::zeroclaw_log::info_span!(
                                        target: "zeroclaw_log_internal_scope",
                                        "zeroclaw_scope",
                                        model = %model_name,
                                        model_provider = %provider_name,
                                    );
                                    let _zc_trim_guard = __zc_trim_span.entered();
                                    if floor_exceeds_budget {
                                        ::zeroclaw_log::record!(
                                            WARN,
                                            ::zeroclaw_log::Event::new(
                                                module_path!(),
                                                ::zeroclaw_log::Action::Fail
                                            )
                                            .with_category(::zeroclaw_log::EventCategory::Agent)
                                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                                            .with_attrs(::serde_json::json!({
                                                "system_floor": system_floor,
                                                "budget": context_token_budget,
                                                "error_key": "context_floor_exceeds_budget",
                                            })),
                                            crate::agent::history::context_floor_remediation(
                                                system_floor,
                                                context_token_budget,
                                            )
                                        );
                                    } else {
                                        ::zeroclaw_log::record!(
                                            WARN,
                                            ::zeroclaw_log::Event::new(
                                                module_path!(),
                                                ::zeroclaw_log::Action::Fail
                                            )
                                            .with_category(::zeroclaw_log::EventCategory::Agent)
                                            .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                                            "Context overflow but only one turn remains; cannot trim further"
                                        );
                                    }
                                }

                                if floor_exceeds_budget {
                                    eprintln!(
                                        "\nError: {e}\n{}\n",
                                        crate::agent::history::context_floor_remediation(
                                            system_floor,
                                            context_token_budget,
                                        )
                                    );
                                    break Err(String::new());
                                }
                            }

                            eprintln!("\nError: {e}\n");
                            break Err(String::new());
                        }
                    }
                };

                // Settle this turn's claim, once, outside the retry loop.
                let response = settle_announcement_guards(announcement_guard, turn_outcome)
                    .unwrap_or_else(|failed_turn_output| failed_turn_output);

                // Clean up: stop the Ctrl+C listener and flush streaming events.
                ctrlc_handle.abort();
                drop(delta_tx);
                let _ = consumer_handle.await;

                final_output = response;
                if content_was_streamed.load(std::sync::atomic::Ordering::Relaxed) {
                    println!();
                } else if let Err(e) = zeroclaw_api::channel::Channel::send(
                    &*cli,
                    &zeroclaw_api::channel::SendMessage::new(format!("\n{final_output}\n"), "user"),
                )
                .await
                {
                    eprintln!("\nError sending CLI response: {e}\n");
                }
                observer.record_event(&ObserverEvent::TurnComplete);

                // Display context usage for this turn.
                if let Some(ref ctx) = cost_tracking_context {
                    let usage = ctx.snapshot_turn_usage();
                    let effective_input_tokens = usage.last_input_tokens;
                    if effective_input_tokens > 0 || usage.output_tokens > 0 {
                        let max_ctx = eff_model_context_window as u64;
                        let pct = if max_ctx > 0 {
                            (effective_input_tokens as f64 / max_ctx as f64 * 100.0).min(100.0)
                        } else {
                            0.0
                        };
                        let bar_width: usize = 16;
                        let filled = ((pct / 100.0) * bar_width as f64).round() as usize;
                        let empty = bar_width.saturating_sub(filled);
                        let bar = format!(
                            "[{}{}]",
                            "\u{2588}".repeat(filled),
                            "\u{2591}".repeat(empty)
                        );
                        let msg = if effective_input_tokens > 0 {
                            crate::i18n::get_required_cli_string_with_args(
                                "cli-agent-context-bar",
                                &[
                                    ("used", format_tokens(effective_input_tokens).as_str()),
                                    ("max", format_tokens(max_ctx).as_str()),
                                    ("bar", &bar),
                                    ("pct", format!("{:.0}", pct).as_str()),
                                ],
                            )
                        } else {
                            crate::i18n::get_required_cli_string_with_args(
                                "cli-agent-context-bar-unknown",
                                &[("max", format_tokens(max_ctx).as_str())],
                            )
                        };
                        eprintln!("\x1b[2m{}\x1b[0m", msg);
                    }
                }

                // Hard cap as a safety net.
                trim_history(&mut history, eff_max_history_messages);

                // Restore base system prompt after the per-turn tool framing
                // and optional thinking prefix have been applied.
                if let Some(sys_msg) = history.first_mut()
                    && sys_msg.role == "system"
                {
                    sys_msg.content.clone_from(&base_system_prompt);
                }

                if let Some(path) = session_state_file.as_deref() {
                    save_interactive_session_history(path, &history)?;
                }
            }
        }

        let tokens_used = cost_tracking_context.as_ref().and_then(|ctx| {
            let usage = ctx.snapshot_turn_usage();
            (usage.input_tokens > 0 || usage.output_tokens > 0).then_some(
                zeroclaw_api::observability_traits::TurnTokenUsage {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                },
            )
        });
        turn_guard.set_model_route(provider_name.clone(), model_name.clone());
        turn_guard.set_usage(tokens_used, None);
        turn_guard.finish();

        Ok(final_output)
    };
    let __zc_instrumented = __zc_body
        .instrument(__zc_scope_span)
        .instrument(__zc_attribution_span);
    if __zc_session_key_scoped {
        // A caller already named this conversation; leave it alone.
        __zc_instrumented.await
    } else {
        scope_session_key(Some(__zc_synthetic_session_key), __zc_instrumented).await
    }
}

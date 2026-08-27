//! Channel/gateway `process_message` entry point for the agent loop.
//!
//! Extracted from `loop_/mod.rs` so the interactive CLI `run` path and the
//! inbound message path can evolve independently.

use crate::agent::TurnMeta;
use crate::approval::ApprovalManager;
use crate::observability::{self, Observer};
use crate::platform;
use crate::security::{AutonomyLevel, SecurityPolicy};
use crate::tools;
use crate::tools::scoped;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::sync::Arc;
use zeroclaw_api::ingress::TurnOrigin;
use zeroclaw_config::schema::Config;
use zeroclaw_memory::{self, Memory};
use zeroclaw_providers::{ChatMessage, ModelProvider};

use super::{
    agent_turn_with_sop_reassembly, apply_text_tool_prompt_policy, build_hardware_context,
    build_tool_instructions_for_names, claim_announcements_for_turn, compute_excluded_mcp_tools,
    live_channel_registry, native_tool_specs_present_for_turn, observe_turn_user_message,
    resolved_agent_for_turn, seed_channel_handles, settle_announcement_guards,
};
use crate::agent::turn::SopStepReassembly;

/// Process a single message through the full agent (with tools, peripherals, memory).
/// Used by channels (Telegram, Discord, etc.) to enable hardware and tool use.
pub async fn process_message(
    config: Config,
    agent_alias: &str,
    message: &str,
    session_id: Option<&str>,
    origin: TurnOrigin,
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
    let __zc_alias = agent_alias.to_string();
    let __zc_message = message.to_string();
    let __zc_session_id = session_id.map(str::to_string);
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
        let message: &str = __zc_message.as_str();
        let session_id: Option<&str> = __zc_session_id.as_deref();

        // ── Effective per-agent runtime tunables ──────────────────────
        // Profile values (when set) override the agent's inline fields.
        // See `Config::resolved_agent_config` for precedence rules.
        let eff_compact_context = agent.resolved.compact_context;
        let eff_max_system_prompt_chars = agent.resolved.max_system_prompt_chars;
        let eff_prompt_injection_mode = agent.resolved.prompt_injection_mode;

        let observer: Arc<dyn Observer> =
            Arc::from(observability::create_observer(&config.observability));
        let runtime: Arc<dyn platform::RuntimeAdapter> =
            Arc::from(platform::create_runtime(&config.runtime)?);
        let security = Arc::new(SecurityPolicy::for_agent(&config, agent_alias)?);
        let (provider_name, provider_alias, agent_model_provider) = match config
            .resolved_model_provider_for_agent(agent_alias)
        {
            Some(resolved) => (resolved.0, resolved.1.to_string(), Some(resolved.2.clone())),
            None => {
                let agent_ref = agent.model_provider.as_str();
                if !agent_ref.is_empty() {
                    anyhow::bail!(
                        "agents.{agent_alias}.model_provider = \"{agent_ref}\" does not resolve to \
                     a configured [providers.models.<type>.<alias>] entry"
                    );
                }
                anyhow::bail!(
                    "agents.{agent_alias}.model_provider is empty \u{2014} set it to a configured \
                 \"<type>.<alias>\" (e.g. \"anthropic.{agent_alias}\")"
                );
            }
        };
        let approval_manager =
            ApprovalManager::for_non_interactive(&risk_profile).with_store_at(&config.data_dir);
        let mem: Arc<dyn Memory> = zeroclaw_memory::create_memory_for_agent(
            &config,
            agent_alias,
            agent_model_provider
                .as_ref()
                .and_then(|e| e.api_key.as_deref()),
        )
        .await?;

        let (composio_key, composio_entity_id) = if config.composio.enabled {
            (
                config.composio.api_key.as_deref(),
                Some(config.composio.entity_id.as_str()),
            )
        } else {
            (None, None)
        };

        let all_tools_result_pm = tools::all_tools_with_runtime(
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
            agent_model_provider
                .as_ref()
                .and_then(|e| e.api_key.as_deref()),
            &config,
            None,
            false,
            None,
            None,
            // Gateway/WS and ACP turn seeding is a top-level origin: the
            // turn's own lineage root, no inherited one.
            None,
        );
        let skills = crate::skills::load_skills_for_agent_from_config(&config, agent_alias);
        let assembled = scoped::ScopedToolRegistry::assemble(scoped::ScopedAssembly {
            config: &config,
            agent_alias,
            security: &security,
            built: all_tools_result_pm,
            skills: &skills,
            runtime: runtime.clone(),
            caller_allowed: None,
            connect_mcp: true,
            connect_peripherals: true,
            exclude_memory: false,
            list_deferred_mcp_specs: false,
            emit_assembly_logs: true,
            // `process_message` is the channel/orchestrator live-chat path;
            // it has no cross-turn reuse contract, so the per-call
            // `connect_all` path inside `assemble` is the correct choice.
            // The daemon heartbeat worker — the only caller that has a
            // reuse contract — passes its own `mcp_registry` through
            // `agent::run` (`AgentRunOverrides::mcp_registry`).
            mcp_registry: None,
        })
        .await;
        // process_message injects one combined MCP prompt block: deferred tool-search
        // listing + pinned resources, composed by the harness. `mut` because the
        // text-tool prompt policy below may clear it for a non-native strict target.
        let mut deferred_section = assembled.combined_mcp_prompt_section();
        let scoped::ScopedAssembled {
            registry,
            delegate_handle: _,
            ask_user_handle,
            reaction_handle,
            poll_handle,
            escalate_handle,
            channel_room_handle,
            activated_handle: activated_handle_pm,
            mcp_tool_names: mcp_tool_names_pm,
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
                &format!("Registered {} channel(s) for process_message agent", count),
            );
        }

        let model_name = match agent_model_provider
            .as_ref()
            .and_then(|e| e.model.as_deref())
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            Some(m) => m.to_string(),
            None => anyhow::bail!(
                "agents.{agent_alias}.model_provider resolves to a model_provider entry with no \
             `model` set. Configure [providers.models.{provider_name}.<alias>] model = \"...\"."
            ),
        };
        let provider_runtime_options = zeroclaw_providers::provider_runtime_options_for_alias(
            &config,
            provider_name,
            provider_alias.as_str(),
        );
        let model_provider: Box<dyn ModelProvider> =
            zeroclaw_providers::create_routed_model_provider_with_options(
                &config,
                &format!("{provider_name}.{provider_alias}"),
                agent_model_provider
                    .as_ref()
                    .and_then(|e| e.api_key.as_deref()),
                agent_model_provider.as_ref().and_then(|e| e.uri.as_deref()),
                &config.reliability,
                &config.model_routes,
                &model_name,
                &provider_runtime_options,
            )?;

        let hardware_rag: Option<crate::rag::HardwareRag> = config
            .peripherals
            .datasheet_dir
            .as_ref()
            .filter(|d| !d.trim().is_empty())
            .map(|dir| crate::rag::HardwareRag::load(&config.data_dir, dir.trim()))
            .and_then(Result::ok)
            .filter(|r: &crate::rag::HardwareRag| !r.is_empty());
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
            ("shell", "Execute terminal commands."),
            ("file_read", "Read file contents."),
            ("file_write", "Write file contents."),
            ("memory_store", "Save to memory."),
            ("memory_recall", "Search memory."),
            ("memory_forget", "Delete a memory entry."),
            ("screenshot", "Capture a screenshot."),
            ("image_info", "Read image metadata."),
        ];
        if matches!(
            eff_prompt_injection_mode,
            zeroclaw_config::schema::SkillsPromptInjectionMode::Compact
        ) {
            tool_descs.push((
                "read_skill",
                "Load the full source for an available skill by name.",
            ));
        }
        if config.browser.enabled {
            tool_descs.push(("browser_open", "Open approved URLs in browser."));
        }
        if config.composio.enabled {
            tool_descs.push(("composio", "Execute actions on 1000+ apps via Composio."));
        }
        tool_descs.push((
            "channel_room",
            "Create channel rooms and invite users through active channels.",
        ));
        if config.peripherals.enabled && !config.peripherals.boards.is_empty() {
            tool_descs.push(("gpio_read", "Read GPIO pin value on connected hardware."));
            tool_descs.push((
                "gpio_write",
                "Set GPIO pin high or low on connected hardware.",
            ));
            tool_descs.push((
            "arduino_upload",
            "Upload Arduino sketch. Use for 'make a heart', custom patterns. You write full .ino code; ZeroClaw uploads it.",
        ));
            tool_descs.push((
            "hardware_memory_map",
            "Return flash and RAM address ranges. Use when user asks for memory addresses or memory map.",
        ));
            tool_descs.push((
            "hardware_board_info",
            "Return full board info (chip, architecture, memory map). Use when user asks for board info, what board, connected hardware, or chip info.",
        ));
            tool_descs.push((
            "hardware_memory_read",
            "Read actual memory/register values from Nucleo. Use when user asks to read registers, read memory, dump lower memory 0-126, or give address and value.",
        ));
            tool_descs.push((
            "hardware_capabilities",
            "Query connected hardware for reported GPIO pins and LED pin. Use when user asks what pins are available.",
        ));
        }

        let effective_message_for_filter =
            crate::agent::thinking::strip_thinking_directive(message);
        let mut excluded_tools = compute_excluded_mcp_tools(
            &tools_registry,
            &agent.resolved.tool_filter_groups,
            effective_message_for_filter.as_ref(),
            &mcp_tool_names_pm,
        );
        {
            let active_profile = &risk_profile;
            if active_profile.level != AutonomyLevel::Full {
                excluded_tools.extend(active_profile.excluded_tools.iter().cloned());
            }
        }

        // Filter tool descriptions to match the effective set.
        tool_descs.retain(|(name, _)| !excluded_tools.iter().any(|ex| ex == name));

        // Derive effective tool names from the filtered set so prompt builders
        // and channel target guards see the correct state.
        let effective_tool_names: HashSet<&str> = tools_registry
            .iter()
            .map(|tool| tool.name())
            .filter(|name| !excluded_tools.iter().any(|ex| ex == *name))
            .collect();
        tool_descs.retain(|(name, _)| effective_tool_names.contains(name));

        let bootstrap_max_chars = if eff_compact_context {
            Some(6000)
        } else {
            None
        };
        let native_tools = model_provider.supports_native_tools();
        let native_tool_specs_present = native_tool_specs_present_for_turn(
            model_provider.as_ref(),
            &tools_registry,
            &excluded_tools,
            activated_handle_pm.as_ref(),
        )?;
        let expose_text_tool_protocol = apply_text_tool_prompt_policy(
            native_tools,
            agent.resolved.strict_tool_parsing,
            &mut tool_descs,
            &mut deferred_section,
        );
        let agent_workspace = config.agent_workspace_dir(agent_alias);
        let mut system_prompt = crate::agent::system_prompt::build_system_prompt_with_persona(
            &agent_workspace,
            &model_name,
            &tool_descs,
            &skills,
            Some(&agent.identity),
            bootstrap_max_chars,
            Some(&risk_profile),
            native_tool_specs_present,
            eff_prompt_injection_mode,
            eff_compact_context,
            eff_max_system_prompt_chars,
            false,
            config.channels.show_tool_calls,
            persona_section.as_deref(),
        );
        if expose_text_tool_protocol {
            system_prompt.push_str(&build_tool_instructions_for_names(
                &tools_registry,
                &effective_tool_names,
            ));
        }
        if !deferred_section.is_empty() {
            system_prompt.push('\n');
            system_prompt.push_str(&deferred_section);
        }

        // ── Parse thinking directive from user message ─────────────
        let (thinking_directive, effective_message) =
            match crate::agent::thinking::parse_thinking_directive(message) {
                Some((level, remaining)) => {
                    ::zeroclaw_log::record!(
                        INFO,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_category(::zeroclaw_log::EventCategory::Agent)
                            .with_attrs(::serde_json::json!({"thinking_level": level})),
                        "Thinking directive parsed from message"
                    );
                    (Some(level), remaining)
                }
                None => (None, message.to_string()),
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
        let effective_temperature: Option<f64> = agent_model_provider
            .as_ref()
            .and_then(|e| e.temperature)
            .map(|t| {
                crate::agent::thinking::clamp_temperature(
                    t + thinking_params.temperature_adjustment,
                )
            });

        // Prepend thinking system prompt prefix when present.
        if let Some(ref prefix) = thinking_params.system_prompt_prefix {
            system_prompt = format!("{prefix}\n\n{system_prompt}");
        }

        let effective_msg_ref = effective_message.as_str();
        let runtime_capability_names: Vec<&str> = effective_tool_names.iter().copied().collect();
        if let Some(suggestion) = crate::skills::render_missing_skill_install_suggestion(
            effective_msg_ref,
            &skills,
            &runtime_capability_names,
            &config.data_dir,
            &config.skills.extra_registries,
            config.skills.install_suggestions.enabled,
        ) {
            return Ok(suggestion);
        }

        // Memory context is injected once in the engine, keyed on the ingress
        // origin (agent::memory_inject); recall is scoped to this entry's
        // session_id. Hardware RAG stays site-built; the engine prepends the
        // memory block above it.
        // Pre-mint the turn id so the pre-turn RAG retrieval and the
        // agent_turn bracket share one correlation id. The RAG span stays a
        // root span (it runs before AgentStart) but carries the matching
        // zeroclaw.turn_id attribute; nesting it is a tracked follow-up.
        let turn_id = uuid::Uuid::new_v4().to_string();
        let rag_limit = if eff_compact_context { 2 } else { 5 };
        let hw_context = hardware_rag
            .as_ref()
            .map(|r| {
                build_hardware_context(
                    r,
                    &*observer,
                    effective_msg_ref,
                    &board_names,
                    rag_limit,
                    TurnMeta {
                        parent_agent_alias: None,
                        agent_alias: Some(agent_alias),
                        turn_id: &turn_id,
                        channel_name: "daemon",
                    },
                )
            })
            .unwrap_or_default();
        // `process_message` does not scope a session key of its own — its
        // callers (gateway, peer messaging) do, and they are outer entry
        // points: nothing calls `process_message` from inside another turn
        // shape, so it is always the claimant for whatever key it inherits and
        // needs no ownership gate of the kind `run()` carries. With no key in
        // scope the claim is a no-op and the turn is unchanged.
        //
        // The guard is settled against the turn's own outcome below; every
        // fallible step between here and the provider call would otherwise
        // consume these announcements without the model reading them.
        let (announcements, announcement_guard) = claim_announcements_for_turn(true).await;
        let context = format!("{hw_context}{announcements}");
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %Z");
        let enriched = if context.is_empty() {
            format!("[{now}] {effective_message}")
        } else {
            format!("{context}[{now}] {effective_message}")
        };
        observe_turn_user_message(&enriched);

        let mut history = vec![
            ChatMessage::system(&system_prompt),
            ChatMessage::user(&enriched),
        ];
        let mut excluded_tools = compute_excluded_mcp_tools(
            &tools_registry,
            &agent.resolved.tool_filter_groups,
            effective_msg_ref,
            &mcp_tool_names_pm,
        );
        {
            let active_profile = &risk_profile;
            if active_profile.level != AutonomyLevel::Full {
                excluded_tools.extend(active_profile.excluded_tools.iter().cloned());
            }
        }

        let routed_approval_channel = risk_profile.approval_route.as_ref().and_then(|route| {
            live_channel_registry().map(|handles| {
                crate::agent::agent::RoutedApprovalChannel::new(handles, route.clone())
            })
        });
        let routed_approval_channel_ref = routed_approval_channel
            .as_ref()
            .map(|c| c as &dyn zeroclaw_api::channel::Channel);

        let turn_result = zeroclaw_api::NATIVE_THINKING_OVERRIDE
            .scope(
                thinking_params.native_thinking,
                agent_turn_with_sop_reassembly(
                    Some(&config),
                    model_provider.as_ref(),
                    &mut history,
                    &tools_registry,
                    observer.as_ref(),
                    provider_name,
                    &model_name,
                    effective_temperature,
                    true,
                    "daemon",
                    None,
                    &config.multimodal,
                    agent.resolved.max_tool_iterations,
                    Some(&approval_manager),
                    &excluded_tools,
                    &agent.resolved.tool_call_dedup_exempt,
                    activated_handle_pm.as_ref(),
                    None,
                    agent.resolved.strict_tool_parsing,
                    agent.resolved.parallel_tools,
                    agent.resolved.max_tool_result_chars,
                    agent.resolved.max_context_tokens,
                    // Cross-channel HITL: a route-only approval bridge when the
                    // profile sets `approval_route` and channels are live, else
                    // `None` (today's channel-less auto-deny). See above.
                    routed_approval_channel_ref,
                    origin,
                    Some(crate::agent::memory_inject::TurnMemory {
                        handle: mem.as_ref(),
                        query: effective_message.clone(),
                        sessions: vec![session_id.map(str::to_string)],
                        suppress: false,
                        cfg: crate::agent::memory_inject::MemoryInjectConfig::from_memory_config(
                            &config.memory,
                            crate::agent::memory_inject::DEFAULT_RECALL_LIMIT,
                        ),
                    }),
                    Some(agent_alias),
                    Some(&turn_id),
                    Some(SopStepReassembly { config: &config }),
                ),
            )
            .await;
        // Success point for this entry point: the turn returns `Ok` only after
        // the provider call, so the announcements have been read. On `Err` the
        // guard drops still armed and returns them to the store.
        settle_announcement_guards(announcement_guard, turn_result)
    };
    __zc_body
        .instrument(__zc_scope_span)
        .instrument(__zc_attribution_span)
        .await
}

//! Channel listener startup and inbound dispatch wiring. Extracted from orchestrator/mod.rs (god-file remainder C7).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use parking_lot::RwLock;
use zeroclaw_api::channel::Channel;
use zeroclaw_api::memory_traits::MemoryStrategy;
use zeroclaw_config::schema::Config;
use zeroclaw_memory::Memory;
use zeroclaw_providers::{ChatMessage, ModelProvider, ProviderDispatch};
use zeroclaw_runtime::agent::loop_::build_tool_instructions_for_names;
use zeroclaw_runtime::approval::ApprovalManager;
use zeroclaw_runtime::observability::{self, Observer};
use zeroclaw_runtime::platform;
use zeroclaw_runtime::security::{AutonomyLevel, SecurityPolicy};
use zeroclaw_runtime::tools;

#[cfg(feature = "channel-nostr")]
use super::NostrChannel;
use super::{
    AgentRouter, CRON_CHANNEL_REGISTRY, ChannelAssembledTools, ChannelCostTrackingState,
    ChannelRuntimeContext, ConfiguredChannel, DEFAULT_CHANNEL_INITIAL_BACKOFF_SECS,
    DEFAULT_CHANNEL_MAX_BACKOFF_SECS, MAX_CHANNEL_HISTORY, MAX_CONVERSATION_SENDERS, MessageInbox,
    TaskPreferenceOverlay, assemble_channel_agent_tools, build_owner_by_channel_key,
    build_system_prompt_with_mode_and_autonomy, collect_configured_channels,
    compose_channel_mcp_prompt_sections, composite_channel_key, configured_channel_map,
    create_resilient_model_provider_nonblocking, effective_channel_message_timeout_secs,
    interrupt_on_new_message_config, max_in_flight_messages_for_config, run_message_dispatch_loop,
    runtime_defaults_from_config, spawn_supervised_listener,
};

#[cfg(feature = "channel-nostr")]
use super::ActiveChannelAliases;

pub async fn start_channels(
    config: Config,
    canvas_store: Option<zeroclaw_runtime::tools::CanvasStore>,
    cancel: tokio_util::sync::CancellationToken,
    companion_store: Option<Arc<zeroclaw_memory::CompanionStore>>,
) -> Result<()> {
    let config_arc = Arc::new(RwLock::new(config));
    let config: Config = config_arc.read().clone();
    let any_agent_provider_resolves = config
        .agents
        .iter()
        .filter(|(_, a)| a.enabled)
        .any(|(_, a)| runtime_defaults_from_config(&config, a.model_provider.as_str()).is_ok());
    if !any_agent_provider_resolves {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "Channels supervisor: no model configured. Waiting for reload \
             (complete onboarding at /onboard or set \
             [providers.models.<type>.<alias>] model = \"...\" and reload)."
        );
        cancel.cancelled().await;
        return Ok(());
    }

    zeroclaw_providers::pricing::spawn_refresher(config_arc.clone());

    let enabled_agents: Vec<String> = {
        let mut v: Vec<String> = config
            .agents
            .iter()
            .filter(|(_, a)| a.enabled)
            .map(|(alias, _)| alias.clone())
            .collect();
        if v.is_empty() {
            anyhow::bail!("start_channels requires at least one enabled [agents.<alias>] entry");
        }
        v.sort();
        v
    };

    let observer: Arc<dyn Observer> =
        Arc::from(observability::create_observer(&config.observability));
    let runtime: Arc<dyn platform::RuntimeAdapter> =
        Arc::from(platform::create_runtime(&config.runtime)?);

    // i18n is process-global; initialize once before the per-agent loop
    // touches tool descriptions.
    let i18n_locale = config
        .locale
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(zeroclaw_runtime::i18n::detect_locale);
    zeroclaw_runtime::i18n::init(&i18n_locale);

    if let Some(store) = companion_store.as_ref() {
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
                ::serde_json::json!({
                    "path": store.path().display().to_string(),
                })
            ),
            "channels supervisor holding companion store"
        );
    }

    // Single session backend shared across agents — they're scoped by
    // `session_key` (which already encodes `<channel_type>.<alias>`), so
    // multiple agent ctxs reading the same backend never overlap.
    let shared_session_store: Option<Arc<dyn zeroclaw_infra::session_backend::SessionBackend>> =
        if config.channels.session_persistence {
            match zeroclaw_infra::make_session_backend(
                &config.data_dir,
                &config.channels.session_backend,
            ) {
                Ok(backend) => {
                    ::zeroclaw_log::record!(
                        INFO,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                        &format!(
                            "📂 Session persistence enabled (backend: {})",
                            config.channels.session_backend
                        )
                    );
                    Some(backend)
                }
                Err(e) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                        "Session persistence disabled"
                    );
                    None
                }
            }
        } else {
            None
        };

    let mut channels_by_name_shared: Option<Arc<HashMap<String, Arc<dyn Channel>>>> = None;
    let mut collected_channel_keys: Vec<String> = Vec::new();
    let mut max_in_flight_messages: Option<usize> = None;
    let mut listener_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let mut rx_holder: Option<tokio::sync::mpsc::Receiver<zeroclaw_api::channel::ChannelMessage>> =
        None;

    let mut agent_ctxs: HashMap<String, Arc<ChannelRuntimeContext>> = HashMap::new();

    let user_model_store = match tokio::task::spawn_blocking({
        let data_dir = config.data_dir.clone();
        move || zeroclaw_memory::companion::UserModelStore::open(&data_dir)
    })
    .await
    {
        Ok(Ok(store)) => Some(Arc::new(store)),
        Ok(Err(err)) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({
                        "data_dir": config.data_dir.display().to_string(),
                        "err": err.to_string(),
                    })),
                "user model store open failed; owner-profile projection disabled"
            );
            None
        }
        Err(_) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "user model store open task failed; owner-profile projection disabled"
            );
            None
        }
    };

    let task_prefs = Arc::new(TaskPreferenceOverlay::new());

    for agent_alias in &enabled_agents {
        let agent = config
            .resolved_agent_config(agent_alias)
            .with_context(|| format!("agents.{agent_alias} is not configured"))?;
        let risk_profile = config
            .risk_profile_for_agent(agent_alias)
            .with_context(|| {
                format!(
                    "agents.{agent_alias}.risk_profile does not name a configured risk_profiles entry"
                )
            })?
            .clone();

        // Resolve the agent's model provider strictly from its mandatory
        // `<type>.<alias>` reference. No fallback to a first/default provider:
        // an agent whose ref does not resolve to a configured entry with a
        // `model` is rejected here.
        let runtime_defaults = runtime_defaults_from_config(&config, agent.model_provider.as_str())
            .with_context(|| format!("agents.{agent_alias}.model_provider"))?;
        let provider_name = runtime_defaults.default_model_provider.clone();
        let model = runtime_defaults.model.clone();
        let temperature = runtime_defaults.temperature;
        let provider_api_key = runtime_defaults.api_key.clone();
        let provider_api_url = runtime_defaults.api_url.clone();
        let provider_reliability = runtime_defaults.reliability.clone();
        let provider_runtime_options =
            zeroclaw_providers::provider_runtime_options_for_agent(&config, agent_alias);
        let model_provider: Arc<dyn ModelProvider> = Arc::from(
            create_resilient_model_provider_nonblocking(
                Arc::new(config.clone()),
                &provider_name,
                provider_api_key.clone(),
                provider_api_url.clone(),
                provider_reliability.clone(),
                provider_runtime_options.clone(),
            )
            .await?,
        );

        if let Err(e) = ProviderDispatch::from_ref(&*model_provider).warmup().await {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(
                        ::serde_json::json!({"error": format!("{}", e), "agent": agent_alias})
                    ),
                "ModelProvider warmup failed (non-fatal)"
            );
        }

        let security = Arc::new(SecurityPolicy::for_agent(&config, agent_alias)?);
        let mem: Arc<dyn Memory> = zeroclaw_memory::create_memory_for_agent(
            &config,
            agent_alias,
            provider_api_key.as_deref(),
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

        let workspace = config.agent_workspace_dir(agent_alias);
        // Per-agent skills: install-wide workspace + open_skills set,
        // unioned with this agent's declared `skill_bundles`.
        let skills =
            zeroclaw_runtime::skills::load_skills_for_agent(&workspace, &config, agent_alias);

        let all_tools_result_ch = tools::all_tools_with_runtime(
            Arc::new(config.clone()),
            &security,
            &risk_profile,
            agent_alias,
            Arc::clone(&runtime),
            Arc::clone(&mem),
            composio_key,
            composio_entity_id,
            &config.browser,
            &config.http_request,
            &config.web_fetch,
            &workspace,
            &config.agents,
            provider_api_key.as_deref(),
            &config,
            canvas_store.clone(),
            false,
            None,
            Some(Arc::clone(&config_arc)),
            // Channel turns are top-level origins: the turn mints its own
            // lineage root; no inherited lineage crosses this boundary.
            None,
        );
        // Route the per-agent tool registry through the one gated seam - see
        // `assemble_channel_agent_tools` for the knobs and why. `mut` because the
        // text-tool prompt policy below may clear `deferred_section` for a
        // non-native strict-tool-parsing target.
        let ChannelAssembledTools {
            tools: built_tools,
            mut deferred_section,
            pinned_section,
            ask_user_handle: ask_user_handle_ch,
            reaction_handle: reaction_handle_ch,
            poll_handle: poll_handle_ch,
            escalate_handle: escalate_handle_ch,
            channel_room_handle: channel_room_handle_ch,
            activated_handle: ch_activated_handle,
        } = assemble_channel_agent_tools(
            &config,
            agent_alias,
            provider_name.as_str(),
            model.as_str(),
            &security,
            all_tools_result_ch,
            &skills,
            Arc::clone(&runtime),
        )
        .await;

        let tool_specs: Vec<(String, String)> = built_tools
            .iter()
            .map(|t| (t.name().to_string(), t.description().to_string()))
            .collect();

        let tools_registry = Arc::new(built_tools);

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
            config.effective_skills_prompt_mode(agent_alias),
            zeroclaw_config::schema::SkillsPromptInjectionMode::Compact
        ) {
            tool_descs.push((
                "read_skill",
                "Load the full source for an available skill by name. Use when: compact mode only shows a summary and you need the complete skill instructions.",
            ));
        }
        if config.browser.enabled {
            tool_descs.push((
                "browser_open",
                "Open approved HTTPS URLs in system browser (allowlist-only, no scraping)",
            ));
        }
        if config.composio.enabled {
            tool_descs.push((
                "composio",
                "Execute actions on 1000+ apps via Composio (Gmail, Notion, GitHub, Slack, etc.). Use action='list' to discover actions, 'list_accounts' to retrieve connected account IDs, 'execute' to run (optionally with connected_account_id), and 'connect' for OAuth.",
            ));
        }
        tool_descs.push((
            "schedule",
            "Manage scheduled tasks (create/list/get/cancel/pause/resume). Supports recurring cron and one-shot delays.",
        ));
        tool_descs.push((
            "pushover",
            "Send a Pushover notification to your device. Requires PUSHOVER_TOKEN and PUSHOVER_USER_KEY in .env file.",
        ));
        tool_descs.push((
            "channel_room",
            "Create channel rooms and invite users through active channels. Use with Matrix channel keys such as matrix.default.",
        ));
        if !config.agents.is_empty() {
            tool_descs.push((
                "delegate",
                "Delegate a subtask to a specialized agent. Use when: a task benefits from a different model (e.g. fast summarization, deep reasoning, code generation). The sub-agent runs a single prompt and returns its response.",
            ));
        }
        if config.channels.email.values().any(|c| c.enabled) {
            tool_descs.push((
                "email_search",
                "Search the IMAP inbox by sender, subject, or date. Returns a list of matching emails with UID, sender, subject, and date. Use when asked about email. Follow up with email_read to fetch the full body.",
            ));
            tool_descs.push((
                "email_read",
                "Fetch the full content of an email by its UID (from email_search). Returns sender, to, date, subject, body text, and attachments.",
            ));
        }

        // Filter out tools excluded for non-CLI channels so this agent's
        // system prompt does not advertise them for channel-driven runs.
        {
            let active_profile = &risk_profile;
            let excluded = &active_profile.excluded_tools;
            if !excluded.is_empty() && active_profile.level != AutonomyLevel::Full {
                tool_descs.retain(|(name, _)| !excluded.iter().any(|ex| ex == name));
            }
        }
        let effective_tool_names: HashSet<&str> =
            tools_registry.iter().map(|tool| tool.name()).collect();
        tool_descs.retain(|(name, _)| effective_tool_names.contains(name));

        let bootstrap_max_chars = if agent.resolved.compact_context {
            Some(6000)
        } else {
            None
        };
        let native_tools = model_provider.supports_native_tools();
        let expose_text_tool_protocol = compose_channel_mcp_prompt_sections(
            native_tools,
            agent.resolved.strict_tool_parsing,
            &mut tool_descs,
            &mut deferred_section,
            &pinned_section,
        );
        let mut system_prompt = build_system_prompt_with_mode_and_autonomy(
            &workspace,
            &model,
            &tool_descs,
            &skills,
            Some(&agent.identity),
            bootstrap_max_chars,
            Some(&risk_profile),
            native_tools,
            config.effective_skills_prompt_mode(agent_alias),
            agent.resolved.compact_context,
            agent.resolved.max_system_prompt_chars,
            true,
            config.channels.show_tool_calls,
        );
        if expose_text_tool_protocol {
            system_prompt.push_str(&build_tool_instructions_for_names(
                tools_registry.as_ref(),
                &effective_tool_names,
            ));
        }
        if !deferred_section.is_empty() {
            system_prompt.push('\n');
            system_prompt.push_str(&deferred_section);
        }
        if agent.resolved.tool_receipts.enabled && agent.resolved.tool_receipts.inject_system_prompt
        {
            system_prompt.push_str(zeroclaw_runtime::agent::tool_receipts::SYSTEM_PROMPT_ADDENDUM);
        }

        if channels_by_name_shared.is_none() {
            if !skills.is_empty() {
                println!(
                    "  🧩 Skills:   {}",
                    skills
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }

            #[allow(unused_mut)]
            let mut configured_channels: Vec<ConfiguredChannel> = collect_configured_channels(
                &config_arc,
                "runtime startup",
                &tool_specs,
            );

            #[cfg(feature = "channel-nostr")]
            {
                let active = ActiveChannelAliases::compute(&config);
                // Materialize the work list into owned values BEFORE any
                // `.await` so we don't hold any lock across the async
                // constructor (parking_lot guards are not Send). Mirrors
                // the same pattern in `doctor_channels`.
                let nostr_jobs: Vec<(String, String, Vec<String>)> = config
                    .channels
                    .nostr
                    .iter()
                    .filter(|(alias, _)| active.contains(&format!("nostr.{alias}")))
                    .filter(|(_, ns)| ns.enabled)
                    .map(|(alias, ns)| (alias.clone(), ns.private_key.clone(), ns.relays.clone()))
                    .collect();
                for (alias, private_key, relays) in nostr_jobs {
                    let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                        let cfg_arc = config_arc.clone();
                        let alias = alias.clone();
                        Arc::new(move || cfg_arc.read().channel_external_peers("nostr", &alias))
                    };
                    configured_channels.push(ConfiguredChannel {
                        display_name: "Nostr",
                        alias: Some(alias.clone()),
                        channel: Arc::new(
                            NostrChannel::new(&private_key, relays, alias, peer_resolver).await?,
                        ),
                    });
                }
            }
            #[cfg(not(feature = "channel-nostr"))]
            if !config.channels.nostr.is_empty() {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                    "Nostr channel is configured but this build was compiled without \
                     `channel-nostr`; skipping Nostr."
                );
            }
            let channels: Vec<Arc<dyn Channel>> = configured_channels
                .iter()
                .map(|cc| Arc::clone(&cc.channel))
                .collect();
            if channels.is_empty() {
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    "No active channels to supervise (none configured or all disabled). \
                     Waiting for reload signal."
                );
                cancel.cancelled().await;
                return Ok(());
            }

            println!("🦀 ZeroClaw Channel Server");
            println!("  🤖 Model:    {model} (agent: {agent_alias})");
            let effective_backend = config.resolve_active_storage().kind();
            println!(
                "  🧠 Memory:   {} (auto-save: {})",
                effective_backend,
                if config.memory.auto_save { "on" } else { "off" }
            );
            let channel_labels: Vec<String> = configured_channels
                .iter()
                .map(|cc| composite_channel_key(cc.channel.name(), cc.alias.as_deref()))
                .collect();
            collected_channel_keys = channel_labels.clone();
            println!("  📡 Channels: {}", channel_labels.join(", "));
            println!("  🤖 Agents:   {}", enabled_agents.join(", "));
            println!();
            println!("  Listening for messages... (Ctrl+C to stop)");
            println!();

            zeroclaw_runtime::health::mark_component_ok("channels");

            let initial_backoff_secs = config
                .reliability
                .channel_initial_backoff_secs
                .max(DEFAULT_CHANNEL_INITIAL_BACKOFF_SECS);
            let max_backoff_secs = config
                .reliability
                .channel_max_backoff_secs
                .max(DEFAULT_CHANNEL_MAX_BACKOFF_SECS);

            let (tx, rx) = tokio::sync::mpsc::channel::<zeroclaw_api::channel::ChannelMessage>(100);

            for cc in &configured_channels {
                listener_handles.push(spawn_supervised_listener(
                    cc.channel.clone(),
                    cc.alias.clone(),
                    tx.clone(),
                    initial_backoff_secs,
                    max_backoff_secs,
                    cancel.clone(),
                ));
            }
            drop(tx);

            // Composite-key registry (see `composite_channel_key`).
            let cbn = Arc::new(configured_channel_map(&configured_channels));
            *CRON_CHANNEL_REGISTRY
                .write()
                .unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&cbn));

            let in_flight = max_in_flight_messages_for_config(channels.len(), &config.channels);
            println!("  🚦 In-flight message limit: {in_flight}");

            max_in_flight_messages = Some(in_flight);
            channels_by_name_shared = Some(cbn);
            rx_holder = Some(rx);
        }

        let channels_by_name = Arc::clone(
            channels_by_name_shared
                .as_ref()
                .expect("channels_by_name initialized on first iteration"),
        );

        // Wire this agent's reaction / ask_user / channel room / escalate tool handles
        // into the shared `channels_by_name` map.
        {
            let mut map = reaction_handle_ch.write();
            for (name, ch) in channels_by_name.as_ref() {
                map.insert(name.clone(), Arc::clone(ch));
            }
        }
        if let Some(ref handle) = ask_user_handle_ch {
            let mut map = handle.write();
            for (name, ch) in channels_by_name.as_ref() {
                map.insert(name.clone(), Arc::clone(ch));
            }
        }
        if let Some(ref handle) = channel_room_handle_ch {
            let mut map = handle.write();
            for (name, ch) in channels_by_name.as_ref() {
                map.insert(name.clone(), Arc::clone(ch));
            }
        }
        if let Some(ref handle) = poll_handle_ch {
            let mut map = handle.write();
            for (name, ch) in channels_by_name.as_ref() {
                map.insert(name.clone(), Arc::clone(ch));
            }
        }
        if let Some(ref handle) = escalate_handle_ch {
            let mut map = handle.write();
            for (name, ch) in channels_by_name.as_ref() {
                map.insert(name.clone(), Arc::clone(ch));
            }
        }

        let mut provider_cache_seed: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
        provider_cache_seed.insert(provider_name.clone(), Arc::clone(&model_provider));
        let message_timeout_secs =
            effective_channel_message_timeout_secs(config.channels.message_timeout_secs);
        let interrupt_on_new_message = interrupt_on_new_message_config(&config.channels);

        let memory_strategy: Arc<dyn MemoryStrategy> = Arc::new(
            zeroclaw_runtime::agent::memory_strategy::DefaultMemoryStrategy::with_config(
                Arc::clone(&mem),
                config.memory.clone(),
                config.data_dir.clone(),
            ),
        );

        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            channels_by_name: Arc::clone(&channels_by_name),
            model_provider: Arc::clone(&model_provider),
            model_provider_ref: Arc::new(provider_name.clone()),
            agent_alias: Arc::new(agent_alias.clone()),
            agent_cfg: Arc::new(agent.clone()),
            prompt_config: Arc::new(config.clone()),
            memory: Arc::clone(&mem),
            memory_strategy,
            companion_store: companion_store.clone(),
            user_model: user_model_store.clone(),
            task_prefs: Arc::clone(&task_prefs),
            tools_registry: Arc::clone(&tools_registry),
            observer: Arc::clone(&observer),
            system_prompt: Arc::new(system_prompt),
            model: Arc::new(model.clone()),
            temperature,
            auto_save_memory: config.memory.auto_save,
            max_tool_iterations: config.effective_max_tool_iterations(agent_alias.as_str()),
            min_relevance_score: config.memory.min_relevance_score,
            conversation_histories: Arc::new(Mutex::new(lru::LruCache::new(
                std::num::NonZeroUsize::new(MAX_CONVERSATION_SENDERS).unwrap(),
            ))),
            pending_new_sessions: Arc::new(Mutex::new(HashSet::new())),
            provider_cache: Arc::new(Mutex::new(provider_cache_seed)),
            route_overrides: Arc::new(Mutex::new(HashMap::new())),
            thinking_overrides: Arc::new(Mutex::new(HashMap::new())),
            scope_overrides: Arc::new(Mutex::new(HashMap::new())),
            reliability: Arc::new(config.reliability.clone()),
            provider_runtime_options,
            workspace_dir: Arc::new(workspace.clone()),
            message_timeout_secs,
            interrupt_on_new_message,
            multimodal: config.multimodal.clone(),
            media_pipeline: config.media_pipeline.clone(),
            transcription_config: config.transcription.clone(),
            agent_transcription_provider: agent.transcription_provider.as_str().to_string(),
            hooks: if config.hooks.enabled {
                Some(Arc::new(zeroclaw_runtime::hooks::HookRunner::from_config(
                    &config.hooks,
                )))
            } else {
                None
            },
            non_cli_excluded_tools: Arc::new(risk_profile.excluded_tools.clone()),
            autonomy_level: risk_profile.level,
            tool_call_dedup_exempt: Arc::new(agent.resolved.tool_call_dedup_exempt.clone()),
            model_routes: Arc::new(config.model_routes.clone()),
            query_classification: config.query_classification.clone(),
            ack_reactions: config.channels.ack_reactions,
            show_tool_calls: config.channels.show_tool_calls,
            session_store: shared_session_store.clone(),
            approval_manager: Arc::new(
                ApprovalManager::for_non_interactive(&risk_profile).with_store_at(&config.data_dir),
            ),
            activated_tools: ch_activated_handle,
            cost_tracking: zeroclaw_runtime::cost::CostTracker::get_or_init_global(
                config.cost.clone(),
                &config.data_dir,
            )
            .map(|tracker| {
                let by_type =
                    zeroclaw_runtime::agent::cost::build_type_level_model_provider_pricing(&config);
                ChannelCostTrackingState {
                    tracker,
                    model_provider_pricing: Arc::new(by_type),
                    agent_alias: Arc::new(agent_alias.clone()),
                }
            }),
            pacing: config.pacing.clone(),
            max_tool_result_chars: agent.resolved.max_tool_result_chars,
            context_token_budget: agent.resolved.max_context_tokens,
            debouncer: Arc::new(zeroclaw_infra::debounce::MessageDebouncer::new(
                Duration::from_millis(config.channels.debounce_ms),
            )),
            receipt_generator: if agent.resolved.tool_receipts.enabled {
                Some(zeroclaw_runtime::agent::tool_receipts::ReceiptGenerator::new())
            } else {
                None
            },
            show_receipts_in_response: agent.resolved.tool_receipts.show_in_response,
            last_applied_config_stamp: Arc::new(Mutex::new(None)),
            runtime_defaults_override: Arc::new(Mutex::new(None)),
            persist_locks: Arc::new(std::sync::Mutex::new(HashMap::new())),
        });

        if let Some(store) = runtime_ctx.companion_store() {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({
                        "path": store.path().display().to_string(),
                        "agent": agent_alias,
                    })),
                "channel runtime holding companion store"
            );
        }

        agent_ctxs.insert(agent_alias.clone(), runtime_ctx);
    }

    let owner_by_channel_key =
        build_owner_by_channel_key(&config, &enabled_agents, &collected_channel_keys);

    // Hydrate persisted session histories into the owning agent's
    // `conversation_histories` LRU. Sessions whose channel has no enabled
    // owner are skipped so their history doesn't end up loaded into the
    // fallback agent (which wouldn't reply on that channel anyway).
    if let Some(ref store) = shared_session_store {
        let mut metadata = store.list_sessions_with_metadata();
        metadata.sort_by_key(|m| std::cmp::Reverse(m.last_activity));
        // Budget proportional to the number of agents — each gets up to
        // `MAX_CONVERSATION_SENDERS` slots, so a multi-agent install
        // hydrates strictly more total sessions than a single-agent one.
        let cap = MAX_CONVERSATION_SENDERS.saturating_mul(enabled_agents.len().max(1));
        if metadata.len() > cap {
            metadata.truncate(cap);
        }

        let mut hydrated = 0usize;
        let mut orphans_closed = 0usize;
        for m in metadata {
            let owner_agent = m
                .channel_id
                .as_deref()
                .and_then(|cid| owner_by_channel_key.get(cid).cloned())
                .or_else(|| {
                    m.channel_id
                        .as_deref()
                        .and_then(|cid| cid.split_once('.').map(|(b, _)| b.to_string()))
                        .and_then(|b| owner_by_channel_key.get(&b).cloned())
                });
            let target_ctx = match owner_agent.as_ref().and_then(|a| agent_ctxs.get(a)) {
                Some(ctx) => ctx,
                None => continue,
            };
            let mut msgs = store.load(&m.key);
            if msgs.is_empty() {
                continue;
            }
            if msgs.len() > MAX_CHANNEL_HISTORY {
                msgs.drain(..msgs.len() - MAX_CHANNEL_HISTORY);
            }
            if msgs.last().is_some_and(|msg| msg.role == "user") {
                let closure =
                    ChatMessage::assistant("[Session interrupted — not continuing this request]");
                if let Err(e) = store.append(&m.key, &closure) {
                    ::zeroclaw_log::record!(
                        DEBUG,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                        &format!("Failed to persist orphan closure for {}", m.key)
                    );
                }
                msgs.push(closure);
                orphans_closed += 1;
            }
            let pruned =
                zeroclaw_runtime::agent::history_pruner::remove_orphaned_tool_messages(&mut msgs);
            if !pruned.is_empty() {
                ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"category": "agent", "agent_alias": owner_agent.as_deref().unwrap_or(""), "channel": m.channel_id.as_deref().unwrap_or(""), "session_key": m.key, "removed": pruned.removed, "orphan_tool_call_ids": pruned.orphan_tool_call_ids})), "removed orphaned tool messages from restored history (tool_use/tool_result pairing inconsistency auto-healed)");
            }

            let mut histories = target_ctx
                .conversation_histories
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            histories.push(m.key.clone(), msgs);
            drop(histories);
            hydrated += 1;
        }
        if hydrated > 0 {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({"hydrated": hydrated})),
                "restored sessions from disk"
            );
        }
        if orphans_closed > 0 {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({"orphans_closed": orphans_closed})),
                "closed orphaned session turns from previous crash"
            );
        }
    }

    let router = AgentRouter::multi(agent_ctxs, owner_by_channel_key);

    let seen_data_dir = config.data_dir.clone();
    let seen_ids =
        match tokio::task::spawn_blocking(move || MessageInbox::open(&seen_data_dir)).await {
            Ok(Ok(store)) => Some(Arc::new(store)),
            Ok(Err(err)) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "data_dir": config.data_dir.display().to_string(),
                            "err": err.to_string(),
                        })),
                    "inbox store open failed; inbound redelivery dedup disabled"
                );
                None
            }
            Err(_) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                    "inbox store open task failed; inbound redelivery dedup disabled"
                );
                None
            }
        };

    let rx = rx_holder.expect("rx initialized by first agent's channel setup");
    let max_in_flight =
        max_in_flight_messages.expect("max_in_flight initialized by first agent's channel setup");
    run_message_dispatch_loop(rx, router, max_in_flight, seen_ids).await;

    for h in listener_handles {
        let _ = h.await;
    }

    Ok(())
}

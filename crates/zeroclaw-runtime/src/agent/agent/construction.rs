//! Config-based Agent constructors extracted from `agent.rs`.

use super::*;
use crate::agent::prompt::SystemPromptBuilder;
use crate::approval::ApprovalManager;
use crate::observability::{self, Observer};
use crate::platform;
use crate::security::SecurityPolicy;
use crate::tools;
use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Arc;
use zeroclaw_config::schema::Config;
use zeroclaw_memory::Memory;

impl Agent {
    pub async fn from_config(config: &Config, agent_alias: &str) -> Result<Self> {
        Self::from_config_with_session_cwd(config, agent_alias, None).await
    }

    pub async fn from_config_with_session_cwd(
        config: &Config,
        agent_alias: &str,
        session_cwd: Option<&Path>,
    ) -> Result<Self> {
        Self::from_config_with_session_cwd_and_mcp(config, agent_alias, session_cwd, true).await
    }

    pub async fn from_config_with_session_cwd_and_mcp(
        config: &Config,
        agent_alias: &str,
        session_cwd: Option<&Path>,
        initialize_mcp: bool,
    ) -> Result<Self> {
        Self::from_config_with_session_cwd_and_mcp_approval_mode(
            config,
            agent_alias,
            session_cwd,
            initialize_mcp,
            false,
            false,
            None,
            None,
            None,
        )
        .await
    }

    pub async fn from_config_with_session_cwd_and_mcp_backchannel(
        config: &Config,
        agent_alias: &str,
        session_cwd: Option<&Path>,
        initialize_mcp: bool,
        exclude_memory: bool,
        canvas_store: Option<tools::CanvasStore>,
    ) -> Result<Self> {
        Self::from_config_with_session_cwd_and_mcp_approval_mode(
            config,
            agent_alias,
            session_cwd,
            initialize_mcp,
            true,
            exclude_memory,
            None,
            canvas_store,
            None,
        )
        .await
    }

    /// Build a daemon-backed ACP/WS Agent whose structured-history cap follows
    /// the shared config after reloads.
    pub async fn from_live_config_with_session_cwd_and_mcp_backchannel(
        live_config: Arc<parking_lot::RwLock<Config>>,
        agent_alias: &str,
        session_cwd: Option<&Path>,
        initialize_mcp: bool,
        exclude_memory: bool,
        canvas_store: Option<tools::CanvasStore>,
    ) -> Result<Self> {
        let config = live_config.read().clone();
        Self::from_config_with_session_cwd_and_mcp_approval_mode(
            &config,
            agent_alias,
            session_cwd,
            initialize_mcp,
            true,
            exclude_memory,
            None,
            canvas_store,
            Some(live_config),
        )
        .await
    }

    /// Like [`Self::from_config_with_session_cwd_and_mcp_backchannel`] but also
    /// injects the TUI's captured shell environment so that tools like
    /// `ShellTool` inherit the user's real `PATH`, `SSH_AUTH_SOCK`, etc.
    /// rather than the daemon's stripped-down process environment.
    pub async fn from_config_with_tui_env(
        config: &Config,
        agent_alias: &str,
        session_cwd: Option<&Path>,
        initialize_mcp: bool,
        exclude_memory: bool,
        tui_env: Option<std::collections::HashMap<String, String>>,
    ) -> Result<Self> {
        Self::from_config_with_session_cwd_and_mcp_approval_mode(
            config,
            agent_alias,
            session_cwd,
            initialize_mcp,
            true,
            exclude_memory,
            tui_env,
            None,
            None,
        )
        .await
    }

    /// Build a daemon-backed TUI Agent whose structured-history cap follows
    /// the shared config after reloads.
    pub async fn from_live_config_with_tui_env(
        live_config: Arc<parking_lot::RwLock<Config>>,
        agent_alias: &str,
        session_cwd: Option<&Path>,
        initialize_mcp: bool,
        exclude_memory: bool,
        tui_env: Option<std::collections::HashMap<String, String>>,
    ) -> Result<Self> {
        let config = live_config.read().clone();
        Self::from_config_with_session_cwd_and_mcp_approval_mode(
            &config,
            agent_alias,
            session_cwd,
            initialize_mcp,
            true,
            exclude_memory,
            tui_env,
            None,
            Some(live_config),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn from_config_with_session_cwd_and_mcp_approval_mode(
        config: &Config,
        agent_alias: &str,
        session_cwd: Option<&Path>,
        initialize_mcp: bool,
        approval_backchannel: bool,
        exclude_memory: bool,
        tui_env: Option<std::collections::HashMap<String, String>>,
        canvas_store: Option<tools::CanvasStore>,
        live_config: Option<Arc<parking_lot::RwLock<Config>>>,
    ) -> Result<Self> {
        let agent_cfg = config
            .agent(agent_alias)
            .with_context(|| format!("agents.{agent_alias} is not configured"))?;
        let risk_profile = config
            .risk_profile_for_agent(agent_alias)
            .with_context(|| {
                format!(
                    "agents.{agent_alias}.risk_profile does not name a configured risk_profiles entry"
                )
            })?;

        let observer: Arc<dyn Observer> =
            Arc::from(observability::create_observer(&config.observability));
        let runtime: Arc<dyn platform::RuntimeAdapter> =
            Arc::from(platform::create_runtime(&config.runtime)?);
        // Per-agent workspace becomes the SecurityPolicy boundary
        // (file_read/write/edit + shell tool jail to the agent's own
        // dir). The session-cwd override still wins so ACP sessions
        // can pin tool path resolution to an IDE-provided cwd.
        let agent_workspace = config.agent_workspace_dir(agent_alias);
        // Create the per-agent workspace dir on demand so bootstrap
        // file writes (and downstream markdown-memory backends) don't
        // hit ENOENT on a fresh install.
        if let Err(e) = tokio::fs::create_dir_all(&agent_workspace).await {
            ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"agent": agent_alias, "workspace": agent_workspace.display().to_string(), "e": e.to_string()})), "Failed to create per-agent workspace dir (continuing): ");
        }
        if let Err(e) = crate::agent::personality::seed_default_personality(
            config,
            agent_alias,
            &agent_workspace,
        )
        .await
        {
            ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"agent": agent_alias, "workspace": agent_workspace.display().to_string(), "e": e.to_string()})), "Failed to ensure per-agent bootstrap files (continuing with whatever exists): ");
        }
        let security = Arc::new({
            // Use for_agent so the runtime profile (max_actions_per_hour,
            // shell_timeout_secs, etc.) is applied — from_risk_profile passes
            // None for the runtime profile and silently falls back to the
            // schema default of 20 actions/hour regardless of config.
            let mut policy = SecurityPolicy::for_agent(config, agent_alias).with_context(|| {
                format!("agents.{agent_alias}: failed to build security policy")
            })?;
            if let Some(cwd) = session_cwd {
                policy.workspace_dir = cwd.to_path_buf();
                policy.allowed_roots.push(agent_workspace.clone());
            }
            policy
        });

        let (provider_name, provider_alias, agent_model_provider) =
            match config.resolved_model_provider_for_agent(agent_alias) {
                Some(resolved) => (resolved.0, resolved.1, Some(resolved.2)),
                None => {
                    let agent_ref = agent_cfg.model_provider.as_str();
                    if !agent_ref.is_empty() {
                        anyhow::bail!(
                            "agents.{agent_alias}.model_provider = \"{agent_ref}\" does not \
                             resolve to a configured [providers.models.<type>.<alias>] entry"
                        );
                    }
                    // V3 schema requires every agent to set model_provider.
                    // Empty is a config error rather than a silent fallback.
                    anyhow::bail!(
                        "agents.{agent_alias}.model_provider is empty — set it to a \
                         configured \"<type>.<alias>\" (e.g. \"anthropic.{agent_alias}\")"
                    );
                }
            };
        let memory: Arc<dyn Memory> = zeroclaw_memory::create_memory_for_agent(
            config,
            agent_alias,
            agent_model_provider.and_then(|e| e.api_key.as_deref()),
        )
        .await?;

        let composio_key = if config.composio.enabled {
            config.composio.api_key.as_deref()
        } else {
            None
        };
        let composio_entity_id = if config.composio.enabled {
            Some(config.composio.entity_id.as_str())
        } else {
            None
        };

        let all_tools_result = tools::all_tools_with_runtime(
            Arc::new(config.clone()),
            &security,
            risk_profile,
            agent_alias,
            runtime.clone(),
            memory.clone(),
            composio_key,
            composio_entity_id,
            &config.browser,
            &config.http_request,
            &config.web_fetch,
            &security.workspace_dir,
            &config.agents,
            agent_model_provider.and_then(|e| e.api_key.as_deref()),
            config,
            canvas_store,
            false,
            tui_env,
            None,
            // Daemon direct-turn construction is a top-level origin: no
            // inherited lineage; the run mints its own root.
            None,
        );
        // Skills are loaded here and handed to `assemble`, which owns skill
        // registration and resolves builtin/MCP elevation against the pre-filter
        // arcs internally. Bundle-aware via `[agents.<alias>].skill_bundles`.
        let skills = crate::skills::load_skills_for_agent_from_config(config, agent_alias);
        let assembled = crate::tools::scoped::ScopedToolRegistry::assemble(
            crate::tools::scoped::ScopedAssembly {
                config,
                agent_alias,
                security: &security,
                built: all_tools_result,
                skills: &skills,
                runtime,
                caller_allowed: None,
                connect_mcp: initialize_mcp,
                connect_peripherals: false,
                exclude_memory,
                list_deferred_mcp_specs: false,
                emit_assembly_logs: true,
                // `from_config` is the Agent (gateway / library) construction
                // path: no cross-turn reuse contract, so the per-call
                // `connect_all` is the correct choice. The daemon heartbeat
                // worker is the only `mcp_registry` supplier.
                mcp_registry: None,
            },
        )
        .await;
        // The Agent injects two distinct MCP prompt slots: `mcp_deferred_section` (the
        // deferred tool-search listing) and `mcp_pinned_section` (pinned resources).
        // `assemble` surfaces the two atomically, so from_config threads each into its
        // own slot below - no duplication, and the deferred advertisement the
        // regression suite asserts is preserved.
        let deferred_section = assembled.deferred_section().to_string();
        let pinned_section = assembled.pinned_section().to_string();
        let crate::tools::scoped::ScopedAssembled {
            registry,
            delegate_handle: _,
            ask_user_handle,
            reaction_handle,
            poll_handle,
            escalate_handle,
            channel_room_handle,
            activated_handle,
            // from_config performs no per-turn tool_filter_groups filtering
            // itself, so mcp_tool_names is dropped here along with `registry`'s
            // already-consumed sibling fields via `..`.
            ..
        } = assembled;
        let tools = registry.into_inner();

        let model_name = match agent_model_provider
            .and_then(|e| e.model.as_deref())
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            Some(m) => m.to_string(),
            None => anyhow::bail!(
                "agents.{agent_alias}.model_provider resolves to a model_provider entry \
                 with no `model` set. Configure [providers.models.{provider_name}.<alias>] \
                 model = \"...\".",
            ),
        };

        let provider_ref = format!("{provider_name}.{provider_alias}");
        let provider_runtime_options = zeroclaw_providers::provider_runtime_options_for_alias(
            config,
            provider_name,
            provider_alias,
        );

        let model_provider: Box<dyn ModelProvider> =
            zeroclaw_providers::create_routed_model_provider_with_options(
                config,
                &provider_ref,
                agent_model_provider.and_then(|e| e.api_key.as_deref()),
                agent_model_provider.and_then(|e| e.uri.as_deref()),
                &config.reliability,
                &config.model_routes,
                &model_name,
                &provider_runtime_options,
            )?;

        let tool_dispatcher = tool_dispatcher_for_provider(agent_cfg, model_provider.as_ref());

        let route_model_by_hint: HashMap<String, String> = config
            .model_routes
            .iter()
            .map(|route| (route.hint.clone(), route.model.clone()))
            .collect();
        let available_hints: Vec<String> = route_model_by_hint.keys().cloned().collect();

        let response_cache = if config.memory.response_cache_enabled {
            zeroclaw_memory::response_cache::ResponseCache::with_hot_cache(
                &config.data_dir,
                config.memory.response_cache_ttl_minutes,
                config.memory.response_cache_max_entries,
                config.memory.response_cache_hot_entries,
            )
            .ok()
            .map(Arc::new)
        } else {
            None
        };

        let approval_manager = if approval_backchannel {
            ApprovalManager::for_non_interactive_backchannel(risk_profile)
        } else {
            ApprovalManager::for_non_interactive(risk_profile)
        }
        .with_store_at(&config.data_dir);

        let structured_history_cap_resolver: Arc<dyn Fn() -> usize + Send + Sync> =
            if let Some(cap_config) = live_config {
                let cap_agent_alias = agent_alias.to_string();
                Arc::new(move || {
                    cap_config
                        .read()
                        .effective_structured_max_history_messages(&cap_agent_alias)
                })
            } else {
                let max = config.effective_structured_max_history_messages(agent_alias);
                Arc::new(move || max)
            };

        let mut agent = Agent::builder()
            .model_provider(model_provider)
            .tools(tools)
            .memory(memory.clone())
            .observer(observer)
            .response_cache(response_cache)
            .tool_dispatcher(tool_dispatcher)
            .memory_inject_cfg(
                crate::agent::memory_inject::MemoryInjectConfig::from_memory_config(
                    &config.memory,
                    config.effective_memory_recall_limit(agent_alias),
                ),
            )
            .prompt_builder(SystemPromptBuilder::with_defaults(
                // Resolved once, here, where `&Config` and the alias are both in
                // hand — not re-resolved per turn. Mirrors the identical
                // `persona_for_agent(...).and_then(to_prompt_section)` expression
                // used by `agent/loop_.rs` and `agent/turn/mod.rs` for the other
                // prompt-building pipeline; `None` when the agent has no persona
                // configured (direct or via card) or every dial sits at medium,
                // which renders the `## Voice` section as nothing.
                //
                // Known cost of once-at-construction (cold review of
                // eb9b155e7): a live `config/set` edit to `agents.<alias>.persona`
                // or `personas.<alias>.*` does not reach an already-built Agent —
                // `rpc/dispatch.rs`'s live-session refresh fires only for
                // model_provider props. The section stays as constructed until
                // the session is rebuilt. Accepted: persona edits are rare and
                // reconnect heals it; widening the refresh matcher is the fix if
                // that ever stops being true.
                config
                    .persona_for_agent(agent_alias)
                    .and_then(zeroclaw_config::persona::PersonaKnobs::to_prompt_section),
            ))
            .config(
                config
                    .resolved_agent_config(agent_alias)
                    .unwrap_or_else(|| agent_cfg.clone()),
            )
            .structured_history_cap_resolver(structured_history_cap_resolver)
            .multimodal_config(config.multimodal.clone())
            .agent_alias(agent_alias.to_string())
            .model_name(model_name)
            .model_provider_name(provider_name.to_string())
            .temperature(agent_model_provider.and_then(|e| e.temperature))
            .workspace_dir(security.workspace_dir.clone())
            .agent_workspace_dir(agent_workspace.clone())
            .classification_config(config.query_classification.clone())
            .available_hints(available_hints)
            .route_model_by_hint(route_model_by_hint)
            .identity_config(agent_cfg.identity.clone())
            .skills(skills)
            .skills_prompt_mode(config.effective_skills_prompt_mode(agent_alias))
            .auto_save(config.memory.auto_save)
            .exclude_memory(exclude_memory)
            .security_summary(Some(security.prompt_summary()))
            .autonomy_level(risk_profile.level)
            .approval_route(risk_profile.approval_route.clone())
            .activated_tools(activated_handle)
            .mcp_deferred_section(Some(deferred_section))
            .mcp_pinned_section(Some(pinned_section))
            .hook_runner(if config.hooks.enabled {
                Some(Arc::new(crate::hooks::HookRunner::from_config(
                    &config.hooks,
                )))
            } else {
                None
            })
            .approval_manager(Some(Arc::new(approval_manager)))
            .provider_switch_config(ProviderSwitchConfig {
                config: Some(std::sync::Arc::new(config.clone())),
            })
            .build()?;

        // Wire per-tool channel-map handles into the agent so callers (e.g.
        // the ACP server) can register back-channels after construction.
        agent.channel_handles = AgentChannelHandles {
            ask_user: ask_user_handle,
            channel_room: channel_room_handle,
            reaction: reaction_handle,
            poll: poll_handle,
            escalate: escalate_handle,
        };

        Ok(agent)
    }
}

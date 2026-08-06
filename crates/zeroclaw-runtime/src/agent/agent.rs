use crate::agent::dispatcher::ToolDispatcher;
#[cfg(test)]
use crate::agent::dispatcher::NativeToolDispatcher;
use crate::agent::eval::AutoClassifyExt;
use crate::agent::prompt::{PromptContext, SystemPromptBuilder};
use crate::approval::ApprovalManager;
use crate::observability::{self, Observer, ObserverEvent};
use crate::platform;
use crate::security::SecurityPolicy;
use crate::sop::{SopAuditLogger, SopEngine};
use crate::tools::{self, Tool};
use anyhow::{Context, Result};
use chrono::{Datelike, Timelike};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use zeroclaw_config::schema::Config;
use zeroclaw_memory::{self, Memory, MemoryCategory};
#[cfg(test)]
use zeroclaw_providers::ChatRequest;
use zeroclaw_providers::{
    self, ChatMessage, ConversationMessage, ModelProvider, ToolResultMessage,
};

// Re-export TurnEvent from zeroclaw-types for backwards compatibility.
pub use zeroclaw_api::agent::TurnEvent;

pub use super::session_model_provider::{
    build_session_model_provider, tool_dispatcher_for_provider,
};

pub(crate) use super::routed_approval::{
    RoutedApproval, RoutedApprovalChannel, resolve_routed_approval,
};

#[derive(Debug)]
struct HistoryTrimNotice {
    dropped_messages: usize,
    kept_turns: usize,
    reason: String,
}

impl HistoryTrimNotice {
    fn into_turn_event(self) -> TurnEvent {
        TurnEvent::HistoryTrimmed {
            dropped_messages: self.dropped_messages,
            kept_turns: self.kept_turns,
            reason: self.reason,
        }
    }
}

async fn forward_history_trim_notice(
    event_tx: &tokio::sync::mpsc::Sender<TurnEvent>,
    notice: Option<HistoryTrimNotice>,
) {
    if let Some(notice) = notice {
        let _ = event_tx.send(notice.into_turn_event()).await;
    }
}

pub struct Agent {
    model_provider: Box<dyn ModelProvider>,
    tools: Vec<Box<dyn Tool>>,
    memory: Arc<dyn Memory>,
    observer: Arc<dyn Observer>,
    prompt_builder: SystemPromptBuilder,
    tool_dispatcher: Box<dyn ToolDispatcher>,
    /// Stable half of the engine's memory-context injection policy
    /// (recall limit, relevance floor, budgets). Threaded into `ToolLoop`
    /// as `TurnMemory.cfg` on every turn.
    memory_inject_cfg: crate::agent::memory_inject::MemoryInjectConfig,
    config: zeroclaw_config::schema::AliasedAgentConfig,
    /// Resolves the structured-history cap from canonical config at use time.
    /// Daemon-backed sessions capture the shared live config handle so reloads
    /// affect existing sessions without duplicating config-derived state.
    structured_history_cap_resolver: Option<Arc<dyn Fn() -> usize + Send + Sync>>,
    multimodal_config: zeroclaw_config::schema::MultimodalConfig,
    model_name: String,
    model_provider_name: String,
    temperature: Option<f64>,
    workspace_dir: std::path::PathBuf,
    /// Per-agent persona workspace (`<install>/agents/<alias>/workspace/`).
    /// Holds IDENTITY.md / SOUL.md / USER.md / AGENTS.md. Distinct from
    /// `workspace_dir`, which is the security sandbox root and can be the
    /// session cwd for IDE-driven sessions (ACP, gateway WS).
    agent_workspace_dir: std::path::PathBuf,
    identity_config: zeroclaw_config::schema::IdentityConfig,
    skills: Vec<crate::skills::Skill>,
    skills_prompt_mode: zeroclaw_config::schema::SkillsPromptInjectionMode,
    auto_save: bool,
    memory_session_id: Option<String>,
    history: Vec<ConversationMessage>,
    /// True only when `history` contains the synthetic trim breadcrumb inserted
    /// by this Agent. User text is never inferred to be synthetic by content.
    history_has_trim_breadcrumb: bool,
    classification_config: zeroclaw_config::schema::QueryClassificationConfig,
    available_hints: Vec<String>,
    route_model_by_hint: HashMap<String, String>,
    response_cache: Option<Arc<zeroclaw_memory::response_cache::ResponseCache>>,
    /// Pre-rendered security policy summary injected into the system prompt
    /// so the LLM knows the concrete constraints before making tool calls.
    security_summary: Option<String>,
    /// Autonomy level from config; controls safety prompt instructions.
    autonomy_level: crate::security::AutonomyLevel,
    /// Cross-channel HITL: resolved from the active risk profile's
    /// `approval_route`. When set, the per-turn approval bridge asks the named
    /// approver channel (bounded + fail-closed) instead of the originating
    /// fan-out. `None` ⇒ today's behavior. See EPIC B.
    approval_route: Option<zeroclaw_config::autonomy::ApprovalRoute>,
    /// Activated MCP tools for deferred loading mode.
    /// When MCP deferred loading is enabled, tools are activated via `tool_search`
    /// and stored here for lookup during tool execution.
    activated_tools: Option<Arc<std::sync::Mutex<crate::tools::ActivatedToolSet>>>,
    /// Pre-rendered MCP pinned-resource system-prompt section, read once at
    /// construction from each server's `pinned_resources` and provenance-wrapped
    /// (`trust="untrusted-external"`). Empty when no pins are configured or all
    /// were skipped. Appended to the system prompt in `build_system_prompt`.
    mcp_pinned_section: String,
    mcp_deferred_section: String,
    /// Hook runner for tool-call auditing and lifecycle side effects.
    hook_runner: Option<Arc<crate::hooks::HookRunner>>,
    /// Approval manager for direct Agent execution paths such as ACP.
    approval_manager: Option<Arc<ApprovalManager>>,
    /// Agent alias, retained for opening attribution spans at external turn
    /// call sites (ACP, gateway WS) where the alias is otherwise unavailable.
    agent_alias: String,
    channel_handles: AgentChannelHandles,
    /// Per-session cache for resolved local image data URIs, threaded into
    /// the turn loop so each unique local image file is read + base64-encoded
    /// at most once per session even though the multimodal pipeline re-walks
    /// the full conversation history on every turn and tool iteration.
    image_cache: zeroclaw_providers::multimodal::LocalImageCache,
    provider_switch_config: Option<ProviderSwitchConfig>,
    /// Channel name stamped onto observer events to identify the calling surface
    /// (e.g. "agent", "wss", "gateway"). Defaults to "agent" for direct Agent callers.
    channel_name: String,
    #[cfg(test)]
    turn_datetime: Option<Arc<dyn Fn() -> chrono::DateTime<chrono::Local> + Send + Sync>>,
}

impl Drop for Agent {
    fn drop(&mut self) {
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_category(::zeroclaw_log::EventCategory::Agent)
                .with_attrs(::serde_json::json!({
                    "model_provider": self.model_provider_name,
                    "model": self.model_name,
                    "history_messages_freed": self.history.len(),
                })),
            "Agent dropped; conversation history and per-session state freed"
        );
    }
}

#[derive(Debug)]
pub struct StreamedTurnSuccess {
    pub response: String,
    pub new_messages: Vec<ConversationMessage>,
}

#[derive(Debug)]
pub struct StreamedTurnError {
    pub error: anyhow::Error,
    pub committed_response: String,
    pub new_messages: Vec<ConversationMessage>,
}

#[derive(Clone, Debug, Default)]
pub struct ProviderSwitchConfig {
    pub config: Option<std::sync::Arc<zeroclaw_config::schema::Config>>,
}

/// Bundle of late-bound channel-map handles owned by an Agent. Cloning is
/// cheap (Arc clones); the underlying maps are shared with the live tools.
#[derive(Clone, Default)]
pub struct AgentChannelHandles {
    pub ask_user: Option<tools::PerToolChannelHandle>,
    pub channel_room: Option<tools::PerToolChannelHandle>,
    pub reaction: tools::PerToolChannelHandle,
    pub poll: Option<tools::PerToolChannelHandle>,
    pub escalate: Option<tools::PerToolChannelHandle>,
}

impl AgentChannelHandles {
    /// Return references to all populated per-tool channel handles.
    fn populated_handles(&self) -> Vec<Option<&tools::PerToolChannelHandle>> {
        vec![
            self.ask_user.as_ref(),
            self.channel_room.as_ref(),
            Some(&self.reaction),
            self.poll.as_ref(),
            self.escalate.as_ref(),
        ]
    }

    /// Register a channel into every populated handle so all channel-driven
    /// tools can resolve it by name.
    pub fn register_channel(
        &self,
        name: impl Into<String>,
        channel: Arc<dyn zeroclaw_api::channel::Channel>,
    ) {
        let name = name.into();
        for handle in self.populated_handles().into_iter().flatten() {
            handle.write().insert(name.clone(), Arc::clone(&channel));
        }
    }

    /// Remove a channel from every populated handle (used on session/stop).
    pub fn unregister_channel(&self, name: &str) {
        for handle in self.populated_handles().into_iter().flatten() {
            handle.write().remove(name);
        }
    }

    /// Look up a registered channel by name from any populated channel map.
    pub fn get_channel(&self, name: &str) -> Option<Arc<dyn zeroclaw_api::channel::Channel>> {
        for handle in self.populated_handles().into_iter().flatten() {
            if let Some(channel) = handle.read().get(name) {
                return Some(Arc::clone(channel));
            }
        }
        None
    }
}

pub struct AgentBuilder {
    model_provider: Option<Box<dyn ModelProvider>>,
    tools: Option<Vec<Box<dyn Tool>>>,
    memory: Option<Arc<dyn Memory>>,
    observer: Option<Arc<dyn Observer>>,
    prompt_builder: Option<SystemPromptBuilder>,
    tool_dispatcher: Option<Box<dyn ToolDispatcher>>,
    memory_inject_cfg: Option<crate::agent::memory_inject::MemoryInjectConfig>,
    config: Option<zeroclaw_config::schema::AliasedAgentConfig>,
    structured_history_cap_resolver: Option<Arc<dyn Fn() -> usize + Send + Sync>>,
    multimodal_config: Option<zeroclaw_config::schema::MultimodalConfig>,
    model_name: Option<String>,
    model_provider_name: Option<String>,
    temperature: Option<f64>,
    workspace_dir: Option<std::path::PathBuf>,
    agent_workspace_dir: Option<std::path::PathBuf>,
    identity_config: Option<zeroclaw_config::schema::IdentityConfig>,
    skills: Option<Vec<crate::skills::Skill>>,
    skills_prompt_mode: Option<zeroclaw_config::schema::SkillsPromptInjectionMode>,
    auto_save: Option<bool>,
    memory_session_id: Option<String>,
    classification_config: Option<zeroclaw_config::schema::QueryClassificationConfig>,
    available_hints: Option<Vec<String>>,
    route_model_by_hint: Option<HashMap<String, String>>,
    allowed_tools: Option<Vec<String>>,
    response_cache: Option<Arc<zeroclaw_memory::response_cache::ResponseCache>>,
    security_summary: Option<String>,
    autonomy_level: Option<crate::security::AutonomyLevel>,
    approval_route: Option<zeroclaw_config::autonomy::ApprovalRoute>,
    activated_tools: Option<Arc<std::sync::Mutex<crate::tools::ActivatedToolSet>>>,
    mcp_pinned_section: Option<String>,
    mcp_deferred_section: Option<String>,
    hook_runner: Option<Arc<crate::hooks::HookRunner>>,
    approval_manager: Option<Arc<ApprovalManager>>,
    agent_alias: Option<String>,
    channel_name: Option<String>,
    exclude_memory: bool,
    provider_switch_config: Option<ProviderSwitchConfig>,
    #[cfg(test)]
    turn_datetime: Option<Arc<dyn Fn() -> chrono::DateTime<chrono::Local> + Send + Sync>>,
}

impl Default for AgentBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentBuilder {
    pub fn new() -> Self {
        Self {
            model_provider: None,
            tools: None,
            memory: None,
            observer: None,
            prompt_builder: None,
            tool_dispatcher: None,
            memory_inject_cfg: None,
            config: None,
            structured_history_cap_resolver: None,
            multimodal_config: None,
            model_name: None,
            model_provider_name: None,
            temperature: None,
            workspace_dir: None,
            agent_workspace_dir: None,
            identity_config: None,
            skills: None,
            skills_prompt_mode: None,
            auto_save: None,
            memory_session_id: None,
            classification_config: None,
            available_hints: None,
            route_model_by_hint: None,
            allowed_tools: None,
            response_cache: None,
            security_summary: None,
            autonomy_level: None,
            approval_route: None,
            activated_tools: None,
            mcp_pinned_section: None,
            mcp_deferred_section: None,
            hook_runner: None,
            approval_manager: None,
            agent_alias: None,
            channel_name: None,
            exclude_memory: false,
            provider_switch_config: None,
            #[cfg(test)]
            turn_datetime: None,
        }
    }

    pub fn model_provider(mut self, model_provider: Box<dyn ModelProvider>) -> Self {
        self.model_provider = Some(model_provider);
        self
    }

    pub fn tools(mut self, tools: Vec<Box<dyn Tool>>) -> Self {
        self.tools = Some(tools);
        self
    }

    pub fn memory(mut self, memory: Arc<dyn Memory>) -> Self {
        self.memory = Some(memory);
        self
    }

    pub fn observer(mut self, observer: Arc<dyn Observer>) -> Self {
        self.observer = Some(observer);
        self
    }

    pub fn prompt_builder(mut self, prompt_builder: SystemPromptBuilder) -> Self {
        self.prompt_builder = Some(prompt_builder);
        self
    }

    pub fn tool_dispatcher(mut self, tool_dispatcher: Box<dyn ToolDispatcher>) -> Self {
        self.tool_dispatcher = Some(tool_dispatcher);
        self
    }

    /// Stable half of the engine's memory-context injection policy. When
    /// unset, defaults preserve the legacy loader shape (recall limit 5,
    /// the schema-default relevance floor).
    pub fn memory_inject_cfg(
        mut self,
        cfg: crate::agent::memory_inject::MemoryInjectConfig,
    ) -> Self {
        self.memory_inject_cfg = Some(cfg);
        self
    }

    pub fn config(mut self, config: zeroclaw_config::schema::AliasedAgentConfig) -> Self {
        self.config = Some(config);
        self
    }

    fn structured_history_cap_resolver(
        mut self,
        resolver: Arc<dyn Fn() -> usize + Send + Sync>,
    ) -> Self {
        self.structured_history_cap_resolver = Some(resolver);
        self
    }

    #[cfg(test)]
    fn structured_max_history_messages(self, max: usize) -> Self {
        self.structured_history_cap_resolver(Arc::new(move || max))
    }

    pub fn multimodal_config(
        mut self,
        multimodal_config: zeroclaw_config::schema::MultimodalConfig,
    ) -> Self {
        self.multimodal_config = Some(multimodal_config);
        self
    }

    pub fn model_name(mut self, model_name: String) -> Self {
        self.model_name = Some(model_name);
        self
    }

    pub fn model_provider_name(mut self, name: String) -> Self {
        self.model_provider_name = Some(name);
        self
    }

    pub fn temperature(mut self, temperature: Option<f64>) -> Self {
        self.temperature = temperature;
        self
    }

    pub fn workspace_dir(mut self, workspace_dir: std::path::PathBuf) -> Self {
        self.workspace_dir = Some(workspace_dir);
        self
    }

    pub fn agent_workspace_dir(mut self, agent_workspace_dir: std::path::PathBuf) -> Self {
        self.agent_workspace_dir = Some(agent_workspace_dir);
        self
    }

    pub fn identity_config(
        mut self,
        identity_config: zeroclaw_config::schema::IdentityConfig,
    ) -> Self {
        self.identity_config = Some(identity_config);
        self
    }

    pub fn skills(mut self, skills: Vec<crate::skills::Skill>) -> Self {
        self.skills = Some(skills);
        self
    }

    pub fn skills_prompt_mode(
        mut self,
        skills_prompt_mode: zeroclaw_config::schema::SkillsPromptInjectionMode,
    ) -> Self {
        self.skills_prompt_mode = Some(skills_prompt_mode);
        self
    }

    pub fn auto_save(mut self, auto_save: bool) -> Self {
        self.auto_save = Some(auto_save);
        self
    }

    pub fn memory_session_id(mut self, memory_session_id: Option<String>) -> Self {
        self.memory_session_id = memory_session_id;
        self
    }

    pub fn classification_config(
        mut self,
        classification_config: zeroclaw_config::schema::QueryClassificationConfig,
    ) -> Self {
        self.classification_config = Some(classification_config);
        self
    }

    pub fn available_hints(mut self, available_hints: Vec<String>) -> Self {
        self.available_hints = Some(available_hints);
        self
    }

    pub fn route_model_by_hint(mut self, route_model_by_hint: HashMap<String, String>) -> Self {
        self.route_model_by_hint = Some(route_model_by_hint);
        self
    }

    pub fn allowed_tools(mut self, allowed_tools: Option<Vec<String>>) -> Self {
        self.allowed_tools = allowed_tools;
        self
    }

    pub fn response_cache(
        mut self,
        cache: Option<Arc<zeroclaw_memory::response_cache::ResponseCache>>,
    ) -> Self {
        self.response_cache = cache;
        self
    }

    pub fn security_summary(mut self, summary: Option<String>) -> Self {
        self.security_summary = summary;
        self
    }

    pub fn autonomy_level(mut self, level: crate::security::AutonomyLevel) -> Self {
        self.autonomy_level = Some(level);
        self
    }

    pub fn approval_route(
        mut self,
        route: Option<zeroclaw_config::autonomy::ApprovalRoute>,
    ) -> Self {
        self.approval_route = route;
        self
    }

    pub fn activated_tools(
        mut self,
        activated: Option<Arc<std::sync::Mutex<tools::ActivatedToolSet>>>,
    ) -> Self {
        self.activated_tools = activated;
        self
    }

    pub fn mcp_pinned_section(mut self, section: Option<String>) -> Self {
        self.mcp_pinned_section = section;
        self
    }

    pub fn mcp_deferred_section(mut self, section: Option<String>) -> Self {
        self.mcp_deferred_section = section;
        self
    }

    pub fn hook_runner(mut self, runner: Option<Arc<crate::hooks::HookRunner>>) -> Self {
        self.hook_runner = runner;
        self
    }

    pub fn approval_manager(mut self, manager: Option<Arc<ApprovalManager>>) -> Self {
        self.approval_manager = manager;
        self
    }

    /// Set the agent alias used for turn-span attribution.
    pub fn agent_alias(mut self, alias: String) -> Self {
        self.agent_alias = Some(alias);
        self
    }

    pub fn channel_name(mut self, name: String) -> Self {
        self.channel_name = Some(name);
        self
    }

    #[cfg(test)]
    fn turn_datetime<F>(mut self, provider: F) -> Self
    where
        F: Fn() -> chrono::DateTime<chrono::Local> + Send + Sync + 'static,
    {
        self.turn_datetime = Some(Arc::new(provider));
        self
    }

    pub fn exclude_memory(mut self, exclude: bool) -> Self {
        self.exclude_memory = exclude;
        self
    }

    pub fn provider_switch_config(mut self, cfg: ProviderSwitchConfig) -> Self {
        self.provider_switch_config = Some(cfg);
        self
    }

    pub fn build(self) -> Result<Agent> {
        let mut tools = self.tools.ok_or_else(|| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"missing_field": "tools"})),
                "AgentBuilder::build missing required field"
            );
            anyhow::Error::msg("tools are required")
        })?;
        let allowed = self.allowed_tools.clone();
        if let Some(ref allow_list) = allowed {
            tools.retain(|t| allow_list.iter().any(|name| name == t.name()));
        }

        // ACP sessions exclude persistent memory: strip memory tools,
        // replace the backend with NoneMemory, and force auto_save off.
        let exclude_memory = self.exclude_memory;
        if exclude_memory {
            tools.retain(|t| !zeroclaw_tools::MEMORY_TOOL_NAMES.contains(&t.name()));
        }

        let memory: Arc<dyn Memory> = if exclude_memory {
            Arc::new(zeroclaw_memory::NoneMemory::new("none"))
        } else {
            self.memory.ok_or_else(|| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"missing_field": "memory"})),
                    "AgentBuilder::build missing required field"
                );
                anyhow::Error::msg("memory is required")
            })?
        };
        let config = self.config.unwrap_or_default();

        Ok(Agent {
            model_provider: self.model_provider.ok_or_else(|| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"missing_field": "model_provider"})),
                    "AgentBuilder::build missing required field"
                );
                anyhow::Error::msg("model_provider is required")
            })?,
            tools,
            memory: memory.clone(),
            observer: self.observer.ok_or_else(|| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"missing_field": "observer"})),
                    "AgentBuilder::build missing required field"
                );
                anyhow::Error::msg("observer is required")
            })?,
            prompt_builder: self
                .prompt_builder
                .unwrap_or_else(|| SystemPromptBuilder::with_defaults(None)),
            tool_dispatcher: self.tool_dispatcher.ok_or_else(|| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"missing_field": "tool_dispatcher"})),
                    "AgentBuilder::build missing required field"
                );
                anyhow::Error::msg("tool_dispatcher is required")
            })?,
            memory_inject_cfg: self.memory_inject_cfg.unwrap_or_else(|| {
                crate::agent::memory_inject::MemoryInjectConfig::from_memory_config(
                    &zeroclaw_config::schema::MemoryConfig::default(),
                    crate::agent::memory_inject::DEFAULT_RECALL_LIMIT,
                )
            }),
            config,
            structured_history_cap_resolver: self.structured_history_cap_resolver,
            multimodal_config: self.multimodal_config.unwrap_or_default(),
            model_name: self.model_name.unwrap_or_else(|| "<unconfigured>".into()),
            model_provider_name: self
                .model_provider_name
                .unwrap_or_else(|| "<unconfigured>".into()),
            temperature: self.temperature,
            // Default for test callers that don't call workspace_dir().
            workspace_dir: self
                .workspace_dir
                .clone()
                .unwrap_or_else(|| std::path::PathBuf::from(".")),
            agent_workspace_dir: self.agent_workspace_dir.unwrap_or_else(|| {
                self.workspace_dir
                    .clone()
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
            }),
            identity_config: self.identity_config.unwrap_or_default(),
            skills: self.skills.unwrap_or_default(),
            skills_prompt_mode: self.skills_prompt_mode.unwrap_or_default(),
            auto_save: if exclude_memory {
                false
            } else {
                self.auto_save.unwrap_or(false)
            },
            memory_session_id: self.memory_session_id,
            history: Vec::new(),
            history_has_trim_breadcrumb: false,
            classification_config: self.classification_config.unwrap_or_default(),
            available_hints: self.available_hints.unwrap_or_default(),
            route_model_by_hint: self.route_model_by_hint.unwrap_or_default(),
            response_cache: self.response_cache,
            security_summary: self.security_summary,
            approval_route: self.approval_route,
            autonomy_level: self
                .autonomy_level
                .unwrap_or(crate::security::AutonomyLevel::Supervised),
            activated_tools: self.activated_tools,
            mcp_pinned_section: self.mcp_pinned_section.unwrap_or_default(),
            mcp_deferred_section: self.mcp_deferred_section.unwrap_or_default(),
            hook_runner: self.hook_runner,
            approval_manager: self.approval_manager,
            agent_alias: self.agent_alias.unwrap_or_default(),
            channel_handles: AgentChannelHandles::default(),
            image_cache: zeroclaw_providers::multimodal::LocalImageCache::new(),
            provider_switch_config: self.provider_switch_config,
            channel_name: self.channel_name.unwrap_or_else(|| "agent".to_string()),
            #[cfg(test)]
            turn_datetime: self.turn_datetime,
        })
    }
}

impl Agent {
    pub fn builder() -> AgentBuilder {
        AgentBuilder::new()
    }

    /// The full `Config` the agent was constructed from, when available. Sourced
    /// from `provider_switch_config` - the single canonical config snapshot the
    /// agent already carries for provider-alias resolution. `None` only on
    /// configless (test-builder) agents; every production construction path
    /// (`from_config` / `from_config_with_tui_env`) populates it. Used by the
    /// vision route to resolve the configured `vision_model_provider`'s
    /// alias-specific options (the `vision` override, endpoint URI, credentials).
    fn full_config(&self) -> Option<&zeroclaw_config::schema::Config> {
        self.provider_switch_config
            .as_ref()
            .and_then(|cfg| cfg.config.as_deref())
    }

    fn tool_loop_cost_tracking_context(&self) -> crate::agent::loop_::ToolLoopCostTrackingContext {
        if let Ok(Some(context)) =
            crate::agent::loop_::TOOL_LOOP_COST_TRACKING_CONTEXT.try_with(Clone::clone)
        {
            return context;
        }

        crate::agent::loop_::ToolLoopCostTrackingContext::usage_only()
    }

    fn current_turn_datetime(&self) -> chrono::DateTime<chrono::Local> {
        #[cfg(test)]
        if let Some(provider) = &self.turn_datetime {
            return provider();
        }

        chrono::Local::now()
    }

    pub fn set_channel_name(&mut self, name: String) {
        self.channel_name = name;
    }

    fn new_turn_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    fn observer_agent_alias(&self) -> Option<String> {
        if self.agent_alias.is_empty() {
            None
        } else {
            Some(self.agent_alias.clone())
        }
    }

    pub fn history(&self) -> &[ConversationMessage] {
        &self.history
    }

    pub fn channel_handles(&self) -> &AgentChannelHandles {
        &self.channel_handles
    }

    pub fn populate_channels(
        &self,
        channel_map: &std::collections::HashMap<String, Arc<dyn zeroclaw_api::channel::Channel>>,
    ) -> Vec<String> {
        let mut names = Vec::new();
        for (name, ch) in channel_map {
            self.channel_handles.register_channel(name, Arc::clone(ch));
            names.push(name.clone());
        }
        names
    }

    /// Attribution fields for opening a turn span at external call sites
    /// (ACP, gateway WS) so every record inside a streamed turn carries the
    /// same `agent_alias`/`model_provider`/`model` the RPC dispatch path sets.
    /// Returns `(agent_alias, model_provider, model)`.
    pub fn attribution_fields(&self) -> (String, String, String) {
        (
            self.agent_alias.clone(),
            self.model_provider_name.clone(),
            self.model_name.clone(),
        )
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
        self.history_has_trim_breadcrumb = false;
    }

    fn encode_response_cache_transcript(messages: &[ChatMessage]) -> String {
        let mut transcript = String::new();
        for message in messages.iter().filter(|message| message.role != "system") {
            transcript.push_str("role=");
            transcript.push_str(&message.role.len().to_string());
            transcript.push(':');
            transcript.push_str(&message.role);
            transcript.push_str(";content=");
            transcript.push_str(&message.content.len().to_string());
            transcript.push(':');
            transcript.push_str(&message.content);
            transcript.push('\n');
        }
        transcript
    }

    fn memory_injection_active(&self) -> bool {
        if self.memory.name() == "none" {
            return false;
        }
        matches!(
            crate::agent::memory_inject::resolve_inject_policy(
                zeroclaw_api::ingress::TurnOrigin::AgentDirect,
                self.memory_session_id.is_some(),
                false,
            ),
            crate::agent::memory_inject::InjectPolicy::Inject { .. }
        )
    }

    fn response_cache_key_for_messages(
        &self,
        messages: &[ChatMessage],
        effective_model: &str,
    ) -> Option<String> {
        // Bypass the cache when a per-turn memory preamble the key cannot see
        // will be injected downstream (see `memory_injection_active`).
        if self.temperature != Some(0.0)
            || self.response_cache.is_none()
            || self.memory_injection_active()
        {
            return None;
        }

        if messages
            .iter()
            .filter(|message| message.role != "system")
            .any(|message| message.content.contains("[IMAGE:"))
        {
            return None;
        }

        let system = messages
            .iter()
            .find(|message| message.role == "system")
            .map(|message| message.content.as_str());
        let transcript = Self::encode_response_cache_transcript(messages);

        Some(zeroclaw_memory::response_cache::ResponseCache::cache_key(
            effective_model,
            system,
            &transcript,
        ))
    }

    /// Build this turn's user message and put it in history.
    ///
    /// Returns the claim guard for any background announcements spliced into
    /// that message. The caller **must** hold it until the turn has ended and
    /// settle it against that turn's outcome
    /// ([`crate::agent::loop_::settle_announcement_guards`]); dropping it
    /// earlier returns the announcements to the store, which is exactly what
    /// should happen when the turn dies on one of the fallible steps between
    /// here and the provider call (`agent/turn/mod.rs` lines 528/535/566/584
    /// versus :628).
    ///
    /// Called once per user message, which includes each mid-turn steering
    /// message: see the steering call site for why a second claim inside one
    /// turn is deliberate.
    async fn append_streamed_user_message_to_history(
        &mut self,
        user_message: &str,
        new_msgs: &mut Vec<ConversationMessage>,
        turn_id: &str,
    ) -> Option<crate::agent::loop_::UnclaimOnDrop> {
        // Memory context is injected once in the engine, keyed on the
        // ingress origin (agent::memory_inject).
        if self.auto_save {
            let store_start = std::time::Instant::now();
            let store_result = self
                .memory
                .store(
                    "user_msg",
                    user_message,
                    MemoryCategory::Conversation,
                    self.memory_session_id.as_deref(),
                )
                .await;
            self.observer.record_event(&ObserverEvent::MemoryStore {
                category: MemoryCategory::Conversation.to_string(),
                backend: self.memory.name().to_string(),
                duration: store_start.elapsed(),
                success: store_result.is_ok(),
                channel: Some(self.channel_name.clone()),
                agent_alias: self.observer_agent_alias(),
                turn_id: Some(turn_id.to_string()),
            });
        }

        // Finished background children, claimed once for this turn. The
        // session key is ambient: ACP (`orchestrator/acp_server.rs:1478`), RPC
        // (`rpc/turn.rs:69`) and gateway-WS (`gateway/ws.rs:995`) each scope it
        // directly around this call, so this pipeline reads it rather than
        // inventing one — and each of those is an outer entry point, so this
        // turn is the claimant for that key and needs no ownership gate of the
        // kind `agent::run` carries for its nested case.
        // It rides in the user message, which is what puts it in `history` and
        // in `new_msgs` — the parent can refer back to what it was told,
        // exactly like every other per-turn context at this site.
        let (announcements, guard) = crate::agent::loop_::claim_announcements_for_turn(true).await;
        let now = self.current_turn_datetime().format("%Y-%m-%d %H:%M:%S %Z");
        let enriched = format!("{announcements}[{now}] {user_message}");

        let user_msg = ConversationMessage::Chat(ChatMessage::user(enriched));
        new_msgs.push(user_msg.clone());
        self.history.push(user_msg);
        guard
    }

    pub fn set_memory_session_id(&mut self, session_id: Option<String>) {
        self.memory_session_id = session_id;
    }

    pub fn set_temperature(&mut self, temperature: Option<f64>) {
        self.temperature = temperature;
    }

    pub fn refresh_memory_embedder(
        &self,
        model_provider: &str,
        api_key: Option<&str>,
        model: &str,
        dimensions: usize,
    ) {
        self.memory
            .refresh_embedder(model_provider, api_key, model, dimensions);
    }

    #[cfg(test)]
    pub fn temperature_for_test(&self) -> Option<f64> {
        self.temperature
    }

    pub fn set_model_name(&mut self, model_name: String) {
        self.model_name = model_name;
    }

    pub fn set_model_provider(&mut self, model_provider: Box<dyn ModelProvider>) {
        self.model_provider = model_provider;
    }

    pub fn set_model_provider_name(&mut self, model_provider_name: String) {
        self.model_provider_name = model_provider_name;
    }

    pub fn set_tool_dispatcher(&mut self, tool_dispatcher: Box<dyn ToolDispatcher>) {
        self.tool_dispatcher = tool_dispatcher;
        self.refresh_system_prompt();
    }

    fn refresh_system_prompt(&mut self) {
        let Some(ConversationMessage::Chat(first)) = self.history.first() else {
            return;
        };
        if first.role != "system" {
            return;
        }
        if let Ok(sys) = self.build_system_prompt() {
            self.history[0] = ConversationMessage::Chat(ChatMessage::system(sys));
        }
    }

    #[cfg(test)]
    pub fn tool_names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    #[cfg(test)]
    pub fn system_prompt_for_test(&self) -> Result<String> {
        self.build_system_prompt()
    }

    #[cfg(test)]
    pub async fn execute_tool_for_test(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Option<anyhow::Result<zeroclaw_api::tool::ToolResult>> {
        let tool = crate::agent::tool_execution::find_tool(&self.tools, name)?;
        Some(tool.execute(args).await)
    }

    pub fn seed_history(&mut self, messages: &[ChatMessage]) {
        let _ = self.seed_history_with_event(messages);
    }

    /// Hydrate prior chat messages and return a transport event when restoring
    /// the history enforces the structured message cap.
    pub fn seed_history_with_event(&mut self, messages: &[ChatMessage]) -> Option<TurnEvent> {
        if self.history.is_empty()
            && let Ok(sys) = self.build_system_prompt()
        {
            self.history
                .push(ConversationMessage::Chat(ChatMessage::system(sys)));
        }
        for msg in messages {
            if msg.role != "system" {
                self.history.push(ConversationMessage::Chat(msg.clone()));
            }
        }
        self.trim_history(None)
            .map(HistoryTrimNotice::into_turn_event)
    }

    /// Hydrate the agent with a full `ConversationMessage` history (e.g. restored
    /// from an ACP session store). Preserves all variants including `AssistantToolCalls`
    /// and `ToolResults` — use this for ACP restore; use `seed_history` for flat
    /// channel session hydration.
    pub fn seed_conversation_history(&mut self, messages: Vec<ConversationMessage>) {
        let _ = self.seed_conversation_history_with_event(messages);
    }

    /// Hydrate structured conversation history and return a transport event
    /// when restoring the history enforces the structured message cap.
    pub fn seed_conversation_history_with_event(
        &mut self,
        messages: Vec<ConversationMessage>,
    ) -> Option<TurnEvent> {
        if self.history.is_empty()
            && let Ok(sys) = self.build_system_prompt()
        {
            self.history
                .push(ConversationMessage::Chat(ChatMessage::system(sys)));
        }
        for msg in messages {
            // Skip system messages from the seed — the system prompt is already prepended above.
            if matches!(&msg, ConversationMessage::Chat(m) if m.role == "system") {
                continue;
            }
            self.history.push(msg);
        }
        // Trim immediately so pre_len snapshots (taken before the first turn)
        // are always within the configured limit; otherwise a long restored
        // history would cause history[pre_len..] to panic after trim_history
        // shrinks the vec below pre_len during the turn.
        self.trim_history(None)
            .map(HistoryTrimNotice::into_turn_event)
    }

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
        sop_engine: Option<Arc<std::sync::Mutex<SopEngine>>>,
        sop_audit: Option<Arc<SopAuditLogger>>,
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
            sop_engine,
            sop_audit,
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
        sop_engine: Option<Arc<std::sync::Mutex<SopEngine>>>,
        sop_audit: Option<Arc<SopAuditLogger>>,
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
            sop_engine,
            sop_audit,
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
        sop_engine: Option<Arc<std::sync::Mutex<SopEngine>>>,
        sop_audit: Option<Arc<SopAuditLogger>>,
    ) -> Result<Self> {
        Self::from_config_with_session_cwd_and_mcp_approval_mode(
            config,
            agent_alias,
            session_cwd,
            initialize_mcp,
            true,
            exclude_memory,
            tui_env,
            sop_engine,
            sop_audit,
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
        sop_engine: Option<Arc<std::sync::Mutex<SopEngine>>>,
        sop_audit: Option<Arc<SopAuditLogger>>,
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
            sop_engine,
            sop_audit,
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
        sop_engine: Option<Arc<std::sync::Mutex<SopEngine>>>,
        sop_audit: Option<Arc<SopAuditLogger>>,
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

        // SOP loading is gated on `[sop] sops_dir`: unset disables all SOP
        // runtime behavior, matching the documented rollback path.
        // If caller provided an engine (daemon path), use it; otherwise
        // build our own (CLI/standalone path) only when the gate is set.
        let (sop_engine, sop_audit) = match (sop_engine, sop_audit) {
            (Some(engine), Some(audit)) => (Some(engine), Some(audit)),
            (None, None) if config.sop.sops_dir.is_some() => {
                let mem: Arc<dyn zeroclaw_memory::Memory> =
                    zeroclaw_memory::create_memory_for_agent(config, agent_alias, None).await?;
                // CLI / standalone path: no channel map is wired here, so the route
                // adapter is the no-op (log-only). The daemon path builds the SOP
                // engine with a real channel-delivering adapter instead.
                let (engine, audit) = crate::sop::build_sop_engine(
                    config.sop.clone(),
                    &config.data_dir,
                    mem,
                    Default::default(),
                );
                (Some(engine), Some(audit))
            }
            _ => (None, None),
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
            sop_engine,
            sop_audit,
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
        };

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

    fn trim_history(&mut self, turn_id: Option<&str>) -> Option<HistoryTrimNotice> {
        let max = self
            .structured_history_cap_resolver
            .as_ref()
            .map_or(self.config.resolved.max_history_messages, |resolve| {
                resolve()
            });
        if self.history.len() <= max {
            return None;
        }
        let result = crate::agent::history_trim::trim_conversation_to_recent_turns(
            std::mem::take(&mut self.history),
            max,
            self.history_has_trim_breadcrumb,
        );
        self.history = result.history;
        if !result.trimmed {
            return None;
        }

        crate::agent::history_trim::insert_conversation_breadcrumb(&mut self.history);
        self.history_has_trim_breadcrumb = true;
        let reason = crate::i18n::get_required_cli_string("history-trim-reason-message-cap");
        let channel = self.channel_name.clone();
        let agent_alias = self.observer_agent_alias();
        let turn_id = turn_id.map(str::to_owned);

        {
            let scope_span = ::zeroclaw_log::info_span!(
                target: "zeroclaw_log_internal_scope",
                "zeroclaw_scope",
                agent_alias = ::zeroclaw_log::field::Empty,
                channel = %channel,
                trace_id = ::zeroclaw_log::field::Empty,
            );
            if let Some(agent_alias) = agent_alias.as_deref() {
                scope_span.record("agent_alias", agent_alias);
            }
            if let Some(turn_id) = turn_id.as_deref() {
                scope_span.record("trace_id", turn_id);
            }
            let _scope_guard = scope_span.enter();
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Complete)
                    .with_category(::zeroclaw_log::EventCategory::Agent)
                    .with_outcome(::zeroclaw_log::EventOutcome::Success)
                    .with_attrs(::serde_json::json!({
                        "max_history_messages": max,
                        "dropped_messages": result.dropped_messages,
                        "dropped_turns": result.dropped_turns,
                        "kept_turns": result.kept_turns,
                        "remaining_messages": self.history.len(),
                    })),
                "trim_history: dropped oldest whole turns"
            );
        }

        self.observer.record_event(&ObserverEvent::HistoryTrimmed {
            dropped_messages: result.dropped_messages,
            kept_turns: result.kept_turns,
            reason: reason.clone(),
            channel: Some(channel),
            agent_alias,
            turn_id,
        });

        Some(HistoryTrimNotice {
            dropped_messages: result.dropped_messages,
            kept_turns: result.kept_turns,
            reason,
        })
    }

    fn append_receipts_block(
        &self,
        response: String,
        scope: Option<&crate::agent::tool_receipts::ReceiptScope>,
    ) -> String {
        if !self.config.resolved.tool_receipts.show_in_response {
            return response;
        }
        let Some(scope) = scope else {
            return response;
        };
        let block = {
            let receipts = scope.collector().lock().unwrap_or_else(|e| e.into_inner());
            crate::agent::tool_receipts::render_receipts_block(&receipts)
        };
        match block {
            Some(block) => {
                if response.is_empty() {
                    block
                } else {
                    format!("{response}\n\n{block}")
                }
            }
            None => response,
        }
    }

    /// Append a user-visible notice when the resilient provider wrapper served
    /// this turn with a different model or provider than requested (silent
    /// model downgrade, e.g. a `fallback_models` entry kicking in). The record
    /// is consumed from the `zeroclaw_providers::reliable` task-local (single
    /// source of truth); nothing is stored.
    ///
    /// The notice is BOTH appended to the returned response (rendered by
    /// consumers of the final text, e.g. the gateway web UI's `done` frame)
    /// and streamed as a trailing [`TurnEvent::Chunk`] (rendered by streaming
    /// consumers that discard the final text on a clean finish, e.g. the
    /// ZeroCode TUI).
    async fn append_model_fallback_notice(
        response: String,
        fallback: Option<&zeroclaw_providers::reliable::ProviderFallbackInfo>,
        event_tx: &tokio::sync::mpsc::Sender<TurnEvent>,
    ) -> String {
        let Some(fallback) = fallback else {
            return response;
        };
        // The wrapper also records plain retries (attempt > 0 on the primary
        // entry); an identical requested/served pair is not a downgrade.
        if fallback.actual_provider == fallback.requested_provider
            && fallback.actual_model == fallback.requested_model
        {
            return response;
        }
        let notice = crate::i18n::get_required_cli_string_with_args(
            "turn-model-fallback-notice",
            &[
                ("requested_model", fallback.requested_model.as_str()),
                ("requested_provider", fallback.requested_provider.as_str()),
                ("actual_model", fallback.actual_model.as_str()),
                ("actual_provider", fallback.actual_provider.as_str()),
            ],
        );
        let delta = format!("\n\n{notice}");
        let _ = event_tx
            .send(TurnEvent::Chunk {
                delta: delta.clone(),
            })
            .await;
        if response.is_empty() {
            notice
        } else {
            format!("{response}{delta}")
        }
    }

    fn build_system_prompt(&self) -> Result<String> {
        self.build_system_prompt_with_dispatcher(self.tool_dispatcher.as_ref())
    }

    fn build_system_prompt_with_dispatcher(
        &self,
        dispatcher: &dyn ToolDispatcher,
    ) -> Result<String> {
        let expose_text_tool_protocol =
            !self.config.resolved.strict_tool_parsing || dispatcher.should_send_tool_specs();
        let no_tools: Vec<Box<dyn Tool>> = Vec::new();
        let prompt_tools = if expose_text_tool_protocol {
            &self.tools
        } else {
            &no_tools
        };
        let instructions = dispatcher.prompt_instructions(prompt_tools);
        let ctx = PromptContext {
            workspace_dir: &self.workspace_dir,
            agent_workspace_dir: &self.agent_workspace_dir,
            model_name: &self.model_name,
            tools: prompt_tools,
            skills: &self.skills,
            skills_prompt_mode: self.skills_prompt_mode,
            identity_config: Some(&self.identity_config),
            dispatcher_instructions: &instructions,
            sends_native_tool_specs: dispatcher.should_send_tool_specs()
                && !prompt_tools.is_empty(),
            security_summary: self.security_summary.clone(),
            autonomy_level: self.autonomy_level,
        };
        let mut prompt = self.prompt_builder.build(&ctx)?;
        let receipts = &self.config.resolved.tool_receipts;
        if receipts.enabled && receipts.inject_system_prompt {
            prompt.push_str(crate::agent::tool_receipts::SYSTEM_PROMPT_ADDENDUM);
        }
        if !self.mcp_deferred_section.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&self.mcp_deferred_section);
        }
        if !self.mcp_pinned_section.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&self.mcp_pinned_section);
        }
        Ok(prompt)
    }

    fn rebuild_system_prompt_for_dispatcher(
        &mut self,
        dispatcher: &dyn ToolDispatcher,
    ) -> Result<()> {
        let new_prompt = self.build_system_prompt_with_dispatcher(dispatcher)?;
        let Some(ConversationMessage::Chat(first)) = self.history.first_mut() else {
            return Ok(());
        };
        if first.role != "system" {
            return Ok(());
        }
        first.content = new_prompt;
        Ok(())
    }

    fn try_apply_model_switch(
        &mut self,
        current_effective_model: &str,
        new_model_provider: String,
        new_model: String,
    ) -> Option<String> {
        // Same-provider, same-model: nothing to do. The request is owned by
        // the completed tool-loop scope, so there is no persistent slot to clear.
        if new_model_provider == self.model_provider_name && new_model == current_effective_model {
            return None;
        }

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            &format!(
                "Model switch detected in turn_streamed: {} {} -> {} {}",
                self.model_provider_name, current_effective_model, new_model_provider, new_model
            )
        );

        let switch_outcome: anyhow::Result<Box<dyn ModelProvider>> = match self
            .provider_switch_config
            .as_ref()
            .and_then(|cfg| cfg.config.as_ref())
        {
            Some(full_config) => {
                let agent_entry = full_config
                    .resolved_model_provider_for_agent(&self.agent_alias)
                    .map(|(_ty, _alias, entry)| entry);
                let default_api_key = agent_entry.and_then(|e| e.api_key.as_deref());
                let default_base_url = agent_entry.and_then(|e| e.uri.as_deref());

                // Prefer a route-specific api_key when the switched
                // provider/model matches a configured model_route entry.
                let route_api_key = full_config
                    .model_routes
                    .iter()
                    .find(|r| {
                        r.model_provider.eq_ignore_ascii_case(&new_model_provider)
                            && (r.model.eq_ignore_ascii_case(&new_model)
                                || r.hint.eq_ignore_ascii_case(&new_model))
                    })
                    .and_then(|r| r.api_key.as_deref());
                let api_key = route_api_key.or(default_api_key);

                let runtime_options = new_model_provider
                    .split_once('.')
                    .map(|(family, alias)| {
                        zeroclaw_providers::provider_runtime_options_for_alias(
                            full_config.as_ref(),
                            family,
                            alias,
                        )
                    })
                    .unwrap_or_default();

                zeroclaw_providers::create_routed_model_provider_with_options(
                    full_config.as_ref(),
                    &new_model_provider,
                    api_key,
                    default_base_url,
                    &full_config.reliability,
                    &full_config.model_routes,
                    &new_model,
                    &runtime_options,
                )
            }
            None => Err(anyhow::Error::msg(
                "model_switch requested but agent has no provider_switch_config; \
                 cannot rebuild provider safely",
            )),
        };

        match switch_outcome {
            Ok(new_prov) => {
                // Commit state only after the provider was built
                // successfully.
                self.model_provider = new_prov;
                self.model_provider_name = new_model_provider;
                self.model_name = new_model.clone();
                Some(new_model)
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"err": e.to_string()})),
                    &format!(
                        "Failed to apply model_switch in turn_streamed; staying on {} {}",
                        self.model_provider_name, current_effective_model
                    )
                );
                None
            }
        }
    }

    fn classify_model(&self, user_message: &str) -> String {
        if let Some(decision) =
            super::classifier::classify_with_decision(&self.classification_config, user_message)
            && self.available_hints.contains(&decision.hint)
        {
            let resolved_model = self
                .route_model_by_hint
                .get(&decision.hint)
                .map(String::as_str)
                .unwrap_or("unknown");
            ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"hint": decision.hint.as_str(), "model": resolved_model, "rule_priority": decision.priority, "message_length": user_message.len()})), "Classified message route");
            return format!("hint:{}", decision.hint);
        }

        // Fallback: auto-classify by complexity when no rule matched.
        if let Some(ref ac) = self.config.resolved.auto_classify {
            let tier = super::eval::estimate_complexity(user_message);
            if let Some(hint) = ac.hint_for(tier)
                && self.available_hints.contains(&hint.to_string())
            {
                ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"hint": hint, "complexity": format!("{:?}", tier), "message_length": user_message.len()})), "Auto-classified by complexity");
                return format!("hint:{hint}");
            }
        }

        self.model_name.clone()
    }

    fn replay_loop_messages(loop_messages: &[ChatMessage]) -> Vec<ConversationMessage> {
        let mut replayed: Vec<ConversationMessage> = Vec::with_capacity(loop_messages.len());
        let push_tool_results = |replayed: &mut Vec<ConversationMessage>,
                                 results: Vec<ToolResultMessage>| {
            if let Some(ConversationMessage::ToolResults(previous)) = replayed.last_mut() {
                previous.extend(results);
            } else {
                replayed.push(ConversationMessage::ToolResults(results));
            }
        };
        for msg in loop_messages {
            if msg.role == "assistant"
                && let Ok(serde_json::Value::Object(obj)) =
                    serde_json::from_str::<serde_json::Value>(&msg.content)
                && let Some(calls) = obj.get("tool_calls").and_then(|c| c.as_array())
                && !calls.is_empty()
                && calls.iter().all(|c| {
                    c.get("id").is_some_and(serde_json::Value::is_string)
                        && c.get("name").is_some_and(serde_json::Value::is_string)
                })
            {
                let tool_calls = calls
                    .iter()
                    .map(|c| zeroclaw_providers::ToolCall {
                        id: c
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        name: c
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        arguments: c
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        extra_content: None,
                    })
                    .collect();
                replayed.push(ConversationMessage::AssistantToolCalls {
                    text: obj
                        .get("content")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    tool_calls,
                    reasoning_content: obj
                        .get("reasoning_content")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                });
                continue;
            }
            if msg.role == "tool" {
                if let Ok(vals) = serde_json::from_str::<Vec<serde_json::Value>>(&msg.content) {
                    let results: Vec<ToolResultMessage> = vals
                        .into_iter()
                        .filter_map(|v| {
                            Some(ToolResultMessage {
                                tool_call_id: v.get("tool_call_id")?.as_str()?.to_string(),
                                content: v
                                    .get("content")
                                    .and_then(|c| c.as_str())
                                    .unwrap_or_default()
                                    .to_string(),
                                // Provider-wire tool messages do not carry the
                                // producing tool name; replayed results fall back
                                // to blind canonicalization
                                tool_name: String::new(),
                            })
                        })
                        .collect();
                    if !results.is_empty() {
                        push_tool_results(&mut replayed, results);
                        continue;
                    }
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg.content) {
                    let result = ToolResultMessage {
                        tool_call_id: v
                            .get("tool_call_id")
                            .and_then(|id| id.as_str())
                            .unwrap_or("unknown")
                            .to_string(),
                        content: v
                            .get("content")
                            .and_then(|c| c.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        // No provenance on the provider-wire shape; blind canon
                        // applies as before
                        tool_name: String::new(),
                    };
                    push_tool_results(&mut replayed, vec![result]);
                    continue;
                }
            }
            replayed.push(ConversationMessage::Chat(msg.clone()));
        }
        replayed
    }

    pub async fn turn(&mut self, user_message: &str) -> Result<String> {
        if user_message.trim().is_empty() {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_category(::zeroclaw_log::EventCategory::Agent)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "reason": "empty_user_message",
                        "entry_point": "Agent::turn",
                        "raw_len": user_message.len(),
                    })),
                "Refusing blank user turn (would emit timestamp-only message and risk prompt-template bleed-through)"
            );
            return Err(anyhow::Error::msg(
                "empty user message: refusing to dispatch a blank turn",
            ));
        }

        if self.history.is_empty() {
            let system_prompt = self.build_system_prompt()?;
            self.history
                .push(ConversationMessage::Chat(ChatMessage::system(
                    system_prompt,
                )));
        }

        let effective_model = self.classify_model(user_message);

        let turn_id = Self::new_turn_id();
        let turn_observer = Arc::clone(&self.observer);
        let mut guard = crate::observability::AgentTurnGuard::start(
            turn_observer.as_ref(),
            self.model_provider_name.clone(),
            effective_model.clone(),
            Some(self.channel_name.clone()),
            self.observer_agent_alias(),
            Some(turn_id.clone()),
        );

        // Memory context is injected once in the engine, keyed on the
        // ingress origin (agent::memory_inject).
        if self.auto_save {
            let store_start = std::time::Instant::now();
            let store_result = self
                .memory
                .store(
                    "user_msg",
                    user_message,
                    MemoryCategory::Conversation,
                    self.memory_session_id.as_deref(),
                )
                .await;
            self.observer.record_event(&ObserverEvent::MemoryStore {
                category: MemoryCategory::Conversation.to_string(),
                backend: self.memory.name().to_string(),
                duration: store_start.elapsed(),
                success: store_result.is_ok(),
                channel: Some(self.channel_name.clone()),
                agent_alias: self.observer_agent_alias(),
                turn_id: Some(turn_id.clone()),
            });
        }

        let now = self.current_turn_datetime();
        let (year, month, day) = (now.year(), now.month(), now.day());
        let (hour, minute, second) = (now.hour(), now.minute(), now.second());
        let tz = now.format("%Z");
        let date_str =
            format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} {tz}");

        // Same claim-once-per-turn contract as
        // `append_streamed_user_message_to_history`; `turn` builds its user
        // message inline instead of going through that helper, so it holds its
        // own guard and settles it at this function's success exits.
        let (announcements, announcement_guard) =
            crate::agent::loop_::claim_announcements_for_turn(true).await;
        let enriched =
            format!("{announcements}[CURRENT DATE & TIME: {date_str}]\n\n{user_message}");

        self.history
            .push(ConversationMessage::Chat(ChatMessage::user(enriched)));

        let active_dispatcher = {
            let base_provider_messages = self.tool_dispatcher.to_provider_messages(&self.history);
            let (vision_provider_box, _degrade_strip_images) =
                match crate::agent::turn::resolve_vision_provider(
                    self.full_config(),
                    self.model_provider.as_ref(),
                    &base_provider_messages,
                    &self.multimodal_config,
                    &self.model_provider_name,
                    &effective_model,
                ) {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        let _ = self.trim_history(Some(&turn_id));
                        return Err(error);
                    }
                };
            let active_provider: &dyn ModelProvider = vision_provider_box
                .as_ref()
                .map(|resolved| resolved.provider.as_ref())
                .unwrap_or(self.model_provider.as_ref());
            tool_dispatcher_for_provider(&self.config, active_provider)
        };

        if let Err(error) = self.rebuild_system_prompt_for_dispatcher(active_dispatcher.as_ref()) {
            let _ = self.trim_history(Some(&turn_id));
            return Err(error);
        }

        let provider_messages = active_dispatcher.to_provider_messages(&self.history);
        let cache_key = self.response_cache_key_for_messages(&provider_messages, &effective_model);

        if let (Some(cache), Some(key)) = (&self.response_cache, &cache_key) {
            if let Ok(Some(cached)) = cache.get(key) {
                self.observer.record_event(&ObserverEvent::CacheHit {
                    cache_type: "response".into(),
                    tokens_saved: 0,
                });
                self.history
                    .push(ConversationMessage::Chat(ChatMessage::assistant(
                        cached.clone(),
                    )));
                let _ = self.trim_history(Some(&turn_id));
                // Cache hit: no provider call, but the block is durably in
                // `self.history` for the next turn. See the same exit in
                // `turn_streamed_with_steering_state`.
                return crate::agent::loop_::settle_announcement_guards(
                    announcement_guard,
                    Ok(cached),
                );
            }
            self.observer.record_event(&ObserverEvent::CacheMiss {
                cache_type: "response".into(),
            });
        }

        // Split provider_messages: loop_history gets past turns only,
        // loop_new_messages gets this turn's user message so the hook
        // can observe/modify it. Seed loop_history with the user message so
        // the provider sees it without clone-and-append.
        let split_idx = provider_messages
            .iter()
            .rposition(|m| m.role == "user")
            .unwrap_or(provider_messages.len());
        let mut loop_history = provider_messages[..split_idx].to_vec();
        let mut loop_new_messages: Vec<ChatMessage> = provider_messages[split_idx..].to_vec();

        let knobs = crate::agent::loop_::LoopKnobs {
            dedup_enabled: false,
            max_iteration_behavior: crate::agent::loop_::MaxIterationBehavior::ErrorAtCap,
            detect_protocol_without_tools: false,
        };
        // E3 never had pattern-based loop detection; default pacing turns it
        // on. Keep the embedder contract (an N-step identical-args tool chain
        // completes) until the Agent surface grows a pacing config of its own.
        let pacing = zeroclaw_config::schema::PacingConfig {
            loop_detection_enabled: false,
            ..zeroclaw_config::schema::PacingConfig::default()
        };

        // Keep the loop call as a plain `.await` on this task. Caller-scoped
        // task-locals (session key, cost tracking, tool choice / thinking
        // overrides) silently vanish across a spawn.
        let cost_context = self.tool_loop_cost_tracking_context();
        let receipt_scope = crate::agent::tool_receipts::ReceiptScope::from_config(
            &self.config.resolved.tool_receipts,
        );
        let agent_alias_for_loop = self.observer_agent_alias();
        let loop_result = crate::agent::loop_::TOOL_LOOP_COST_TRACKING_CONTEXT
            .scope(
                Some(cost_context.clone()),
                crate::agent::tool_receipts::scope_receipts(
                    receipt_scope.clone(),
                    crate::agent::loop_::run_tool_call_loop(crate::agent::loop_::ToolLoop {
                        exec: crate::agent::loop_::ResolvedAgentExecution::resolve(
                            crate::agent::loop_::ResolvedModelAccess {
                                model_provider: self.model_provider.as_ref(),
                                provider_name: &self.model_provider_name,
                                model: &effective_model,
                                temperature: self.temperature,
                            },
                            crate::agent::loop_::ResolvedIo {
                                tools_registry: &self.tools,
                                observer: self.observer.as_ref(),
                                silent: false,
                                approval: self.approval_manager.as_deref(),
                                multimodal_config: &self.multimodal_config,
                                // Inlined `full_config()` (per-field borrow) so it coexists with
                                // the `&mut self.image_cache` in this same ToolLoop expression.
                                config: self
                                    .provider_switch_config
                                    .as_ref()
                                    .and_then(|c| c.config.as_deref()),
                                hooks: self.hook_runner.as_deref(),
                                activated_tools: self.activated_tools.as_ref(),
                                model_switch_callback: None,
                                receipt_generator: receipt_scope
                                    .as_ref()
                                    .map(crate::agent::tool_receipts::ReceiptScope::generator),
                            },
                            crate::agent::loop_::ResolvedRuntimeKnobs {
                                max_tool_iterations: self.config.resolved.max_tool_iterations,
                                excluded_tools: &[],
                                dedup_exempt_tools: &self.config.resolved.tool_call_dedup_exempt,
                                pacing: &pacing,
                                strict_tool_parsing: self.config.resolved.strict_tool_parsing,
                                parallel_tools: self.config.resolved.parallel_tools,
                                max_tool_result_chars: self.config.resolved.max_tool_result_chars,
                                context_token_budget: self
                                    .config
                                    .resolved
                                    .effective_context_budget(),
                                knobs: &knobs,
                            },
                        ),
                        history: &mut loop_history,
                        channel_name: &self.channel_name,
                        channel_reply_target: None,
                        cancellation_token: None,
                        on_delta: None,
                        shared_budget: None,
                        channel: None,
                        collected_receipts: receipt_scope
                            .as_ref()
                            .map(crate::agent::tool_receipts::ReceiptScope::collector),
                        event_tx: None,
                        steering: None,
                        new_messages_out: Some(&mut loop_new_messages),
                        image_cache: Some(&mut self.image_cache),
                        // Direct embedded Agent::turn call; source/transport/
                        // trust stay placeholders, not yet stamped at the edge.
                        memory: Some(crate::agent::memory_inject::TurnMemory {
                            handle: self.memory.as_ref(),
                            query: user_message.to_string(),
                            sessions: vec![self.memory_session_id.clone()],
                            suppress: false,
                            cfg: self.memory_inject_cfg,
                        }),
                        ingress: zeroclaw_api::ingress::IngressContext::agent_direct(),
                        agent_alias: agent_alias_for_loop.as_deref(),
                        parent_agent_alias: None,
                        turn_id: &turn_id,
                        // Live-daemon SOP path: re-assemble a nested step's agent
                        // when it delegates elsewhere. Config survives only via
                        // `provider_switch_config`; with `None` (test builder) a
                        // cross-agent step FAILS CLOSED rather than inheriting
                        // this turn's context.
                        sop_reassembly: self
                            .provider_switch_config
                            .as_ref()
                            .and_then(|c| c.config.as_deref())
                            .map(|config| crate::agent::turn::SopStepReassembly { config }),
                    }),
                ),
            )
            .await;

        // Feed the accumulated per-call usage into the AgentEnd guard before
        // any return below drops it — including the error path, which must
        // still report usage from calls that succeeded earlier in the turn.
        let usage = cost_context.snapshot_turn_usage();
        if usage.input_tokens > 0 || usage.output_tokens > 0 {
            guard.set_usage(
                Some(zeroclaw_api::observability_traits::TurnTokenUsage {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                }),
                None,
            );
        }
        // Pop the original user message (pushed before the loop) so the
        // replayed version — which includes the user message, possibly
        // modified by the hook.
        self.history.pop();
        for replayed in Self::replay_loop_messages(&loop_new_messages) {
            self.history.push(replayed);
        }
        let response = match loop_result {
            Ok(response) => response,
            Err(error) => {
                let _ = self.trim_history(Some(&turn_id));
                return Err(error);
            }
        };

        let response = self.append_receipts_block(response, receipt_scope.as_ref());

        // Store in the response cache only when the turn was a single
        // tool-free exchange (exactly one assistant message), mirroring the
        // old "no tool calls" put condition.
        if let (Some(cache), Some(key)) = (&self.response_cache, &cache_key)
            && loop_new_messages.len() == 2
            && loop_new_messages
                .last()
                .is_some_and(|m| m.role == "assistant")
        {
            #[allow(clippy::cast_possible_truncation)]
            let _ = cache.put(key, &effective_model, &response, usage.output_tokens as u32);
        }

        let _ = self.trim_history(Some(&turn_id));

        // Success point: the tool loop returned, so the provider was called
        // with the history containing the announcement block. Every other exit
        // above leaves the guard to drop armed.
        crate::agent::loop_::settle_announcement_guards(announcement_guard, Ok(response))
    }

    pub async fn turn_streamed(
        &mut self,
        user_message: &str,
        event_tx: tokio::sync::mpsc::Sender<TurnEvent>,
        cancel_token: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<(String, Vec<ConversationMessage>)> {
        // See `Agent::turn` for the rationale. Same guard: blank input would
        // push a timestamp-only user message into history and the model would
        // narrate the trailing prompt-template sentinel instead of replying.
        if user_message.trim().is_empty() {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_category(::zeroclaw_log::EventCategory::Agent)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "reason": "empty_user_message",
                        "entry_point": "Agent::turn_streamed",
                        "raw_len": user_message.len(),
                    })),
                "Refusing blank user turn (would emit timestamp-only message and risk prompt-template bleed-through)"
            );
            return Err(anyhow::Error::msg(
                "empty user message: refusing to dispatch a blank turn",
            ));
        }

        self.turn_streamed_with_steering_state(user_message, event_tx, cancel_token, None)
            .await
            .map(|outcome| (outcome.response, outcome.new_messages))
            .map_err(|err| err.error)
    }

    pub async fn turn_streamed_with_steering_state(
        &mut self,
        user_message: &str,
        event_tx: tokio::sync::mpsc::Sender<TurnEvent>,
        cancel_token: Option<tokio_util::sync::CancellationToken>,
        mut steering_rx: Option<&mut tokio::sync::mpsc::Receiver<String>>,
    ) -> std::result::Result<StreamedTurnSuccess, StreamedTurnError> {
        // See `Agent::turn` for the rationale. Same guard: blank input would
        // push a timestamp-only user message into history and the model would
        // narrate the trailing prompt-template sentinel instead of replying.
        if user_message.trim().is_empty() {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_category(::zeroclaw_log::EventCategory::Agent)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "reason": "empty_user_message",
                        "entry_point": "Agent::turn_streamed_with_steering_state",
                        "raw_len": user_message.len(),
                    })),
                "Refusing blank user turn (would emit timestamp-only message and risk prompt-template bleed-through)"
            );
            return Err(StreamedTurnError {
                error: anyhow::Error::msg("empty user message: refusing to dispatch a blank turn"),
                committed_response: String::new(),
                new_messages: Vec::new(),
            });
        }

        // ── Preamble (identical to turn) ───────────────────────────────
        if self.history.is_empty() {
            let system_prompt = self
                .build_system_prompt()
                .map_err(|error| StreamedTurnError {
                    error,
                    committed_response: String::new(),
                    new_messages: Vec::new(),
                })?;
            self.history
                .push(ConversationMessage::Chat(ChatMessage::system(
                    system_prompt,
                )));
        }

        let mut new_msgs: Vec<ConversationMessage> = Vec::new();
        // `effective_model` is `mut` so a `model_switch` requested mid-turn
        // (handled in the round loop's `ModelSwitchRequested` arm via
        // `try_apply_model_switch`) can rebind it for later rounds
        let mut effective_model = self.classify_model(user_message);
        let turn_id = Self::new_turn_id();
        let mut committed_response = String::new();
        // Requested-vs-served divergence for THIS turn. Source of truth is the
        // task-local record inside `zeroclaw_providers::reliable`, consumed
        // once per round below; this is a per-turn transient resolved at
        // use-time, never stored on the agent.
        let mut turn_model_fallback: Option<zeroclaw_providers::reliable::ProviderFallbackInfo> =
            None;
        let turn_observer = Arc::clone(&self.observer);
        let mut guard = crate::observability::AgentTurnGuard::start(
            turn_observer.as_ref(),
            self.model_provider_name.clone(),
            effective_model.clone(),
            Some(self.channel_name.clone()),
            self.observer_agent_alias(),
            Some(turn_id.clone()),
        );
        // One guard per user message claimed into this turn: the opening one
        // here, plus one for each mid-turn steering message drained below.
        // They are settled together against this turn's one outcome at every
        // success exit; any other exit drops them armed and the announcements
        // go back to the store.
        let mut announcement_guards: Vec<crate::agent::loop_::UnclaimOnDrop> = Vec::new();
        announcement_guards.extend(
            self.append_streamed_user_message_to_history(user_message, &mut new_msgs, &turn_id)
                .await,
        );

        let active_dispatcher = {
            let base_provider_messages = self.tool_dispatcher.to_provider_messages(&self.history);
            let (vision_provider_box, _degrade_strip_images) =
                match crate::agent::turn::resolve_vision_provider(
                    self.full_config(),
                    self.model_provider.as_ref(),
                    &base_provider_messages,
                    &self.multimodal_config,
                    &self.model_provider_name,
                    &effective_model,
                ) {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        let notice = self.trim_history(Some(&turn_id));
                        forward_history_trim_notice(&event_tx, notice).await;
                        return Err(StreamedTurnError {
                            error,
                            committed_response: String::new(),
                            new_messages: new_msgs,
                        });
                    }
                };
            let active_provider: &dyn ModelProvider = vision_provider_box
                .as_ref()
                .map(|resolved| resolved.provider.as_ref())
                .unwrap_or(self.model_provider.as_ref());
            tool_dispatcher_for_provider(&self.config, active_provider)
        };

        if let Err(error) = self.rebuild_system_prompt_for_dispatcher(active_dispatcher.as_ref()) {
            let notice = self.trim_history(Some(&turn_id));
            forward_history_trim_notice(&event_tx, notice).await;
            return Err(StreamedTurnError {
                error,
                committed_response: String::new(),
                new_messages: new_msgs,
            });
        }

        let provider_messages = active_dispatcher.to_provider_messages(&self.history);
        let cache_key = self.response_cache_key_for_messages(&provider_messages, &effective_model);

        if let (Some(cache), Some(key)) = (&self.response_cache, &cache_key) {
            if let Ok(Some(cached)) = cache.get(key) {
                self.observer.record_event(&ObserverEvent::CacheHit {
                    cache_type: "response".into(),
                    tokens_saved: 0,
                });
                let cached_msg = ConversationMessage::Chat(ChatMessage::assistant(cached.clone()));
                new_msgs.push(cached_msg.clone());
                self.history.push(cached_msg);
                let notice = self.trim_history(Some(&turn_id));
                forward_history_trim_notice(&event_tx, notice).await;
                self.observer.record_event(&ObserverEvent::TurnComplete);
                committed_response.push_str(&cached);
                // A cache hit is a completed turn: no provider call happened,
                // but the announcement block is in `self.history`, which this
                // pipeline carries into the next turn — so the news is not
                // lost, and returning it to the store would show it twice.
                return crate::agent::loop_::settle_announcement_guards(
                    announcement_guards,
                    Ok(StreamedTurnSuccess {
                        response: committed_response,
                        new_messages: new_msgs,
                    }),
                );
            }
            self.observer.record_event(&ObserverEvent::CacheMiss {
                cache_type: "response".into(),
            });
        }

        // Split provider_messages: loop_history gets past turns, user_msg_for_loop
        // seeds round 0's round_added so the hook can observe/modify the user message.
        let split_idx = provider_messages
            .iter()
            .rposition(|m| m.role == "user")
            .unwrap_or(provider_messages.len());
        let mut loop_history = provider_messages[..split_idx].to_vec();
        let user_msg_for_loop: Vec<ChatMessage> = provider_messages[split_idx..].to_vec();

        let approval_bridge: Option<Box<dyn zeroclaw_api::channel::Channel>> =
            self.channel_handles.ask_user.as_ref().map(|handles| {
                Box::new(crate::agent::approval_bridge::AskUserApprovalBridge::new(
                    Arc::clone(handles),
                    self.approval_route.clone(),
                )) as Box<dyn zeroclaw_api::channel::Channel>
            });

        let knobs = crate::agent::loop_::LoopKnobs {
            dedup_enabled: false,
            max_iteration_behavior: crate::agent::loop_::MaxIterationBehavior::GracefulSummary,
            detect_protocol_without_tools: false,
        };
        // The streaming engine never had pattern-based loop detection; default
        // pacing turns it on. Keep the embedder contract until this surface
        // grows a pacing config of its own (matches `Agent::turn`).
        let pacing = zeroclaw_config::schema::PacingConfig {
            loop_detection_enabled: false,
            ..zeroclaw_config::schema::PacingConfig::default()
        };

        let cost_context = self.tool_loop_cost_tracking_context();
        let agent_alias_for_loop = self.observer_agent_alias();

        // Built once per turn so the HMAC key is stable across steering rounds
        // and the same collector accumulates every round's receipts. `None`
        // when receipts are disabled, gated by the one shared seam.
        let receipt_scope = crate::agent::tool_receipts::ReceiptScope::from_config(
            &self.config.resolved.tool_receipts,
        );

        // ── Round loop: one tool-call-loop run per steering round ──────────
        for round in 0..self.config.resolved.max_tool_iterations {
            // Early exit if the caller cancelled this turn (e.g. user abort)
            if cancel_token
                .as_ref()
                .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
            {
                let marker = crate::i18n::get_required_cli_string("turn-interrupted-by-user");
                let interruption =
                    ConversationMessage::Chat(ChatMessage::assistant(marker.clone()));
                new_msgs.push(interruption.clone());
                self.history.push(interruption);
                committed_response.push_str(&marker);
                let notice = self.trim_history(Some(&turn_id));
                forward_history_trim_notice(&event_tx, notice).await;
                return Err(StreamedTurnError {
                    error: crate::agent::loop_::ToolLoopCancelled.into(),
                    committed_response,
                    new_messages: new_msgs,
                });
            }

            let mut round_added: Vec<ChatMessage> = if round == 0 {
                user_msg_for_loop.clone()
            } else {
                Vec::new()
            };

            // Steering drain: each accepted mid-turn message becomes its own
            // enriched user turn in both transcripts before the next round.
            //
            // This claims again inside a turn that already claimed once, and
            // that is deliberate, not an oversight: a steering message is a
            // fresh user turn in everything but name, it gets its own round
            // with the provider, and any child that finished since the turn
            // started is news the model can still act on. Delivering it with
            // the message that triggered it is better than holding it back
            // until the whole turn ends.
            for steering_message in crate::agent::loop_::drain_steering_messages(&mut steering_rx) {
                // Mirror the enrichment logic from append_streamed_user_message_to_history
                // but route through round_added instead of self.history/new_msgs, so
                // the before-llm-call hook sees steering messages as round-local context
                // without polluting the durable history (PR-A behavioral equivalence).
                if self.auto_save {
                    let store_start = std::time::Instant::now();
                    let store_result = self
                        .memory
                        .store(
                            "user_msg",
                            &steering_message,
                            MemoryCategory::Conversation,
                            self.memory_session_id.as_deref(),
                        )
                        .await;
                    self.observer.record_event(&ObserverEvent::MemoryStore {
                        category: MemoryCategory::Conversation.to_string(),
                        backend: self.memory.name().to_string(),
                        duration: store_start.elapsed(),
                        success: store_result.is_ok(),
                        channel: Some(self.channel_name.clone()),
                        agent_alias: self.observer_agent_alias(),
                        turn_id: Some(turn_id.clone()),
                    });
                }
                // Claim finished background children for this steering turn —
                // same as append_streamed_user_message_to_history, but routed to
                // round_added instead of self.history.
                let (announcements, guard) =
                    crate::agent::loop_::claim_announcements_for_turn(true).await;
                if let Some(g) = guard {
                    announcement_guards.push(g);
                }
                let now = self.current_turn_datetime().format("%Y-%m-%d %H:%M:%S %Z");
                let enriched = format!("{announcements}[{now}] {steering_message}");
                round_added.push(ChatMessage::user(enriched));
            }
            let round_loop = crate::agent::loop_::TOOL_LOOP_COST_TRACKING_CONTEXT.scope(
                Some(cost_context.clone()),
                crate::agent::tool_receipts::scope_receipts(
                    receipt_scope.clone(),
                    crate::agent::loop_::run_tool_call_loop(crate::agent::loop_::ToolLoop {
                        exec: crate::agent::loop_::ResolvedAgentExecution::resolve(
                            crate::agent::loop_::ResolvedModelAccess {
                                model_provider: self.model_provider.as_ref(),
                                provider_name: &self.model_provider_name,
                                model: &effective_model,
                                temperature: self.temperature,
                            },
                            crate::agent::loop_::ResolvedIo {
                                tools_registry: &self.tools,
                                observer: self.observer.as_ref(),
                                silent: true,
                                approval: self.approval_manager.as_deref(),
                                multimodal_config: &self.multimodal_config,
                                // Inlined `full_config()` (per-field borrow) so it coexists with
                                // the `&mut self.image_cache` in this same ToolLoop expression.
                                config: self
                                    .provider_switch_config
                                    .as_ref()
                                    .and_then(|c| c.config.as_deref()),
                                hooks: self.hook_runner.as_deref(),
                                activated_tools: self.activated_tools.as_ref(),
                                // `None` here (rather than a shared global) is
                                // deliberate: `run_tool_call_loop` mints a fresh,
                                // task-local switch state for this round when it
                                // sees `None`, so a `model_switch` requested this
                                // round can never leak into a sibling round or a
                                // concurrently running turn/agent.
                                model_switch_callback: None,
                                receipt_generator: receipt_scope
                                    .as_ref()
                                    .map(crate::agent::tool_receipts::ReceiptScope::generator),
                            },
                            crate::agent::loop_::ResolvedRuntimeKnobs {
                                max_tool_iterations: self.config.resolved.max_tool_iterations,
                                excluded_tools: &[],
                                dedup_exempt_tools: &self.config.resolved.tool_call_dedup_exempt,
                                pacing: &pacing,
                                strict_tool_parsing: self.config.resolved.strict_tool_parsing,
                                parallel_tools: self.config.resolved.parallel_tools,
                                max_tool_result_chars: self.config.resolved.max_tool_result_chars,
                                context_token_budget: self
                                    .config
                                    .resolved
                                    .effective_context_budget(),
                                knobs: &knobs,
                            },
                        ),
                        history: &mut loop_history,
                        channel_name: &self.channel_name,
                        channel_reply_target: None,
                        cancellation_token: cancel_token.clone(),
                        on_delta: None,
                        shared_budget: None,
                        channel: approval_bridge.as_deref(),
                        collected_receipts: receipt_scope
                            .as_ref()
                            .map(crate::agent::tool_receipts::ReceiptScope::collector),
                        event_tx: Some(event_tx.clone()),
                        steering: None,
                        new_messages_out: Some(&mut round_added),
                        image_cache: Some(&mut self.image_cache),
                        // Direct embedded Agent::turn call; source/transport/
                        // trust stay placeholders, not yet stamped at the edge.
                        memory: Some(crate::agent::memory_inject::TurnMemory {
                            handle: self.memory.as_ref(),
                            query: user_message.to_string(),
                            sessions: vec![self.memory_session_id.clone()],
                            suppress: false,
                            cfg: self.memory_inject_cfg,
                        }),
                        ingress: zeroclaw_api::ingress::IngressContext::agent_direct(),
                        agent_alias: agent_alias_for_loop.as_deref(),
                        parent_agent_alias: None,
                        turn_id: &turn_id,
                        // Live-daemon SOP path: re-assemble a nested step's
                        // agent when it delegates elsewhere. Config survives
                        // only via `provider_switch_config`; with `None`
                        // (test builder) a cross-agent step FAILS CLOSED
                        // rather than inheriting this turn's context.
                        sop_reassembly: self
                            .provider_switch_config
                            .as_ref()
                            .and_then(|c| c.config.as_deref())
                            .map(|config| crate::agent::turn::SopStepReassembly { config }),
                    }),
                ),
            );
            // Scope the provider-fallback task-local around the round so the
            // resilient wrapper's requested-vs-served record is visible here,
            // then read it immediately (same pattern as the channels
            // orchestrator's `scope_provider_fallback` wrapping). Box::pin
            // moves the round future to the heap: nesting it inside another
            // async block otherwise grows the turn future past the tokio
            // worker stack in debug builds (observed live as a worker-thread
            // stack overflow aborting the gateway).
            let (loop_result, round_fallback) =
                zeroclaw_providers::reliable::scope_provider_fallback(async {
                    let result = Box::pin(round_loop).await;
                    (
                        result,
                        zeroclaw_providers::reliable::take_last_provider_fallback(),
                    )
                })
                .await;
            if round_fallback.is_some() {
                turn_model_fallback = round_fallback;
            }

            // Feed cumulative usage into the AgentEnd guard before any return
            // below drops it — the error paths must still report usage from
            // calls that succeeded earlier in the turn.
            let usage = cost_context.snapshot_turn_usage();
            if usage.input_tokens > 0 || usage.output_tokens > 0 {
                guard.set_usage(
                    Some(zeroclaw_api::observability_traits::TurnTokenUsage {
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                    }),
                    None,
                );
            }

            // round_added now contains the user message for round 0;
            // a single tool-free exchange is [user, assistant].
            let single_text_exchange = round == 0
                && round_added.len() == 2
                && round_added.first().is_some_and(|m| m.role == "user")
                && round_added.last().is_some_and(|m| m.role == "assistant");

            if round == 0 {
                self.history.pop();
                new_msgs.pop();
            }
            for replayed in Self::replay_loop_messages(&round_added) {
                new_msgs.push(replayed.clone());
                self.history.push(replayed);
            }

            match loop_result {
                Ok(response) => {
                    // Commit-before-drain: this round's assistant output is in
                    // history/new_msgs (replay above) and committed_response
                    // before any steering continuation is folded in.
                    committed_response.push_str(&response);
                    let notice = self.trim_history(Some(&turn_id));
                    forward_history_trim_notice(&event_tx, notice).await;

                    let has_more_steering =
                        steering_rx.as_deref_mut().is_some_and(|rx| !rx.is_empty());
                    if has_more_steering {
                        continue;
                    }

                    // Cache put only when the turn was a single tool-free
                    // exchange, mirroring the old "no tool calls" condition.
                    if single_text_exchange
                        && let (Some(cache), Some(key)) = (&self.response_cache, &cache_key)
                    {
                        #[allow(clippy::cast_possible_truncation)]
                        let _ =
                            cache.put(key, &effective_model, &response, usage.output_tokens as u32);
                    }

                    self.observer.record_event(&ObserverEvent::TurnComplete);
                    let committed_response =
                        self.append_receipts_block(committed_response, receipt_scope.as_ref());
                    let committed_response = Self::append_model_fallback_notice(
                        committed_response,
                        turn_model_fallback.as_ref(),
                        &event_tx,
                    )
                    .await;
                    // Success point: the round loop returned, which means the
                    // provider was called and the model read every user
                    // message this turn appended — the opening one and each
                    // steering message. A steering continuation `continue`s
                    // above without settling, so this runs once per turn.
                    return crate::agent::loop_::settle_announcement_guards(
                        announcement_guards,
                        Ok(StreamedTurnSuccess {
                            response: committed_response,
                            new_messages: new_msgs,
                        }),
                    );
                }
                Err(error) => {
                    // Model switch requested mid-turn: the unified loop
                    // signals a pending `model_switch` by returning
                    // `ModelSwitchRequested`. The
                    // round's tool call + result are already replayed into
                    // history/new_msgs above; rebuild the provider from the
                    // captured `ProviderSwitchConfig` and continue the round
                    // loop so the next provider call uses the switched
                    // provider/model. A failed rebuild (no switch config / build
                    // error) falls through to the normal error handling below.
                    if let Some((new_model_provider, new_model)) =
                        crate::agent::loop_::is_model_switch_requested(&error)
                        && let Some(new_effective_model) = self.try_apply_model_switch(
                            &effective_model,
                            new_model_provider,
                            new_model,
                        )
                    {
                        let notice = self.trim_history(Some(&turn_id));
                        forward_history_trim_notice(&event_tx, notice).await;
                        effective_model = new_effective_model;
                        continue;
                    }
                    // Rebuild the committed text from the failed round's plain
                    // assistant output (e.g. a persisted stream partial) when
                    // no prior round committed anything.
                    if committed_response.is_empty() {
                        for replayed in Self::replay_loop_messages(&round_added) {
                            if let ConversationMessage::Chat(message) = &replayed
                                && message.role == "assistant"
                            {
                                committed_response.push_str(&message.content);
                            }
                        }
                    }
                    let error = if crate::agent::loop_::is_tool_loop_cancelled(&error) {
                        // When the cancel arrived after event-visible
                        // streamed text, the error itself carries the
                        // partial the loop persisted (replayed into
                        // history/new_msgs above, and into
                        // committed_response by the empty-committed
                        // rebuild). Provenance, not content sniffing:
                        // model-authored text can end with the marker
                        // literal, so suffix-matching round_added would
                        // misfire. Synthesize the bare marker only when no
                        // interruption text was persisted this round.
                        let marker =
                            crate::i18n::get_required_cli_string("turn-interrupted-by-user");
                        let persisted_interruption = error
                            .downcast_ref::<crate::agent::loop_::StreamCancelledAfterOutput>()
                            .map(|cancelled| format!("{}\n\n{marker}", cancelled.partial_text));
                        match persisted_interruption {
                            Some(text) => {
                                if !committed_response.ends_with(&marker) {
                                    if !committed_response.is_empty() {
                                        committed_response.push_str("\n\n");
                                    }
                                    committed_response.push_str(&text);
                                }
                            }
                            None => {
                                committed_response.push_str(&marker);
                                let interruption = ConversationMessage::Chat(
                                    ChatMessage::assistant(marker.clone()),
                                );
                                new_msgs.push(interruption.clone());
                                self.history.push(interruption);
                            }
                        }
                        crate::agent::loop_::ToolLoopCancelled.into()
                    } else {
                        // Mark the interruption only when nothing was committed —
                        // prior-round text must round-trip unmodified.
                        if committed_response.is_empty() {
                            committed_response.push_str(&crate::i18n::get_required_cli_string(
                                "turn-stream-interrupted",
                            ));
                        }
                        error
                    };
                    let notice = self.trim_history(Some(&turn_id));
                    forward_history_trim_notice(&event_tx, notice).await;
                    return Err(StreamedTurnError {
                        error,
                        committed_response,
                        new_messages: new_msgs,
                    });
                }
            }
        }

        let notice = self.trim_history(Some(&turn_id));
        forward_history_trim_notice(&event_tx, notice).await;
        Err(StreamedTurnError {
            error: anyhow::Error::msg(format!(
                "Agent exceeded maximum tool iterations ({})",
                self.config.resolved.max_tool_iterations
            )),
            committed_response,
            new_messages: new_msgs,
        })
    }

    pub async fn run_single(&mut self, message: &str) -> Result<String> {
        self.turn(message).await
    }

    pub async fn run_interactive(&mut self) -> Result<()> {
        println!("🦀 ZeroClaw Interactive Mode");
        println!("Type /quit to exit.\n");

        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let cli = crate::agent::loop_::CLI_CHANNEL_FN
            .get()
            .expect("CLI channel factory not registered — call register_cli_channel_fn at startup")(
        );

        let listen_handle = zeroclaw_spawn::spawn!(async move {
            let _ = zeroclaw_api::channel::Channel::listen(&*cli, tx).await;
        });

        while let Some(msg) = rx.recv().await {
            let response = match self.turn(&msg.content).await {
                Ok(resp) => resp,
                Err(e) => {
                    eprintln!("\nError: {e}\n");
                    continue;
                }
            };
            println!("\n{response}\n");
        }

        listen_handle.abort();
        Ok(())
    }
}

pub async fn run(
    config: Config,
    agent_alias: &str,
    message: Option<String>,
    provider_override: Option<String>,
    model_override: Option<String>,
    temperature: Option<f64>,
) -> Result<()> {
    let mut effective_config = config;
    if let Some(ref p) = provider_override {
        // When a model_provider override is specified, ensure that model_provider type exists
        // in models and update the agent's model_provider to reference it.
        let (type_key, alias_key) = p.split_once('.').unwrap_or((p.as_str(), agent_alias));
        effective_config
            .providers
            .models
            .ensure(type_key, alias_key);
        if let Some(agent_cfg) = effective_config.agents.get_mut(agent_alias) {
            agent_cfg.model_provider = format!("{type_key}.{alias_key}").into();
        }
    }
    // Apply model/temperature overrides to the agent's resolved provider entry.
    if let Some(agent_cfg) = effective_config.agents.get(agent_alias)
        && let Some((fam, ali)) = agent_cfg.model_provider.split_once('.')
        && let Some(entry) = effective_config.providers.models.ensure(fam, ali)
    {
        if let Some(m) = model_override {
            entry.model = Some(m);
        }
        entry.temperature = temperature;
    }

    let mut agent = Agent::from_config(&effective_config, agent_alias).await?;

    if let Some(msg) = message {
        let response = agent.run_single(&msg).await?;
        println!("{response}");
    } else {
        agent.run_interactive().await?;
    }

    Ok(())
}

// safety net (child module so fixtures can reach Agent internals the
// same way `mod tests` does).
#[cfg(test)]
#[path = "safety_net.rs"]
mod safety_net;

#[cfg(test)]
#[path = "parity.rs"]
mod parity;


#[cfg(test)]
#[path = "agent_inline_tests.rs"]
mod tests;

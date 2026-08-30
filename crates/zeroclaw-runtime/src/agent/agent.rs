#[cfg(test)]
use crate::agent::dispatcher::NativeToolDispatcher;
use crate::agent::dispatcher::ToolDispatcher;
use crate::agent::eval::AutoClassifyExt;
use crate::agent::prompt::{PromptContext, SystemPromptBuilder};
use crate::approval::ApprovalManager;
#[cfg(test)]
use crate::observability;
use crate::observability::{Observer, ObserverEvent};
#[cfg(all(test, feature = "heavy-tests"))]
use crate::security::SecurityPolicy;
use crate::tools::{self, Tool};
use anyhow::Result;
use chrono::{Datelike, Timelike};
use std::collections::HashMap;
#[cfg(test)]
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

mod construction;
mod turn_entry;

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

    #[cfg(all(test, feature = "heavy-tests"))]
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

    #[cfg(all(test, feature = "heavy-tests"))]
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
    /// Called once per user message, which includes each mid-turn steering
    /// message.
    async fn append_streamed_user_message_to_history(
        &mut self,
        user_message: &str,
        new_msgs: &mut Vec<ConversationMessage>,
        turn_id: &str,
    ) {
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

        let now = self.current_turn_datetime().format("%Y-%m-%d %H:%M:%S %Z");
        let enriched = format!("[{now}] {user_message}");

        let user_msg = ConversationMessage::Chat(ChatMessage::user(enriched));
        new_msgs.push(user_msg.clone());
        self.history.push(user_msg);
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

// Heavy suite gated so lib-test iteration does not pay 6.9k lines; CI runtime leg enables it.
#[cfg(all(test, feature = "heavy-tests"))]
#[path = "agent_inline_tests.rs"]
mod tests;

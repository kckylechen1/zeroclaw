//! Tool subsystem for agent-callable capabilities.

pub mod attribution;
pub mod cron_add;
pub(crate) mod cron_common;
pub mod cron_list;
pub mod cron_remove;
pub mod cron_run;
pub mod cron_runs;
pub mod cron_update;
pub mod file_read;
pub mod model_switch;
pub mod param_options;
#[cfg(test)]
mod provider_wire_budget;
pub mod read_skill;
mod runtime_command_error;
pub mod schedule;
pub mod scoped;
pub mod security_ops;
pub mod send_message_to_peer;
pub mod shell;
pub mod skill_http;
pub mod skill_manage;
pub mod skill_tool;
pub mod todo_write;
pub mod verifiable_intent;

// Tool types from zeroclaw-tools (direct imports, no shims)
pub use zeroclaw_tools::ask_user::AskUserTool;
pub use zeroclaw_tools::ask_user::ChannelMapHandle;
pub use zeroclaw_tools::backup_tool::BackupTool;
pub use zeroclaw_tools::browser::{BrowserTool, ComputerUseConfig};
pub use zeroclaw_tools::browser_open::BrowserOpenTool;
pub use zeroclaw_tools::calculator::CalculatorTool;
pub use zeroclaw_tools::canvas::{ALLOWED_CONTENT_TYPES, MAX_CONTENT_SIZE};
pub use zeroclaw_tools::canvas::{CanvasStore, CanvasTool};
pub use zeroclaw_tools::channel_room::ChannelRoomTool;
pub use zeroclaw_tools::cloud_ops::CloudOpsTool;
pub use zeroclaw_tools::cloud_patterns::CloudPatternsTool;
#[cfg(feature = "integrations-saas")]
pub use zeroclaw_tools::composio::ComposioTool;
pub use zeroclaw_tools::content_search::ContentSearchTool;
pub use zeroclaw_tools::data_management::DataManagementTool;
pub use zeroclaw_tools::discord_search::DiscordSearchTool;
pub use zeroclaw_tools::email_read::EmailReadTool;
pub use zeroclaw_tools::email_search::EmailSearchTool;
pub use zeroclaw_tools::escalate::EscalateToHumanTool;
pub use zeroclaw_tools::file_download::FileDownloadTool;
pub use zeroclaw_tools::file_edit::FileEditTool;
pub use zeroclaw_tools::file_upload::FileUploadTool;
pub use zeroclaw_tools::file_upload_bundle::FileUploadBundleTool;
pub use zeroclaw_tools::file_write::FileWriteTool;
pub use zeroclaw_tools::git_forge::GitForgeTool;
pub use zeroclaw_tools::git_operations::GitOperationsTool;
pub use zeroclaw_tools::glob_search::GlobSearchTool;
#[cfg(feature = "integrations-saas")]
pub use zeroclaw_tools::google_workspace::GoogleWorkspaceTool;
#[cfg(feature = "hardware-tools")]
pub use zeroclaw_tools::hardware_board_info::HardwareBoardInfoTool;
#[cfg(feature = "hardware-tools")]
pub use zeroclaw_tools::hardware_memory_map::HardwareMemoryMapTool;
#[cfg(feature = "hardware-tools")]
pub use zeroclaw_tools::hardware_memory_read::HardwareMemoryReadTool;
pub use zeroclaw_tools::http_request::HttpRequestTool;
pub use zeroclaw_tools::image_gen::ImageGenTool;
pub use zeroclaw_tools::image_info::ImageInfoTool;
#[cfg(feature = "integrations-saas")]
pub use zeroclaw_tools::jira_tool::JiraTool;
pub use zeroclaw_tools::knowledge_tool::KnowledgeTool;
#[cfg(feature = "integrations-saas")]
pub use zeroclaw_tools::linkedin::LinkedInTool;
pub use zeroclaw_tools::llm_task::LlmTaskTool;
pub use zeroclaw_tools::mcp_client::{McpRegistry, McpServer};
pub use zeroclaw_tools::mcp_context;
pub use zeroclaw_tools::mcp_deferred::{
    ActivatedToolSet, DeferredMcpToolSet, build_deferred_tools_section,
    build_deferred_tools_section_excluding, build_deferred_tools_section_filtered,
};
pub use zeroclaw_tools::mcp_prompts_tool::McpPromptsTool;
pub use zeroclaw_tools::mcp_resources_tool::McpResourcesTool;
pub use zeroclaw_tools::mcp_tool::McpToolWrapper;
pub use zeroclaw_tools::memory_export::MemoryExportTool;
pub use zeroclaw_tools::memory_forget::MemoryForgetTool;
pub use zeroclaw_tools::memory_purge::MemoryPurgeTool;
pub use zeroclaw_tools::memory_recall::MemoryRecallTool;
pub use zeroclaw_tools::memory_store::MemoryStoreTool;
#[cfg(feature = "integrations-saas")]
pub use zeroclaw_tools::microsoft365::Microsoft365Tool;
pub use zeroclaw_tools::model_routing_config::ModelRoutingConfigTool;
#[cfg(feature = "integrations-saas")]
pub use zeroclaw_tools::notion_tool::NotionTool;
pub use zeroclaw_tools::pipeline::PipelineTool;
pub use zeroclaw_tools::poll::PollTool;
pub use zeroclaw_tools::project_intel::ProjectIntelTool;
pub use zeroclaw_tools::proxy_config::ProxyConfigTool;
#[cfg(feature = "integrations-saas")]
pub use zeroclaw_tools::pushover::PushoverTool;
pub use zeroclaw_tools::reaction::ReactionTool;
pub use zeroclaw_tools::report_template_tool::ReportTemplateTool;
pub use zeroclaw_tools::screenshot::ScreenshotTool;
pub use zeroclaw_tools::send_via::{
    AgentPeerGroupResolver, SendViaTool, TURN_ROUTING, TurnRoutingHandle,
};
pub use zeroclaw_tools::sessions::{
    SessionDeleteTool, SessionResetTool, SessionsCurrentTool, SessionsHistoryTool,
    SessionsListTool, SessionsSendTool,
};
pub use zeroclaw_tools::text_browser::TextBrowserTool;
pub use zeroclaw_tools::tool_search::ToolSearchTool;
pub use zeroclaw_tools::weather_tool::WeatherTool;
pub use zeroclaw_tools::web_fetch::WebFetchTool;
pub use zeroclaw_tools::web_search_tool::WebSearchTool;
pub use zeroclaw_tools::wrappers::{PathGuardedTool, RateLimitedTool};

// Traits from zeroclaw-api
pub use zeroclaw_api::schema::{CleaningStrategy, SchemaCleanr};
pub use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult, ToolSpec};

// Local tool re-exports (tools with root deps, kept in misc)
pub use cron_add::CronAddTool;
pub use cron_list::CronListTool;
pub use cron_remove::CronRemoveTool;
pub use cron_run::CronRunTool;
pub use cron_runs::CronRunsTool;
pub use cron_update::CronUpdateTool;
pub use file_read::FileReadTool;
pub use model_switch::ModelSwitchTool;
pub use read_skill::ReadSkillTool;
pub use schedule::ScheduleTool;
pub use security_ops::SecurityOpsTool;
pub use send_message_to_peer::SendMessageToPeerTool;
pub use shell::ShellTool;
pub use skill_http::SkillHttpTool;
pub use skill_tool::{SkillBuiltinTool, SkillShellTool};
pub use todo_write::TodoWriteTool;
pub use verifiable_intent::VerifiableIntentTool;

/// Re-entrant agent-spawning tools that must never be collapsed by the
/// per-turn duplicate-call guard: launching several with the same prompt
/// (redundancy, sampling, fan-out) is intentional, not an accidental
/// repeat. Unioned with config-provided exemptions in the tool-call loop.
pub const REENTRANT_AGENT_TOOLS: &[&str] = &[
    // `spawn_subagent` retired from this list with the spawn_subagent wall;
    // the V1 entrypoint is the surviving re-entrant spawn surface.
    crate::subagent_v1::ReasoningSubagentTool::NAME,
];

use crate::platform::{NativeRuntime, RuntimeAdapter};
use crate::security::{SecurityPolicy, create_sandbox};
use async_trait::async_trait;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use zeroclaw_config::schema::{AliasedAgentConfig, Config};
use zeroclaw_memory::Memory;

pub type PerToolChannelHandle =
    Arc<RwLock<HashMap<String, Arc<dyn zeroclaw_api::channel::Channel>>>>;

/// Thin wrapper that makes an `Arc<dyn Tool>` usable as `Box<dyn Tool>`.
pub struct ArcToolRef(pub Arc<dyn Tool>);
// ArcToolRef is the public constructor name for ArcToolWrapper

#[async_trait]
impl Tool for ArcToolRef {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn description(&self) -> &str {
        self.0.description()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.0.parameters_schema()
    }

    fn output_schema(&self) -> Option<serde_json::Value> {
        self.0.output_schema()
    }

    fn param_domains(&self) -> Vec<(&'static str, ::zeroclaw_api::tool::OptionDomain)> {
        self.0.param_domains()
    }

    // Forward `spec()` so inner overrides keep their `Arc`-shared parameter
    // schemas; the trait default would rebuild the spec from
    // `parameters_schema()`, deep-cloning MCP schemas every loop iteration.
    fn spec(&self) -> zeroclaw_api::tool::ToolSpec {
        self.0.spec()
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.0.execute(args).await
    }
}

#[derive(Clone)]
struct ArcDelegatingTool {
    inner: Arc<dyn Tool>,
}

impl ArcDelegatingTool {
    fn boxed(inner: Arc<dyn Tool>) -> Box<dyn Tool> {
        Box::new(Self { inner })
    }
}

impl ::zeroclaw_api::attribution::Attributable for ArcDelegatingTool {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        self.inner.role()
    }
    fn alias(&self) -> &str {
        self.inner.alias()
    }
}

#[async_trait]
impl Tool for ArcDelegatingTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }

    fn output_schema(&self) -> Option<serde_json::Value> {
        self.inner.output_schema()
    }

    fn param_domains(&self) -> Vec<(&'static str, ::zeroclaw_api::tool::OptionDomain)> {
        self.inner.param_domains()
    }

    // Forward `spec()` so inner overrides keep their `Arc`-shared parameter
    // schemas; the trait default would rebuild the spec from
    // `parameters_schema()`, deep-cloning MCP schemas every loop iteration.
    fn spec(&self) -> zeroclaw_api::tool::ToolSpec {
        self.inner.spec()
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.inner.execute(args).await
    }
}

fn boxed_registry_from_arcs(tools: Vec<Arc<dyn Tool>>) -> Vec<Box<dyn Tool>> {
    tools.into_iter().map(ArcDelegatingTool::boxed).collect()
}

/// Create the default tool registry
pub fn default_tools(security: Arc<SecurityPolicy>) -> Vec<Box<dyn Tool>> {
    default_tools_with_runtime(security, Arc::new(NativeRuntime::new()))
}

/// Create the default tool registry with explicit runtime adapter.
pub fn default_tools_with_runtime(
    security: Arc<SecurityPolicy>,
    runtime: Arc<dyn RuntimeAdapter>,
) -> Vec<Box<dyn Tool>> {
    let persistent_writes = runtime.has_filesystem_access();
    vec![
        Box::new(RateLimitedTool::new(
            PathGuardedTool::new(
                ShellTool::new(security.clone(), runtime).with_persistent_writes(persistent_writes),
                security.clone(),
            ),
            security.clone(),
        )),
        Box::new(RateLimitedTool::new(
            PathGuardedTool::new(
                FileReadTool::new_with_persistence(security.clone(), persistent_writes),
                security.clone(),
            ),
            security.clone(),
        )),
        Box::new(RateLimitedTool::new(
            PathGuardedTool::new(
                FileWriteTool::new_with_persistence(security.clone(), persistent_writes),
                security.clone(),
            ),
            security.clone(),
        )),
        Box::new(RateLimitedTool::new(
            PathGuardedTool::new(
                FileEditTool::new_with_persistence(security.clone(), persistent_writes),
                security.clone(),
            ),
            security.clone(),
        )),
        Box::new(RateLimitedTool::new(
            PathGuardedTool::new(GlobSearchTool::new(security.clone()), security.clone()),
            security.clone(),
        )),
        Box::new(RateLimitedTool::new(
            PathGuardedTool::new(ContentSearchTool::new(security.clone()), security.clone()),
            security,
        )),
    ]
}

pub fn register_skill_tools(
    tools_registry: &mut Vec<Box<dyn Tool>>,
    skills: &[crate::skills::Skill],
    security: Arc<SecurityPolicy>,
) {
    register_skill_tools_with_context(tools_registry, skills, security, &[]);
}

/// Register skill-defined tools with full context for builtin kinds.
/// `unfiltered_registry` provides the pre-policy tool list for `kind = "builtin"`
/// delegation.
pub fn register_skill_tools_with_context(
    tools_registry: &mut Vec<Box<dyn Tool>>,
    skills: &[crate::skills::Skill],
    security: Arc<SecurityPolicy>,
    unfiltered_registry: &[Arc<dyn Tool>],
) {
    register_skill_tools_with_context_and_runtime(
        tools_registry,
        skills,
        security,
        unfiltered_registry,
        Arc::new(NativeRuntime::new()),
    );
}

pub fn register_skill_tools_with_context_and_runtime(
    tools_registry: &mut Vec<Box<dyn Tool>>,
    skills: &[crate::skills::Skill],
    security: Arc<SecurityPolicy>,
    unfiltered_registry: &[Arc<dyn Tool>],
    runtime: Arc<dyn RuntimeAdapter>,
) {
    if skills.is_empty() {
        return;
    }

    let before = tools_registry.len();
    let policy = Arc::clone(&security);
    let skill_tools = crate::skills::skills_to_tools_with_context_and_runtime(
        skills,
        security,
        unfiltered_registry,
        runtime,
    );
    let existing_names: std::collections::HashSet<String> = tools_registry
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    for tool in skill_tools {
        if existing_names.contains(tool.name()) {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                &format!(
                    "Skill tool '{}' shadows built-in tool, skipping",
                    tool.name()
                )
            );
        } else if policy.is_tool_excluded(tool.name()) {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                &format!(
                    "Skill tool '{}' denied by excluded_tools, skipping",
                    tool.name()
                )
            );
        } else {
            tools_registry.push(tool);
        }
    }
    let registered = tools_registry.len() - before;

    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
        &format!(
            "Registered {} skill tool(s) from {} skill(s): {}",
            registered,
            skills.len(),
            skills
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        )
    );
}

pub async fn collect_mcp_elevation_arcs(registry: &Arc<McpRegistry>) -> Vec<Arc<dyn Tool>> {
    let mut arcs: Vec<Arc<dyn Tool>> = Vec::new();
    for name in registry.tool_names() {
        if let Some(def) = registry.get_tool_def(&name).await {
            arcs.push(Arc::new(McpToolWrapper::new(
                name,
                def,
                Arc::clone(registry),
            )));
        }
    }
    arcs
}

/// Build the two generic MCP capability tools (`mcp_resources`, `mcp_prompts`),
/// including each only when the access `policy` admits its name. A `None` policy
/// admits both. Returned as `Arc<dyn Tool>` ready to register and/or expose to
/// delegates.
pub fn build_mcp_capability_tools(
    registry: &Arc<McpRegistry>,
    policy: Option<&zeroclaw_tools::tool_search::ToolAccessPolicy>,
) -> Vec<Arc<dyn Tool>> {
    let admit = |name: &str| policy.is_none_or(|p| p.is_tool_allowed(name));
    let mut out: Vec<Arc<dyn Tool>> = Vec::new();
    if admit("mcp_resources") {
        out.push(Arc::new(McpResourcesTool::new(Arc::clone(registry))));
    }
    if admit("mcp_prompts") {
        out.push(Arc::new(McpPromptsTool::new(Arc::clone(registry))));
    }
    out
}

pub const BUILTIN_TOOL_INTEGRATIONS: &[(&str, &str)] = &[
    ("Shell", "Terminal command execution"),
    ("File System", "Read/write files"),
    ("Weather", "Forecasts & conditions (wttr.in)"),
    (
        "Reasoning SubAgent",
        "Run one bounded, contract-admitted reasoning child (V1 entry point)",
    ),
];

/// The registry's reasoning-spawn construction site. Single point where the
/// run's spawn lineage (SA-9) is threaded into the surviving spawn-capable
/// tool, so `registry_rebuild_carries_spawn_lineage_and_cannot_reset_depth`
/// can discriminate a dropped thread-through.
fn reasoning_spawn_tool_for_registry(
    root_config: &zeroclaw_config::schema::Config,
    agent_alias: &str,
    security: &Arc<SecurityPolicy>,
    spawn_lineage: Option<zeroclaw_api::subagent_v1::LineageRef>,
) -> crate::subagent_v1::ReasoningSubagentTool {
    crate::subagent_v1::ReasoningSubagentTool::new(
        Arc::new(root_config.clone()),
        agent_alias,
        security.clone(),
    )
    .with_lineage(spawn_lineage)
}

/// Tool names retired from the ordinary model-visible registry. No assembly
/// path may register them and no plugin may claim the names. Most entries
/// keep their implementations compiled (operator surfaces and tests
/// construct them directly); the SOP run tools below were deleted outright
/// with the legacy run side. Kept as one list so the registry totality test
/// and the plugin collision guard assert the same set.
#[cfg(any(test, feature = "plugins-wasm"))]
pub(crate) const RETIRED_OPERATOR_TOOL_NAMES: &[&str] = &[
    "model_routing_config",
    "model_switch",
    "proxy_config",
    "security_ops",
    "backup",
    "data_management",
    "sop_execute",
    "sop_advance",
    "sop_approve",
    "sop_status",
    "sop_list",
    "sop_workshop",
    "delegate",
    // spawn_subagent wall: the legacy full-Parent-inheritance spawn entry
    // (same-alias child, full `Arc<Config>` clone, parent memory UUID) —
    // retired; `reasoning_subagent` is the single spawn surface.
    "spawn_subagent",
    // Wall 2 prune epic: Parent-visible raw harness/vendor launch surfaces.
    "claude_code",
    "claude_code_runner",
    "codex_cli",
    "gemini_cli",
    "opencode_cli",
    "browser_delegate",
];

/// Bundled return values from tool registry construction.
/// Named struct to avoid an ever-growing positional tuple that's painful
/// to destructure across many callers.
#[allow(clippy::type_complexity)]
pub struct AllToolsResult {
    pub tools: Vec<Box<dyn Tool>>,
    pub ask_user_handle: Option<PerToolChannelHandle>,
    pub channel_room_handle: Option<PerToolChannelHandle>,
    pub reaction_handle: PerToolChannelHandle,
    pub poll_handle: Option<PerToolChannelHandle>,
    pub escalate_handle: Option<PerToolChannelHandle>,
    /// Pre-boxed Arcs of every tool (before policy filter). Used by
    /// skill-scoped builtin elevation to resolve targets at registration.
    pub unfiltered_tool_arcs: Vec<Arc<dyn Tool>>,
}

/// Create full tool registry including memory tools and optional Composio
#[allow(
    clippy::implicit_hasher,
    clippy::too_many_arguments,
    clippy::type_complexity
)]
pub fn all_tools(
    config: Arc<Config>,
    security: &Arc<SecurityPolicy>,
    risk_profile: &zeroclaw_config::schema::RiskProfileConfig,
    agent_alias: &str,
    memory: Arc<dyn Memory>,
    composio_key: Option<&str>,
    composio_entity_id: Option<&str>,
    browser_config: &zeroclaw_config::schema::BrowserConfig,
    http_config: &zeroclaw_config::schema::HttpRequestConfig,
    web_fetch_config: &zeroclaw_config::schema::WebFetchConfig,
    workspace_dir: &std::path::Path,
    // Formerly the delegate tool's agent roster / parent fallback key.
    // `delegate` is retired (wall 1); the parameters stay in the
    // signature (underscored, intentionally unused) so call sites do not
    // churn and the registry contract is unchanged for callers.
    _agents: &HashMap<String, AliasedAgentConfig>,
    _fallback_api_key: Option<&str>,
    root_config: &zeroclaw_config::schema::Config,
    canvas_store: Option<CanvasStore>,
    is_subagent_caller: bool,
    tui_env: Option<HashMap<String, String>>,
) -> AllToolsResult {
    all_tools_with_runtime(
        config,
        security,
        risk_profile,
        agent_alias,
        Arc::new(NativeRuntime::new()),
        memory,
        composio_key,
        composio_entity_id,
        browser_config,
        http_config,
        web_fetch_config,
        workspace_dir,
        _agents,
        _fallback_api_key,
        root_config,
        canvas_store,
        is_subagent_caller,
        tui_env,
        // No runtime adapter / live-config here; and no lineage —
        // callers of the non-runtime variant are top-level origins.
        None,
        None,
    )
}

/// Peer groups that include `agent_alias`, cloned from `config`. Used as the
/// live resolver body for `send_via` authority (and the snapshot fallback).
fn filter_agent_peer_groups(
    config: &Config,
    agent_alias: &str,
) -> HashMap<String, zeroclaw_config::multi_agent::PeerGroupConfig> {
    config
        .peer_groups
        .iter()
        .filter(|(_, pg)| pg.agents.iter().any(|a| a.as_str() == agent_alias))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Create full tool registry including memory tools and optional Composio.
#[allow(
    clippy::implicit_hasher,
    clippy::too_many_arguments,
    clippy::type_complexity
)]
pub fn all_tools_with_runtime(
    config: Arc<Config>,
    security: &Arc<SecurityPolicy>,
    risk_profile: &zeroclaw_config::schema::RiskProfileConfig,
    agent_alias: &str,
    runtime: Arc<dyn RuntimeAdapter>,
    memory: Arc<dyn Memory>,
    composio_key: Option<&str>,
    composio_entity_id: Option<&str>,
    browser_config: &zeroclaw_config::schema::BrowserConfig,
    http_config: &zeroclaw_config::schema::HttpRequestConfig,
    web_fetch_config: &zeroclaw_config::schema::WebFetchConfig,
    workspace_dir: &std::path::Path,
    // Formerly the delegate tool's agent roster / parent fallback key.
    // `delegate` is retired (wall 1); the parameters stay in the
    // signature (underscored, intentionally unused) so call sites do not
    // churn and the registry contract is unchanged for callers.
    _agents: &HashMap<String, AliasedAgentConfig>,
    _fallback_api_key: Option<&str>,
    root_config: &zeroclaw_config::schema::Config,
    canvas_store: Option<CanvasStore>,
    // Formerly the legacy `spawn_subagent` tool's depth-1 self-cap flag.
    // `spawn_subagent` is retired (spawn_subagent wall); the parameter
    // stays in the signature (underscored, intentionally unused) so call
    // sites do not churn and the registry contract is unchanged for
    // callers.
    _is_subagent_caller: bool,
    tui_env: Option<HashMap<String, String>>,
    // Live config handle for `send_via` peer-group authority. `Some` from the
    // channel daemon (so reloads take effect); `None` for one-shot / non-channel
    // callers, which fall back to a snapshot of `root_config`.
    live_config: Option<Arc<parking_lot::RwLock<zeroclaw_config::schema::Config>>>,
    // Unified spawn lineage (SA-9): the lineage of the run this registry
    // is being built for. Spawn-capable tools constructed here carry it,
    // so depth survives registry rebuilds (SA-11) and hops stay on one
    // ledger (SA-10). `None` for top-level origins (the run mints a
    // root) and legacy test callers.
    spawn_lineage: Option<zeroclaw_api::subagent_v1::LineageRef>,
) -> AllToolsResult {
    let persistent_writes = runtime.has_filesystem_access();
    // Composio credentials are only consumed when the SaaS family is compiled
    // in; the parameters stay part of the stable signature for both builds.
    #[cfg(not(feature = "integrations-saas"))]
    let _ = (composio_key, composio_entity_id);
    // `has_shell_access` gates only the SaaS-family gws integration now; the
    // raw launcher registrations that consumed it unconditionally are retired.
    #[cfg(feature = "integrations-saas")]
    let has_shell_access = runtime.has_shell_access();
    let runtime_kind = root_config.runtime.kind.as_wire();
    let sandbox_cfg = risk_profile.sandbox_config();
    let sandbox = create_sandbox(&sandbox_cfg, runtime_kind, Some(&security.workspace_dir));
    let mut tool_arcs: Vec<Arc<dyn Tool>> = vec![
        Arc::new(RateLimitedTool::new(
            PathGuardedTool::new(
                ShellTool::new_with_sandbox(security.clone(), runtime.clone(), sandbox.clone())
                    .with_timeout_secs(if security.shell_timeout_secs > 0 {
                        security.shell_timeout_secs
                    } else {
                        root_config.shell_tool.timeout_secs
                    })
                    .with_tui_env(tui_env)
                    .with_persistent_writes(persistent_writes),
                security.clone(),
            ),
            security.clone(),
        )),
        Arc::new(RateLimitedTool::new(
            PathGuardedTool::new(
                FileReadTool::new_with_persistence(security.clone(), persistent_writes),
                security.clone(),
            ),
            security.clone(),
        )),
        Arc::new(RateLimitedTool::new(
            PathGuardedTool::new(
                FileWriteTool::new_with_persistence(security.clone(), persistent_writes),
                security.clone(),
            ),
            security.clone(),
        )),
        Arc::new(RateLimitedTool::new(
            PathGuardedTool::new(
                FileEditTool::new_with_persistence(security.clone(), persistent_writes),
                security.clone(),
            ),
            security.clone(),
        )),
        Arc::new(RateLimitedTool::new(
            PathGuardedTool::new(GlobSearchTool::new(security.clone()), security.clone()),
            security.clone(),
        )),
        Arc::new(RateLimitedTool::new(
            PathGuardedTool::new(ContentSearchTool::new(security.clone()), security.clone()),
            security.clone(),
        )),
        Arc::new(CronAddTool::new(
            config.clone(),
            security.clone(),
            agent_alias,
        )),
        Arc::new(CronListTool::new(config.clone())),
        Arc::new(CronRemoveTool::new(
            config.clone(),
            security.clone(),
            agent_alias,
        )),
        Arc::new(CronUpdateTool::new(
            config.clone(),
            security.clone(),
            agent_alias,
        )),
        Arc::new(CronRunTool::new(config.clone(), security.clone())),
        Arc::new(CronRunsTool::new(config.clone())),
        Arc::new(MemoryStoreTool::new(memory.clone(), security.clone())),
        Arc::new(MemoryRecallTool::new(memory.clone())),
        Arc::new(MemoryForgetTool::new(memory.clone(), security.clone())),
        Arc::new(MemoryExportTool::new(memory.clone())),
        Arc::new(MemoryPurgeTool::new(memory.clone(), security.clone())),
        Arc::new(ScheduleTool::new(
            security.clone(),
            root_config.clone(),
            agent_alias,
        )),
        Arc::new(reasoning_spawn_tool_for_registry(
            root_config,
            agent_alias,
            security,
            spawn_lineage.clone(),
        )),
        Arc::new(SendMessageToPeerTool::new(
            Arc::new(root_config.clone()),
            agent_alias,
        )),
        // Operator/admin tools are deliberately absent from this registry:
        // model_routing_config, model_switch, and proxy_config mutate
        // routing/proxy state whose authority is operator-level. The
        // trusted surfaces are the gateway config API
        // (PUT/DELETE /api/config...) for routing and proxy config, the
        // channel `/model` command for runtime model switching, and
        // startup application of persisted proxy config. The tool
        // implementations stay compiled and are re-exported below; they
        // are simply never handed to the model.
        Arc::new(GitOperationsTool::new(
            security.clone(),
            workspace_dir.to_path_buf(),
        )),
    ];

    // Pushover notifications are part of the SaaS integration family: only
    // registered when the family is compiled in. Pushed here — between the
    // git tool and the calculator group — so the full build keeps the exact
    // registry position it had before the feature gate.
    #[cfg(feature = "integrations-saas")]
    tool_arcs.push(Arc::new(PushoverTool::new(
        security.clone(),
        workspace_dir.to_path_buf(),
    )));

    tool_arcs.push(Arc::new(CalculatorTool::new()));
    tool_arcs.push(Arc::new(WeatherTool::new()));
    tool_arcs.push(Arc::new(CanvasTool::new(canvas_store.unwrap_or_default())));
    tool_arcs.push(Arc::new(TodoWriteTool::new()));

    // Register discord_search if any configured Discord alias has
    // archive enabled. Multiple Discord aliases are supported (one per
    // bot/server set); the search tool reads from a shared archive DB
    // so it's enabled when at least one alias archives.
    if root_config.channels.discord.values().any(|d| d.archive) {
        match zeroclaw_memory::SqliteMemory::new_named("sqlite", &config.data_dir, "discord") {
            Ok(discord_mem) => {
                tool_arcs.push(Arc::new(DiscordSearchTool::new(Arc::new(discord_mem))));
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "discord_search: failed to open discord.db"
                );
            }
        }
    }

    // email_search — registered when at least one email channel is enabled
    {
        let email_configs: std::collections::HashMap<
            String,
            zeroclaw_config::scattered_types::EmailConfig,
        > = root_config
            .channels
            .email
            .iter()
            .filter(|(_, c)| c.enabled)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        if !email_configs.is_empty() {
            let auth_service = if email_configs.values().any(|c| c.oauth2.is_some()) {
                Some(Arc::new(
                    zeroclaw_providers::auth::AuthService::from_config(root_config),
                ))
            } else {
                None
            };
            let configs = Arc::new(email_configs);
            tool_arcs.push(Arc::new(EmailSearchTool::new(
                Arc::clone(&configs),
                auth_service.clone(),
            )));
            tool_arcs.push(Arc::new(EmailReadTool::new(
                Arc::clone(&configs),
                auth_service,
            )));
        }
    }

    // LLM task tool — registered using the calling agent's provider
    if let Some((family, alias, entry)) = root_config.resolved_model_provider_for_agent(agent_alias)
    {
        let llm_task_provider = family.to_string();
        let llm_task_model = entry
            .model
            .clone()
            .unwrap_or_else(|| "openai/gpt-4o-mini".to_string());
        let llm_task_runtime_options =
            zeroclaw_providers::provider_runtime_options_for_alias(root_config, family, alias);
        tool_arcs.push(Arc::new(LlmTaskTool::new(
            security.clone(),
            llm_task_provider,
            llm_task_model,
            entry.temperature,
            entry.api_key.clone(),
            llm_task_runtime_options,
        )));
    }

    if matches!(
        root_config.effective_skills_prompt_mode(agent_alias),
        zeroclaw_config::schema::SkillsPromptInjectionMode::Compact
    ) {
        // ReadSkillTool now holds full config to support all skill sources:
        // workspace skills, open-skills, agent-bound bundles, and plugin skills.
        tool_arcs.push(Arc::new(ReadSkillTool::new(
            config.clone(),
            agent_alias.to_string(),
        )));
    }

    if browser_config.enabled {
        // Add legacy browser_open tool for simple URL opening
        match BrowserOpenTool::new_with_private_hosts(
            security.clone(),
            browser_config.allowed_domains.clone(),
            browser_config.allowed_private_hosts.clone(),
        ) {
            Ok(tool) => {
                tool_arcs.push(Arc::new(tool));
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "browser_open: failed to construct tool, skipping registration"
                );
            }
        }
        // Add full browser automation tool (pluggable backend)
        match BrowserTool::new_with_backend(
            security.clone(),
            browser_config.allowed_domains.clone(),
            browser_config.session_name.clone(),
            browser_config.backend.clone(),
            browser_config.headed,
            browser_config.native_headless,
            browser_config.native_webdriver_url.clone(),
            browser_config.native_chrome_path.clone(),
            ComputerUseConfig {
                endpoint: browser_config.computer_use.endpoint.clone(),
                api_key: browser_config.computer_use.api_key.clone(),
                timeout_ms: browser_config.computer_use.timeout_ms,
                allow_remote_endpoint: browser_config.computer_use.allow_remote_endpoint,
                window_allowlist: browser_config.computer_use.window_allowlist.clone(),
                max_coordinate_x: browser_config.computer_use.max_coordinate_x,
                max_coordinate_y: browser_config.computer_use.max_coordinate_y,
            },
            browser_config.allowed_private_hosts.clone(),
        ) {
            Ok(tool) => {
                tool_arcs.push(Arc::new(RateLimitedTool::new(tool, security.clone())));
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "browser: failed to construct tool, skipping registration"
                );
            }
        }
    }

    // Browser delegation tool (conditionally registered; requires shell access)
    if http_config.enabled {
        match HttpRequestTool::new_with_config(
            security.clone(),
            http_config.allowed_domains.clone(),
            http_config.max_response_size,
            http_config.timeout_secs,
            http_config.allow_private_hosts,
            http_config.allowed_private_hosts.clone(),
            root_config.config_path.clone(),
            root_config.secrets.encrypt,
        ) {
            Ok(tool) => {
                tool_arcs.push(Arc::new(RateLimitedTool::new(tool, security.clone())));
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "http_request: failed to construct tool, skipping registration"
                );
            }
        }
    }

    if web_fetch_config.enabled {
        match WebFetchTool::new(
            security.clone(),
            web_fetch_config.allowed_domains.clone(),
            web_fetch_config.blocked_domains.clone(),
            web_fetch_config.max_response_size,
            web_fetch_config.timeout_secs,
            web_fetch_config.firecrawl.clone(),
            web_fetch_config.allowed_private_hosts.clone(),
        ) {
            Ok(tool) => {
                tool_arcs.push(Arc::new(RateLimitedTool::new(tool, security.clone())));
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "web_fetch: failed to construct tool, skipping registration"
                );
            }
        }
    }

    // Text browser tool (headless text-based browser rendering)
    if root_config.text_browser.enabled {
        match TextBrowserTool::new_with_private_hosts(
            security.clone(),
            root_config.text_browser.preferred_browser.clone(),
            root_config.text_browser.timeout_secs,
            root_config.text_browser.allowed_private_hosts.clone(),
        ) {
            Ok(tool) => {
                tool_arcs.push(Arc::new(tool));
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "text_browser: failed to construct tool, skipping registration"
                );
            }
        }
    }

    // Web search tool (enabled by default for GLM and other models)
    if root_config.web_search.enabled {
        tool_arcs.push(Arc::new(WebSearchTool::new_with_config(
            root_config.web_search.search_provider.clone(),
            root_config.web_search.brave_api_key.clone(),
            root_config.web_search.tavily_api_key.clone(),
            root_config.web_search.jina_api_key.clone(),
            root_config.web_search.searxng_instance_url.clone(),
            root_config.web_search.max_results,
            root_config.web_search.timeout_secs,
            root_config.config_path.clone(),
            root_config.secrets.encrypt,
        )));
    }

    // Notion API tool (conditionally registered)
    #[cfg(feature = "integrations-saas")]
    if root_config.notion.enabled {
        let notion_api_key = if root_config.notion.api_key.trim().is_empty() {
            std::env::var("NOTION_API_KEY").unwrap_or_default()
        } else {
            root_config.notion.api_key.trim().to_string()
        };
        if notion_api_key.trim().is_empty() {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "Notion tool enabled but no API key found (set notion.api_key or NOTION_API_KEY env var)"
            );
        } else {
            tool_arcs.push(Arc::new(NotionTool::new(notion_api_key, security.clone())));
        }
    }

    // Jira integration (config-gated)
    #[cfg(feature = "integrations-saas")]
    if root_config.jira.enabled {
        let api_token = if root_config.jira.api_token.trim().is_empty() {
            std::env::var("JIRA_API_TOKEN").unwrap_or_default()
        } else {
            root_config.jira.api_token.trim().to_string()
        };
        if api_token.trim().is_empty() {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "Jira tool enabled but no API token found (set jira.api_token or JIRA_API_TOKEN env var)"
            );
        } else if root_config.jira.base_url.trim().is_empty() {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "Jira tool enabled but jira.base_url is empty — skipping registration"
            );
        } else {
            let email = root_config
                .jira
                .email
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from);
            if email.is_some() {
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    "Jira tool: Cloud mode (API v3, Basic auth)"
                );
            } else {
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    "Jira tool: Server/DC mode (API v2, Bearer auth)"
                );
            }
            tool_arcs.push(Arc::new(JiraTool::new(
                root_config.jira.base_url.trim().to_string(),
                email,
                api_token,
                root_config.jira.allowed_actions.clone(),
                security.clone(),
                root_config.jira.timeout_secs,
            )));
        }
    }

    // Project delivery intelligence
    if root_config.project_intel.enabled {
        tool_arcs.push(Arc::new(ProjectIntelTool::new(
            root_config.project_intel.default_language.clone(),
            root_config.project_intel.risk_sensitivity.clone(),
        )));
        // Report template tool — direct access to template engine
        tool_arcs.push(Arc::new(ReportTemplateTool::new()));
    }

    // MCSS Security Operations: no longer registered as a model tool. The
    // diagnostics module stays compiled; `security_ops.enabled` no longer
    // admits it to any registry (the daemon notes the withheld section at
    // boot and reload instead).
    //
    // Backup and data management: operator-only surfaces. The gateway
    // operator API (`/api/agents/{alias}/backup*`,
    // `/api/agents/{alias}/data-retention*`) dispatches to the same
    // BackupTool / DataManagementTool command methods; the `[backup]` and
    // `[data_retention]` sections keep configuring that surface, not a
    // model tool.

    // Cloud operations advisory tools (read-only analysis)
    if root_config.cloud_ops.enabled {
        tool_arcs.push(Arc::new(CloudOpsTool::new(root_config.cloud_ops.clone())));
        tool_arcs.push(Arc::new(CloudPatternsTool::new()));
    }

    // Google Workspace CLI (gws) integration — requires shell access
    #[cfg(feature = "integrations-saas")]
    if root_config.google_workspace.enabled && has_shell_access {
        tool_arcs.push(Arc::new(GoogleWorkspaceTool::new(
            security.clone(),
            root_config.google_workspace.allowed_services.clone(),
            root_config.google_workspace.allowed_operations.clone(),
            root_config.google_workspace.credentials_path.clone(),
            root_config.google_workspace.default_account.clone(),
            root_config.google_workspace.rate_limit_per_minute,
            root_config.google_workspace.timeout_secs,
            root_config.google_workspace.audit_log,
        )));
    } else if root_config.google_workspace.enabled {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "google_workspace: skipped registration because shell access is unavailable"
        );
    }

    // Vision tools are always available
    tool_arcs.push(Arc::new(ScreenshotTool::new(security.clone())));
    tool_arcs.push(Arc::new(RateLimitedTool::new(
        PathGuardedTool::new(ImageInfoTool::new(security.clone()), security.clone()),
        security.clone(),
    )));

    if let Ok(backend) =
        zeroclaw_infra::make_session_backend(&config.data_dir, &config.channels.session_backend)
    {
        tool_arcs.push(Arc::new(SessionsCurrentTool::new(backend.clone())));
        tool_arcs.push(Arc::new(SessionsListTool::new(backend.clone())));
        tool_arcs.push(Arc::new(SessionsHistoryTool::new(
            backend.clone(),
            security.clone(),
        )));
        tool_arcs.push(Arc::new(SessionsSendTool::new(backend, security.clone())));
    }

    // LinkedIn integration (config-gated)
    #[cfg(feature = "integrations-saas")]
    if root_config.linkedin.enabled {
        tool_arcs.push(Arc::new(LinkedInTool::new(
            security.clone(),
            workspace_dir.to_path_buf(),
            root_config.linkedin.api_version.clone(),
            root_config.linkedin.content.clone(),
            root_config.linkedin.image.clone(),
        )));
    }

    // Standalone image generation tool (config-gated)
    if root_config.image_gen.enabled {
        tool_arcs.push(Arc::new(ImageGenTool::new_with_persistence(
            security.clone(),
            workspace_dir.to_path_buf(),
            root_config.image_gen.default_model.clone(),
            root_config.image_gen.api_key_env.clone(),
            persistent_writes,
        )));
    }

    // File upload tool — enabled iff [file_upload].url is set
    if root_config
        .file_upload
        .url
        .as_deref()
        .is_some_and(|u| !u.trim().is_empty())
    {
        tool_arcs.push(Arc::new(FileUploadTool::new(
            security.clone(),
            root_config.file_upload.clone(),
        )));
    }

    // File upload bundle tool — enabled iff [file_upload_bundle].url is set
    if root_config
        .file_upload_bundle
        .url
        .as_deref()
        .is_some_and(|u| !u.trim().is_empty())
    {
        tool_arcs.push(Arc::new(FileUploadBundleTool::new(
            security.clone(),
            root_config.file_upload_bundle.clone(),
        )));
    }

    // File download tool — enabled iff [file_download].url is set
    if root_config
        .file_download
        .url
        .as_deref()
        .is_some_and(|u| !u.trim().is_empty())
    {
        tool_arcs.push(Arc::new(FileDownloadTool::new_with_persistence(
            security.clone(),
            root_config.file_download.clone(),
            persistent_writes,
        )));
    }

    // Poll tool — always registered; owns its own late-bound channel map.
    let poll_handle: PerToolChannelHandle = Arc::new(RwLock::new(HashMap::new()));
    tool_arcs.push(Arc::new(PollTool::new(
        security.clone(),
        Arc::clone(&poll_handle),
    )));

    #[cfg(feature = "integrations-saas")]
    if let Some(key) = composio_key
        && !key.is_empty()
    {
        tool_arcs.push(Arc::new(ComposioTool::new(
            key,
            composio_entity_id,
            security.clone(),
        )));
    }

    // Emoji reaction tool — always registered; owns its own late-bound channel map.
    let reaction_handle: PerToolChannelHandle = Arc::new(RwLock::new(HashMap::new()));
    let reaction_tool = ReactionTool::new(security.clone(), Arc::clone(&reaction_handle));
    tool_arcs.push(Arc::new(reaction_tool));

    // Unified forge operations tool, routes through the git channel via the
    // same late-bound channel map as the reaction tool. Resource/action grid
    // plus a raw catch-all over the channel's single forge_request transport.
    let git_forge_tool = GitForgeTool::new(security.clone(), Arc::clone(&reaction_handle));
    tool_arcs.push(Arc::new(git_forge_tool));

    // Channel room-management tool — always registered; owns its own late-bound channel map.
    let channel_room_handle: Option<PerToolChannelHandle> =
        Some(Arc::new(RwLock::new(HashMap::new())));
    let channel_room_tool = ChannelRoomTool::new(
        security.clone(),
        channel_room_handle.as_ref().cloned().unwrap(),
    );
    tool_arcs.push(Arc::new(channel_room_tool));

    // Interactive ask_user tool — always registered; owns its own late-bound channel map.
    let ask_user_handle: Option<PerToolChannelHandle> = Some(Arc::new(RwLock::new(HashMap::new())));
    let ask_user_tool =
        AskUserTool::new(security.clone(), ask_user_handle.as_ref().cloned().unwrap());
    tool_arcs.push(Arc::new(ask_user_tool));

    {
        let agent_peer_groups: AgentPeerGroupResolver = if let Some(live) = live_config.clone() {
            let alias = agent_alias.to_string();
            Arc::new(move || filter_agent_peer_groups(&live.read(), &alias))
        } else {
            let snapshot = filter_agent_peer_groups(root_config, agent_alias);
            Arc::new(move || snapshot.clone())
        };
        tool_arcs.push(Arc::new(SendViaTool::new(
            security.clone(),
            ask_user_handle.as_ref().cloned().unwrap(),
            agent_peer_groups,
        )));
    }

    // Human escalation tool — always registered; owns its own late-bound channel map.
    let escalate_handle: Option<PerToolChannelHandle> = Some(Arc::new(RwLock::new(HashMap::new())));
    let escalate_tool = EscalateToHumanTool::new(
        security.clone(),
        root_config.escalation.alert_channels.clone(),
        escalate_handle.as_ref().cloned().unwrap(),
    );
    tool_arcs.push(Arc::new(escalate_tool));

    // Microsoft 365 Graph API integration
    #[cfg(feature = "integrations-saas")]
    if root_config.microsoft365.enabled {
        let ms_cfg = &root_config.microsoft365;
        let tenant_id = ms_cfg
            .tenant_id
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_string();
        let client_id = ms_cfg
            .client_id
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_string();
        if !tenant_id.is_empty() && !client_id.is_empty() {
            // Fail fast: client_credentials flow requires a client_secret at registration time.
            if ms_cfg.auth_flow.trim() == "client_credentials"
                && ms_cfg
                    .client_secret
                    .as_deref()
                    .is_none_or(|s| s.trim().is_empty())
            {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                    "microsoft365: client_credentials auth_flow requires a non-empty client_secret"
                );
                apply_install_composition(&mut tool_arcs, root_config);
                return AllToolsResult {
                    unfiltered_tool_arcs: tool_arcs.clone(),
                    tools: boxed_registry_from_arcs(tool_arcs),
                    ask_user_handle,
                    channel_room_handle,
                    reaction_handle,
                    poll_handle: Some(poll_handle),
                    escalate_handle,
                };
            }

            let resolved = zeroclaw_tools::microsoft365::types::Microsoft365ResolvedConfig {
                tenant_id,
                client_id,
                client_secret: ms_cfg.client_secret.clone(),
                auth_flow: ms_cfg.auth_flow.clone(),
                scopes: ms_cfg.scopes.clone(),
                token_cache_encrypted: ms_cfg.token_cache_encrypted,
                user_id: ms_cfg.user_id.as_deref().unwrap_or("me").to_string(),
            };
            // Store token cache in the config directory (next to config.toml),
            // not the workspace directory, to keep bearer tokens out of the
            // project tree.
            let cache_dir = root_config.config_path.parent().unwrap_or(workspace_dir);
            match Microsoft365Tool::new(resolved, security.clone(), cache_dir) {
                Ok(tool) => tool_arcs.push(Arc::new(tool)),
                Err(e) => {
                    ::zeroclaw_log::record!(
                        ERROR,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                        "microsoft365: failed to initialize tool"
                    );
                }
            }
        } else {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "microsoft365: skipped registration because tenant_id or client_id is empty"
            );
        }
    }

    // Knowledge graph tool
    if root_config.knowledge.enabled {
        let db_path_str = root_config.knowledge.db_path.replace(
            '~',
            &directories::UserDirs::new()
                .map(|u| u.home_dir().to_string_lossy().to_string())
                .unwrap_or_else(|| ".".to_string()),
        );
        let db_path = std::path::PathBuf::from(&db_path_str);
        match zeroclaw_memory::knowledge_graph::KnowledgeGraph::new(
            &db_path,
            root_config.knowledge.max_nodes,
        ) {
            Ok(graph) => {
                tool_arcs.push(Arc::new(KnowledgeTool::new(Arc::new(graph))));
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "knowledge graph disabled due to init error"
                );
            }
        }
    }

    // `delegate` is retired (wall 1): the legacy full-parent-inheritance
    // delegation tool is no longer constructed on any composition. Its
    // replacement-first surfaces are the V1 `reasoning_subagent` (minimal and
    // full alike) and the Tachi bridge for durable/heavy work. The name stays
    // reserved in RETIRED_OPERATOR_TOOL_NAMES so a plugin cannot ride it back.

    // `vi_verify` is deliberately absent while no chain verifier exists: it checked
    // caller-supplied constraints against a caller-supplied fulfillment with nothing
    // establishing that either came from a signed credential. The operator-facing
    // notice lives at config load, since this function also runs per gateway request
    // and per nested registry rebuild. Register it again only behind a
    // verify-and-evaluate path that consumes a verified chain result.

    // ── WASM plugin tools (requires plugins-wasm feature) ──
    #[cfg(feature = "plugins-wasm")]
    {
        let plugin_path = config.plugins.resolved_plugins_dir();

        if plugin_path.exists() && config.plugins.enabled {
            let signature_mode = zeroclaw_plugins::host::PluginHost::resolve_signature_mode(
                &config.plugins.security.signature_mode,
            );
            let trusted_publisher_keys = config.plugins.security.trusted_publisher_keys.clone();
            match zeroclaw_plugins::host::PluginHost::from_plugins_dir_with_security(
                &plugin_path,
                signature_mode,
                trusted_publisher_keys,
            ) {
                Ok(host) => {
                    let mut details = host.tool_plugin_details();
                    details.sort_unstable_by(|(left, _), (right, _)| left.name.cmp(&right.name));
                    let discovered_count = details.len();
                    let mut registered_count = 0_usize;
                    let mut registered_names: std::collections::HashSet<String> = tool_arcs
                        .iter()
                        .map(|tool| tool.name().to_string())
                        .collect();
                    if root_config.pipeline.enabled {
                        registered_names.insert(PipelineTool::NAME.to_string());
                    }
                    // Operator/admin tools retired from the model surface keep
                    // their names reserved: a plugin must not be able to claim
                    // `backup` or `proxy_config` and ride the retired name back
                    // onto the provider wire.
                    registered_names
                        .extend(RETIRED_OPERATOR_TOOL_NAMES.iter().map(|s| s.to_string()));
                    let plugin_limits = zeroclaw_plugins::component::PluginLimits {
                        call_fuel: config.plugins.limits.call_fuel,
                        max_memory_bytes: config
                            .plugins
                            .limits
                            .max_memory_mb
                            .saturating_mul(1024 * 1024),
                        max_table_elements: config.plugins.limits.max_table_elements,
                        max_instances: config.plugins.limits.max_instances,
                    };
                    for (manifest, wasm_path) in details {
                        let plugin_config = config
                            .plugins
                            .entry_config(&manifest.name)
                            .cloned()
                            .unwrap_or_default();
                        let tool = (|| -> anyhow::Result<_> {
                            let scope =
                                zeroclaw_plugins::instance::PluginInstanceScope::from_manifest(
                                    manifest,
                                    zeroclaw_plugins::PluginCapability::Tool,
                                    manifest.name.clone(),
                                    manifest.permissions.iter().copied(),
                                )?;
                            zeroclaw_plugins::wasm_tool::WasmTool::from_wasm(
                                wasm_path.to_path_buf(),
                                scope,
                                plugin_config,
                                plugin_limits,
                            )
                        })();
                        match tool {
                            Ok(tool) => {
                                if !claim_plugin_tool_name(&mut registered_names, tool.name()) {
                                    ::zeroclaw_log::record!(
                                        WARN,
                                        ::zeroclaw_log::Event::new(
                                            module_path!(),
                                            ::zeroclaw_log::Action::Load
                                        )
                                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                                        .with_attrs(
                                            ::serde_json::json!({
                                                "plugin": manifest.name,
                                                "tool": tool.name(),
                                                "error_key": "plugin_tool_name_conflict",
                                            })
                                        ),
                                        "Plugin tool conflicts with an already registered tool"
                                    );
                                    continue;
                                }
                                tool_arcs.push(Arc::new(tool));
                                registered_count += 1;
                            }
                            Err(e) => {
                                ::zeroclaw_log::record!(
                                    WARN,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Load
                                    )
                                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                                    .with_attrs(
                                        ::serde_json::json!({
                                            "plugin": manifest.name,
                                            "error": format!("{e:#}"),
                                        })
                                    ),
                                    "Failed to register WASM plugin tool"
                                );
                            }
                        }
                    }
                    ::zeroclaw_log::record!(
                        INFO,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_attrs(::serde_json::json!({
                                "discovered": discovered_count,
                                "registered": registered_count,
                            })),
                        "Registered WASM plugin tools"
                    );
                }
                Err(e) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                        "Failed to load WASM plugins"
                    );
                }
            }
        }

        // Surface plugins stranded in a legacy install dir so they aren't
        // silently ignored — the user can relocate them with `plugin migrate`.
        if config.plugins.enabled {
            for legacy in zeroclaw_config::schema::legacy_plugin_dirs_with_entries(&config) {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "legacy_dir": legacy.display().to_string()
                        })),
                    "Plugins in a legacy directory are not loaded; run `zeroclaw plugin migrate`"
                );
            }
        }
    }

    // Pipeline construction waits for ScopedToolRegistry::assemble(), where the
    // effective per-agent policy and optional caller allowlist are both known.

    // The concrete SaaS integration family is not compiled into this build
    // (the `integrations-saas` feature is off). Config sections still parse,
    // so an install that enables one of these families must hear about the
    // mismatch instead of silently losing the tool.
    #[cfg(not(feature = "integrations-saas"))]
    {
        let enabled_but_absent = [
            ("jira", root_config.jira.enabled),
            ("notion", root_config.notion.enabled),
            ("google_workspace", root_config.google_workspace.enabled),
            ("microsoft365", root_config.microsoft365.enabled),
            ("linkedin", root_config.linkedin.enabled),
            ("composio", root_config.composio.enabled),
        ];
        for (family, enabled) in enabled_but_absent {
            if enabled {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"integration": family})),
                    "config enables an integration whose tool family is not compiled into this build (integrations-saas feature is off); skipping registration"
                );
            }
        }
    }

    apply_install_composition(&mut tool_arcs, root_config);

    AllToolsResult {
        unfiltered_tool_arcs: tool_arcs.clone(),
        tools: boxed_registry_from_arcs(tool_arcs),
        ask_user_handle,
        channel_room_handle,
        reaction_handle,
        poll_handle: Some(poll_handle),
        escalate_handle,
    }
}

/// Apply the install-wide composition cut to the assembled registry.
///
/// Under `composition = "minimal"` the registry is reduced to the explicit
/// membership table (`zeroclaw_config::composition::MINIMAL_TOOL_MEMBERSHIP`)
/// before anything derives from it — the boxed registry and the
/// skill-elevation arcs both clone the filtered set — so no later stage can
/// resurrect a built-in non-member (scoped assembly gates its own built-in
/// appends the same way). Extension surfaces — MCP tools admitted by the
/// effective policy and skill-defined tools — are not built-ins and stay
/// governed by their own admission policies. An absent field keeps today's assembly: existing
/// installs must not lose tools on upgrade. Individual `enabled = true`
/// flags do not widen the minimal profile back; the exclusion is logged.
fn apply_install_composition(
    tool_arcs: &mut Vec<Arc<dyn Tool>>,
    root_config: &zeroclaw_config::schema::Config,
) {
    use zeroclaw_config::composition::Composition;

    if Composition::effective(root_config.composition) != Composition::Minimal {
        return;
    }
    let before = tool_arcs.len();
    tool_arcs.retain(|tool| Composition::is_minimal_member(tool.name()));
    let dropped = before - tool_arcs.len();
    if dropped > 0 {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({
                    "dropped": dropped,
                    "composition": "minimal"
                })),
            "Minimal composition excluded non-member tools from assembly"
        );
    }
}

#[cfg(feature = "plugins-wasm")]
fn claim_plugin_tool_name(
    registered_names: &mut std::collections::HashSet<String>,
    plugin_name: &str,
) -> bool {
    registered_names.insert(plugin_name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use zeroclaw_config::schema::{BrowserConfig, Config, MemoryConfig};

    #[tokio::test]
    async fn mcp_capability_tools_respect_policy() {
        use zeroclaw_tools::tool_search::ToolAccessPolicy;
        let registry = std::sync::Arc::new(McpRegistry::connect_all(&[]).await.unwrap());

        // No policy → both tools present.
        let both = build_mcp_capability_tools(&registry, None);
        let names: Vec<_> = both.iter().map(|t| t.name().to_string()).collect();
        assert!(names.contains(&"mcp_resources".to_string()));
        assert!(names.contains(&"mcp_prompts".to_string()));

        // Deny mcp_prompts → only mcp_resources present.
        let policy = ToolAccessPolicy::from_security(
            None,
            Some(&["mcp_prompts".to_string()]),
            None,
            zeroclaw_config::autonomy::McpDiscoveredToolPolicy::default(),
        );
        let one = build_mcp_capability_tools(&registry, policy.as_ref());
        let names: Vec<_> = one.iter().map(|t| t.name().to_string()).collect();
        assert!(names.contains(&"mcp_resources".to_string()));
        assert!(!names.contains(&"mcp_prompts".to_string()));
    }

    fn test_config(tmp: &TempDir) -> Config {
        Config {
            data_dir: tmp.path().join("data"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        }
    }

    #[test]
    fn default_tools_has_expected_count() {
        let security = Arc::new(SecurityPolicy::default());
        let tools = default_tools(security);
        assert_eq!(tools.len(), 6);
    }

    #[cfg(feature = "plugins-wasm")]
    #[test]
    fn plugin_tool_names_cannot_shadow_native_reserved_or_prior_plugin_tools() {
        let mut registered_names =
            std::collections::HashSet::from(["shell".to_string(), PipelineTool::NAME.to_string()]);
        let accepted = ["shell", PipelineTool::NAME, "novel-tool", "novel-tool"]
            .into_iter()
            .filter(|name| claim_plugin_tool_name(&mut registered_names, name))
            .collect::<Vec<_>>();

        assert_eq!(accepted, vec!["novel-tool"]);
        assert_eq!(
            registered_names,
            std::collections::HashSet::from([
                "shell".to_string(),
                PipelineTool::NAME.to_string(),
                "novel-tool".to_string(),
            ])
        );
    }

    #[cfg(feature = "plugins-wasm")]
    #[test]
    fn retired_tool_names_stay_reserved_from_plugin_claims() {
        let mut registered_names = RETIRED_OPERATOR_TOOL_NAMES
            .iter()
            .map(|s| s.to_string())
            .collect::<std::collections::HashSet<_>>();
        for name in RETIRED_OPERATOR_TOOL_NAMES {
            assert!(
                !claim_plugin_tool_name(&mut registered_names, name),
                "a plugin reclaimed retired tool name {name}"
            );
        }
    }

    #[cfg(feature = "plugins-wasm")]
    #[test]
    fn component_with_failed_metadata_probe_is_not_registered() {
        let tmp = TempDir::new().unwrap();
        let package_dir = tmp.path().join("plugins").join("metadata-probe");
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::write(
            package_dir.join("manifest.toml"),
            "name = \"metadata-probe\"\nversion = \"0.1.0\"\nwasm_path = \"plugin.wasm\"\ncapabilities = [\"tool\"]\n",
        )
        .unwrap();
        std::fs::write(package_dir.join("plugin.wasm"), b"not a component").unwrap();

        let mut config = test_config(&tmp);
        config.plugins.enabled = true;
        config.plugins.plugins_dir = tmp.path().join("plugins").display().to_string();
        let security = Arc::new(SecurityPolicy::default());
        let memory: Arc<dyn Memory> = Arc::from(
            zeroclaw_memory::create_memory(
                &MemoryConfig {
                    backend: "markdown".into(),
                    ..MemoryConfig::default()
                },
                tmp.path(),
                None,
            )
            .unwrap(),
        );
        let browser = BrowserConfig {
            enabled: false,
            ..BrowserConfig::default()
        };

        let tools = all_tools(
            Arc::new(config.clone()),
            &security,
            &zeroclaw_config::schema::RiskProfileConfig::default(),
            "test-agent",
            memory,
            None,
            None,
            &browser,
            &zeroclaw_config::schema::HttpRequestConfig::default(),
            &zeroclaw_config::schema::WebFetchConfig::default(),
            tmp.path(),
            &HashMap::new(),
            None,
            &config,
            None,
            false,
            None,
        )
        .tools;

        assert!(
            tools.iter().all(|tool| tool.name() != "metadata-probe"),
            "a component whose required metadata probe fails must not receive manifest fallback metadata"
        );
    }

    /// Discrimination guard for the retired SOP run side: the
    /// legacy agent-facing run tools must never re-enter the registry. Run
    /// truth is Tachi-side (procedure_v1 seam); definitions have no tool
    /// surface here.
    #[test]
    fn sop_run_tools_stay_retired_from_the_registry() {
        let tmp = TempDir::new().unwrap();
        let security = Arc::new(SecurityPolicy::default());
        let mem_cfg = MemoryConfig {
            backend: "markdown".into(),
            ..MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> =
            Arc::from(zeroclaw_memory::create_memory(&mem_cfg, tmp.path(), None).unwrap());

        let browser = BrowserConfig {
            enabled: false,
            allowed_domains: vec![],
            session_name: None,
            ..BrowserConfig::default()
        };
        let http = zeroclaw_config::schema::HttpRequestConfig::default();
        let cfg = test_config(&tmp);

        let tools = all_tools(
            Arc::new(Config::default()),
            &security,
            &zeroclaw_config::schema::RiskProfileConfig::default(),
            "test-agent",
            mem,
            None,
            None,
            &browser,
            &http,
            &zeroclaw_config::schema::WebFetchConfig::default(),
            tmp.path(),
            &HashMap::new(),
            None,
            &cfg,
            None,
            false,
            None,
        )
        .tools;
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();

        let retired_sop_tools = [
            "sop_list",
            "sop_execute",
            "sop_advance",
            "sop_approve",
            "sop_status",
            "sop_workshop",
        ];
        for name in &retired_sop_tools {
            assert!(
                !names.contains(name),
                "legacy SOP run tool '{name}' is retired; the registry must not re-admit it"
            );
        }
    }

    #[tokio::test]
    async fn retired_raw_launcher_tools_never_register_even_when_enabled() {
        // Wall 2 raw-launcher retirement: the Parent-visible raw harness/vendor
        // launch tools are retired together with their config sections. No
        // registry path may re-admit them; harness execution goes through the
        // typed subagent/Tachi paths.
        let tmp = TempDir::new().unwrap();
        let security = Arc::new(SecurityPolicy {
            autonomy: crate::security::AutonomyLevel::Full,
            workspace_dir: tmp.path().to_path_buf(),
            ..SecurityPolicy::default()
        });
        let mem_cfg = MemoryConfig {
            backend: "markdown".into(),
            ..MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> =
            Arc::from(zeroclaw_memory::create_memory(&mem_cfg, tmp.path(), None).unwrap());
        let browser = BrowserConfig {
            enabled: false,
            ..BrowserConfig::default()
        };
        let cfg = test_config(&tmp);
        let risk = zeroclaw_config::schema::RiskProfileConfig {
            sandbox_enabled: Some(false),
            sandbox_backend: Some("none".to_string()),
            ..zeroclaw_config::schema::RiskProfileConfig::default()
        };

        let tools = all_tools_with_runtime(
            Arc::new(cfg.clone()),
            &security,
            &risk,
            "test-agent",
            Arc::new(zeroclaw_config::platform::NativeRuntime::new()),
            mem,
            None,
            None,
            &browser,
            &zeroclaw_config::schema::HttpRequestConfig::default(),
            &zeroclaw_config::schema::WebFetchConfig::default(),
            tmp.path(),
            &HashMap::new(),
            None,
            &cfg,
            None,
            false,
            None,
            None,
            None,
        )
        .tools;
        let names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();

        for tool_name in [
            "claude_code",
            "claude_code_runner",
            "codex_cli",
            "gemini_cli",
            "opencode_cli",
            "browser_delegate",
        ] {
            assert!(
                !names.contains(&tool_name),
                "retired raw launcher '{tool_name}' must not register under any composition"
            );
        }
        assert!(
            names.contains(&"shell"),
            "positive control: ordinary tools should still register"
        );
    }

    #[test]
    fn shared_store_tools_open_data_dir_not_per_agent_workspace() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data"); // shared store (writers' dir)
        let workspace_dir = tmp.path().join("agent-ws"); // per-agent, intentionally distinct
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let security = Arc::new(SecurityPolicy::default());
        let mem_cfg = MemoryConfig {
            backend: "markdown".into(),
            ..MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> =
            Arc::from(zeroclaw_memory::create_memory(&mem_cfg, tmp.path(), None).unwrap());
        let browser = BrowserConfig::default();
        let http = zeroclaw_config::schema::HttpRequestConfig::default();
        let web = zeroclaw_config::schema::WebFetchConfig::default();
        let risk = zeroclaw_config::schema::RiskProfileConfig::default();

        // root_config: shared data_dir + a Discord alias that archives (this is
        // what gates discord_search registration).
        let mut root_config = test_config(&tmp);
        root_config.data_dir = data_dir.clone();
        root_config.channels.discord.insert(
            "oracle".to_string(),
            zeroclaw_config::schema::DiscordConfig {
                archive: true,
                ..Default::default()
            },
        );

        // `config` (arg 1) carries the canonical shared data_dir — exactly how
        // the production callers pass it (a clone of the runtime config).
        let config = Config {
            data_dir: data_dir.clone(),
            ..Config::default()
        };

        let tools = all_tools_with_runtime(
            Arc::new(config),
            &security,
            &risk,
            "test-agent",
            Arc::new(NativeRuntime::new()),
            mem,
            None,
            None,
            &browser,
            &http,
            &web,
            workspace_dir.as_path(), // DIFFERENT from data_dir
            &HashMap::new(),
            None,
            &root_config,
            None,
            false,
            None,
            None,
            None,
        )
        .tools;

        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(
            names.contains(&"discord_search"),
            "discord_search must register when a Discord alias archives"
        );
        assert!(
            names.iter().any(|n| n.starts_with("sessions")),
            "session tools must register"
        );

        // The fix: both stores open under the shared data_dir, never the
        // per-agent workspace. Pre-fix the readers created `memory/discord.db`
        // and `sessions/sessions.db` under the workspace_dir.
        assert!(
            !workspace_dir.join("memory").exists(),
            "discord_search must not open/create a store under the per-agent workspace_dir"
        );
        assert!(
            !workspace_dir.join("sessions").exists(),
            "session tools must not open/create a store under the per-agent workspace_dir"
        );
    }

    /// A runtime that reports an ephemeral workspace (no host persistence) while
    /// delegating real shell execution to `NativeRuntime`. Used to exercise the
    /// registration wiring of `has_filesystem_access()` -> `persistent_writes`.
    struct EphemeralRuntime(NativeRuntime);

    impl RuntimeAdapter for EphemeralRuntime {
        fn name(&self) -> &str {
            "ephemeral-test"
        }
        fn has_shell_access(&self) -> bool {
            true
        }
        fn has_filesystem_access(&self) -> bool {
            false
        }
        fn storage_path(&self) -> std::path::PathBuf {
            std::env::temp_dir()
        }
        fn supports_long_running(&self) -> bool {
            false
        }
        fn build_shell_command(
            &self,
            command: &str,
            workspace_dir: &std::path::Path,
        ) -> anyhow::Result<tokio::process::Command> {
            self.0.build_shell_command(command, workspace_dir)
        }
    }

    #[tokio::test]
    async fn registered_tools_warn_or_block_on_ephemeral_runtime() {
        let tmp = TempDir::new().unwrap();
        tokio::fs::write(tmp.path().join("notes.txt"), "data")
            .await
            .unwrap();
        let security = Arc::new(SecurityPolicy {
            autonomy: crate::security::AutonomyLevel::Supervised,
            max_actions_per_hour: 100,
            workspace_dir: tmp.path().to_path_buf(),
            ..SecurityPolicy::default()
        });
        let runtime: Arc<dyn RuntimeAdapter> = Arc::new(EphemeralRuntime(NativeRuntime::new()));
        let tools = default_tools_with_runtime(security, runtime);
        let by_name = |n: &str| tools.iter().find(|t| t.name() == n).unwrap();

        // shell: warns on the executed command.
        let r = by_name("shell")
            .execute(serde_json::json!({"command": "echo hi"}))
            .await
            .unwrap();
        assert!(
            r.output.contains("EPHEMERAL WORKSPACE"),
            "shell must warn, got: {}",
            r.output
        );

        // file_read: warns on a successful text read.
        let r = by_name("file_read")
            .execute(serde_json::json!({"path": "notes.txt"}))
            .await
            .unwrap();
        assert!(
            r.success && r.output.contains("EPHEMERAL WORKSPACE"),
            "file_read must warn, got: {r:?}"
        );

        // file_edit: warns on a successful edit.
        let r = by_name("file_edit")
            .execute(
                serde_json::json!({"path": "notes.txt", "old_string": "data", "new_string": "x"}),
            )
            .await
            .unwrap();
        assert!(
            r.success && r.output.contains("EPHEMERAL WORKSPACE"),
            "file_edit must warn, got: {r:?}"
        );

        // file_write: refuses outright (does not warn-and-write).
        let r = by_name("file_write")
            .execute(serde_json::json!({"path": "new.txt", "content": "x"}))
            .await
            .unwrap();
        assert!(
            !r.success,
            "file_write must refuse on ephemeral, got: {r:?}"
        );
        assert!(
            r.error
                .as_deref()
                .unwrap_or("")
                .contains("ephemeral workspace"),
            "file_write error must name the cause, got: {:?}",
            r.error
        );
        assert!(
            !tmp.path().join("new.txt").exists(),
            "file_write must not write anything on ephemeral"
        );
    }

    #[test]
    fn all_tools_excludes_browser_when_disabled() {
        let tmp = TempDir::new().unwrap();
        let security = Arc::new(SecurityPolicy::default());
        let mem_cfg = MemoryConfig {
            backend: "markdown".into(),
            ..MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> =
            Arc::from(zeroclaw_memory::create_memory(&mem_cfg, tmp.path(), None).unwrap());

        let browser = BrowserConfig {
            enabled: false,
            allowed_domains: vec!["example.com".into()],
            session_name: None,
            ..BrowserConfig::default()
        };
        let http = zeroclaw_config::schema::HttpRequestConfig::default();
        let cfg = test_config(&tmp);

        let tools = all_tools(
            Arc::new(Config::default()),
            &security,
            &zeroclaw_config::schema::RiskProfileConfig::default(),
            "test-agent",
            mem,
            None,
            None,
            &browser,
            &http,
            &zeroclaw_config::schema::WebFetchConfig::default(),
            tmp.path(),
            &HashMap::new(),
            None,
            &cfg,
            None,
            false,
            None,
        )
        .tools;
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(!names.contains(&"browser_open"));
        assert!(names.contains(&"schedule"));
        // Operator/admin tools are never part of the model surface.
        assert!(!names.contains(&"model_routing_config"));
        assert!(!names.contains(&"proxy_config"));
        // The pushover tool only exists when the SaaS family is compiled in.
        #[cfg(feature = "integrations-saas")]
        assert!(names.contains(&"pushover"));
    }

    #[test]
    fn minimal_composition_cuts_registry_to_membership() {
        let tmp = TempDir::new().unwrap();
        let security = Arc::new(SecurityPolicy::default());
        let mem_cfg = MemoryConfig {
            backend: "markdown".into(),
            ..MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> =
            Arc::from(zeroclaw_memory::create_memory(&mem_cfg, tmp.path(), None).unwrap());

        let browser = BrowserConfig {
            enabled: false,
            allowed_domains: vec!["example.com".into()],
            session_name: None,
            ..BrowserConfig::default()
        };
        let http = zeroclaw_config::schema::HttpRequestConfig::default();
        let mut cfg = test_config(&tmp);
        cfg.composition = Some(zeroclaw_config::composition::Composition::Minimal);
        // Explicitly enabled non-members must not widen the minimal profile.
        cfg.browser.enabled = true;

        let tools = all_tools(
            Arc::new(Config::default()),
            &security,
            &zeroclaw_config::schema::RiskProfileConfig::default(),
            "test-agent",
            mem,
            None,
            None,
            &browser,
            &http,
            &zeroclaw_config::schema::WebFetchConfig::default(),
            tmp.path(),
            &HashMap::new(),
            None,
            &cfg,
            None,
            false,
            None,
        )
        .tools;
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();

        // `read_skill` and `tool_search` are conditional members (compact
        // skills mode / deferred MCP respectively); in this default-config
        // fixture they are not registered at all, which the totality check
        // below covers from the other side.
        for member in [
            "shell",
            "file_read",
            "file_write",
            "file_edit",
            "glob_search",
            "content_search",
            "schedule",
            "reasoning_subagent",
        ] {
            assert!(
                names.contains(&member),
                "minimal profile must keep {member}, got: {names:?}"
            );
        }
        // The minimal composition fronts the V1 entrypoint; the legacy
        // `spawn_subagent` is retired on every composition.
        assert!(
            !names.contains(&"spawn_subagent"),
            "minimal profile must drop the retired spawn_subagent; got: {names:?}"
        );
        // Fail-closed totality: nothing outside the membership table may be
        // assembled under minimal, whatever flags enabled it.
        for name in &names {
            assert!(
                zeroclaw_config::composition::is_minimal_member(name),
                "non-member leaked into minimal assembly: {name}"
            );
        }
        assert!(!names.contains(&"model_routing_config"));
        assert!(!names.contains(&"proxy_config"));
        assert!(!names.contains(&"pushover"));
        assert!(!names.contains(&"claude_code"));
    }

    #[test]
    fn absent_composition_keeps_full_assembly() {
        let tmp = TempDir::new().unwrap();
        let security = Arc::new(SecurityPolicy::default());
        let mem_cfg = MemoryConfig {
            backend: "markdown".into(),
            ..MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> =
            Arc::from(zeroclaw_memory::create_memory(&mem_cfg, tmp.path(), None).unwrap());

        let browser = BrowserConfig {
            enabled: false,
            allowed_domains: vec!["example.com".into()],
            session_name: None,
            ..BrowserConfig::default()
        };
        let http = zeroclaw_config::schema::HttpRequestConfig::default();
        let cfg = test_config(&tmp);
        assert!(
            cfg.composition.is_none(),
            "test_config must not set a composition"
        );

        let tools = all_tools(
            Arc::new(Config::default()),
            &security,
            &zeroclaw_config::schema::RiskProfileConfig::default(),
            "test-agent",
            mem,
            None,
            None,
            &browser,
            &http,
            &zeroclaw_config::schema::WebFetchConfig::default(),
            tmp.path(),
            &HashMap::new(),
            None,
            &cfg,
            None,
            false,
            None,
        )
        .tools;
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        // Absent field resolves as full: today's default assembly minus the
        // retired operator/admin tools, which no composition may re-admit.
        assert!(!names.contains(&"model_routing_config"));
        assert!(!names.contains(&"proxy_config"));
        // The legacy full-Parent-inheritance spawn entrypoint is retired
        // (spawn_subagent wall): no composition registers it, and the
        // retired-name guard keeps plugins from claiming it.
        assert!(
            !names.contains(&"spawn_subagent"),
            "full composition must not register the retired spawn_subagent; got: {names:?}"
        );
        assert!(
            names.contains(&"reasoning_subagent"),
            "full composition must carry the V1 reasoning_subagent; got: {names:?}"
        );
        // The pushover tool only exists when the SaaS family is compiled in.
        #[cfg(feature = "integrations-saas")]
        assert!(names.contains(&"pushover"));
    }

    #[test]
    fn all_tools_includes_browser_when_enabled() {
        let tmp = TempDir::new().unwrap();
        let security = Arc::new(SecurityPolicy::default());
        let mem_cfg = MemoryConfig {
            backend: "markdown".into(),
            ..MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> =
            Arc::from(zeroclaw_memory::create_memory(&mem_cfg, tmp.path(), None).unwrap());

        let browser = BrowserConfig {
            enabled: true,
            allowed_domains: vec!["example.com".into()],
            session_name: None,
            ..BrowserConfig::default()
        };
        let http = zeroclaw_config::schema::HttpRequestConfig::default();
        let cfg = test_config(&tmp);

        let tools = all_tools(
            Arc::new(Config::default()),
            &security,
            &zeroclaw_config::schema::RiskProfileConfig::default(),
            "test-agent",
            mem,
            None,
            None,
            &browser,
            &http,
            &zeroclaw_config::schema::WebFetchConfig::default(),
            tmp.path(),
            &HashMap::new(),
            None,
            &cfg,
            None,
            false,
            None,
        )
        .tools;
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"browser_open"));
        assert!(names.contains(&"content_search"));
        // Operator/admin tools are never part of the model surface.
        assert!(!names.contains(&"model_routing_config"));
        assert!(!names.contains(&"proxy_config"));
        // The pushover tool only exists when the SaaS family is compiled in.
        #[cfg(feature = "integrations-saas")]
        assert!(names.contains(&"pushover"));
    }

    #[test]
    fn default_tools_names() {
        let security = Arc::new(SecurityPolicy::default());
        let tools = default_tools(security);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"shell"));
        assert!(names.contains(&"file_read"));
        assert!(names.contains(&"file_write"));
        assert!(names.contains(&"file_edit"));
        assert!(names.contains(&"glob_search"));
        assert!(names.contains(&"content_search"));
    }

    #[test]
    fn default_tools_all_have_descriptions() {
        let security = Arc::new(SecurityPolicy::default());
        let tools = default_tools(security);
        for tool in &tools {
            assert!(
                !tool.description().is_empty(),
                "Tool {} has empty description",
                tool.name()
            );
        }
    }

    #[test]
    fn default_tools_all_have_schemas() {
        let security = Arc::new(SecurityPolicy::default());
        let tools = default_tools(security);
        for tool in &tools {
            let schema = tool.parameters_schema();
            assert!(
                schema.is_object(),
                "Tool {} schema is not an object",
                tool.name()
            );
            assert!(
                schema["properties"].is_object(),
                "Tool {} schema has no properties",
                tool.name()
            );
        }
    }

    #[test]
    fn tool_spec_generation() {
        let security = Arc::new(SecurityPolicy::default());
        let tools = default_tools(security);
        for tool in &tools {
            let spec = tool.spec();
            assert_eq!(spec.name, tool.name());
            assert_eq!(spec.description, tool.description());
            assert!(spec.parameters.is_object());
        }
    }

    #[test]
    fn tool_result_serde() {
        let result = ToolResult {
            success: true,
            output: "hello".into(),
            error: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: ToolResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.success);
        assert_eq!(parsed.output, "hello");
        assert!(parsed.error.is_none());
    }

    #[test]
    fn tool_result_with_error_serde() {
        let result = ToolResult {
            success: false,
            output: ToolOutput::default(),
            error: Some("boom".into()),
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: ToolResult = serde_json::from_str(&json).unwrap();
        assert!(!parsed.success);
        assert_eq!(parsed.error.as_deref(), Some("boom"));
    }

    #[test]
    fn tool_spec_serde() {
        let spec = ToolSpec::new("test", "A test tool", serde_json::json!({"type": "object"}));
        let json = serde_json::to_string(&spec).unwrap();
        let parsed: ToolSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "test");
        assert_eq!(parsed.description, "A test tool");
    }

    #[test]
    fn delegate_stays_absent_from_every_registry() {
        // Wall 1: the legacy full-parent-inheritance delegation tool is
        // retired. Neither an agents-configured full-composition registry nor
        // an agents-less one may surface it, and the retired-name guard keeps
        // plugins from claiming the name. The V1 SubAgent entrypoint
        // (`reasoning_subagent`) is the only spawn-capable model-visible
        // tool; the legacy `spawn_subagent` retired with its wall.
        let tmp = TempDir::new().unwrap();
        let security = Arc::new(SecurityPolicy::default());
        let mem_cfg = MemoryConfig {
            backend: "markdown".into(),
            ..MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> =
            Arc::from(zeroclaw_memory::create_memory(&mem_cfg, tmp.path(), None).unwrap());

        let browser = BrowserConfig::default();
        let http = zeroclaw_config::schema::HttpRequestConfig::default();
        let cfg = test_config(&tmp);

        let mut agents = HashMap::new();
        agents.insert(
            "researcher".to_string(),
            AliasedAgentConfig {
                model_provider: "ollama.researcher".into(),
                ..Default::default()
            },
        );

        for (label, agents) in [
            ("agents configured", agents.clone()),
            ("no agents", HashMap::new()),
        ] {
            let tools = all_tools(
                Arc::new(Config::default()),
                &security,
                &zeroclaw_config::schema::RiskProfileConfig::default(),
                "test-agent",
                mem.clone(),
                None,
                None,
                &browser,
                &http,
                &zeroclaw_config::schema::WebFetchConfig::default(),
                tmp.path(),
                &agents,
                Some("delegate-test-credential"),
                &cfg,
                None,
                false,
                None,
            )
            .tools;
            let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
            assert!(
                !names.contains(&"delegate"),
                "delegate must not be registered ({label}); got {names:?}"
            );
        }
    }

    // ── Unified lineage (SA-9/SA-10/SA-11): the GREEN side of the
    // census zig-zag red→green pair. The RED evidence (master, where the
    // rebuilt registry minted depth 0 and sailed past the depth gate) is
    // captured verbatim in the PR body; these tests pin the closed
    // behavior.

    fn lineage_agents() -> HashMap<String, AliasedAgentConfig> {
        let mut agents = HashMap::new();
        for alias in ["parent-agent", "child-target"] {
            agents.insert(
                alias.to_string(),
                AliasedAgentConfig {
                    risk_profile: "default".into(),
                    ..AliasedAgentConfig::default()
                },
            );
        }
        agents
    }

    fn lineage_registry_config() -> Config {
        let mut config = Config::default();
        let risk = zeroclaw_config::schema::RiskProfileConfig::default();
        config.risk_profiles.insert("default".to_string(), risk);
        config.agents = lineage_agents();
        config
    }

    #[tokio::test]
    async fn registry_rebuild_carries_spawn_lineage_and_cannot_reset_depth() {
        // The census zig-zag GREEN half: a registry built for a child
        // context whose lineage is at the depth cap (exactly what
        // `agent::run` builds for a spawned child of a depth-3 parent)
        // carries the ONE ledger through the rebuild. The behavioral
        // refusal at that depth is pinned on the surviving spawn
        // surface in `subagent_v1::tests`; here the rebuilt registry's
        // SHAPE is the discrimination: both retired spawn tools are
        // absent, the V1 entrypoint is present and inherits the depth.
        let tmp = TempDir::new().unwrap();
        let cfg = lineage_registry_config();
        let mut build_cfg = cfg.clone();
        build_cfg.data_dir = tmp.path().join("data");
        build_cfg.config_path = tmp.path().join("config.toml");
        let security = Arc::new(SecurityPolicy::for_agent(&build_cfg, "parent-agent").unwrap());
        let mem: Arc<dyn Memory> = Arc::from(
            zeroclaw_memory::create_memory(&MemoryConfig::default(), tmp.path(), None).unwrap(),
        );

        let at_cap = zeroclaw_api::subagent_v1::LineageRef::new_root(
            zeroclaw_api::subagent_v1::ParentRunRef::from_opaque("zigzag-root"),
        )
        .child()
        .child()
        .child(); // depth 3 = default cap

        // The lineage thread-through is discriminated at the construction
        // site: the helper the registry vec calls must carry the run's
        // lineage, so dropping the `.with_lineage` thread flips this red
        // (a lineage-None reasoning tool inside a child registry would
        // admit D1-forbidden spawns from depth > 0 contexts).
        let probe = reasoning_spawn_tool_for_registry(
            &build_cfg,
            "parent-agent",
            &security,
            Some(at_cap.clone()),
        );
        assert_eq!(
            probe
                .carried_lineage()
                .map(zeroclaw_api::subagent_v1::LineageRef::depth),
            Some(3),
            "the registry construction site must thread the run's spawn lineage \
             into the surviving spawn tool"
        );

        let built = all_tools_with_runtime(
            Arc::new(build_cfg),
            &security,
            &cfg.risk_profiles.get("default").cloned().unwrap(),
            "parent-agent",
            Arc::new(NativeRuntime::new()),
            mem,
            None,
            None,
            &BrowserConfig::default(),
            &zeroclaw_config::schema::HttpRequestConfig::default(),
            &zeroclaw_config::schema::WebFetchConfig::default(),
            tmp.path(),
            &cfg.agents,
            None,
            &lineage_registry_config(),
            None,
            true, // is_subagent_caller: registry belongs to a child run
            None,
            None,
            Some(at_cap.clone()),
        );

        // The retired legacy spawn tools must not reappear in a rebuilt
        // registry: the zig-zag chain loses its legacy hops entirely.
        let names: Vec<String> = built.tools.iter().map(|t| t.name().to_string()).collect();
        assert!(
            !names.contains(&"delegate".to_string()),
            "delegate is retired and must be absent from a rebuilt registry"
        );
        assert!(
            !names.contains(&"spawn_subagent".to_string()),
            "spawn_subagent is retired and must be absent from a rebuilt registry"
        );
        // The V1 entrypoint is the surviving spawn surface in the same
        // rebuilt registry (and refuses at depth > 0 per D1 — asserted
        // behaviorally by `subagent_v1::tests`).
        assert!(
            names.contains(&"reasoning_subagent".to_string()),
            "reasoning_subagent must be the surviving spawn surface in a rebuilt registry"
        );
    }

    #[test]
    fn zigzag_is_counted_by_one_ledger_across_spawn_hops() {
        // SA-9/SA-10: any spawn chain (the census chain was
        // `delegate → spawn_subagent → delegate`; both legacy hops are
        // retired) is counted by ONE counter. The depth a rebuilt
        // registry sees is exactly the spawning context's lineage
        // advanced by one, however many hops the chain took.
        use zeroclaw_api::subagent_v1::{LineageRef, ParentRunRef};

        let root = LineageRef::new_root(ParentRunRef::from_opaque("chain-root"));
        assert_eq!(root.depth(), 0);

        let after_first_hop = root.child();
        assert_eq!(after_first_hop.depth(), 1);

        let after_second_hop = after_first_hop.child();
        assert_eq!(after_second_hop.depth(), 2);

        // A rebuilt registry in the grandchild context carries the same
        // lineage — depth 2 against the cap (3), refusing at the NEXT
        // hop, never resetting (the ledger law is what the test pins):
        let after_third_hop = after_second_hop.child();
        assert_eq!(after_third_hop.depth(), 3);
        // ...and 3 >= cap is the refusal asserted behaviorally on the
        // surviving spawn surface in `subagent_v1::tests`.
        assert!(after_third_hop.depth() >= 3);

        // The ledger identity is the root run, shared across the whole
        // chain (SA-11: rebuilds inherit, roots are typed transitions).
        assert_eq!(root.root_ref(), after_third_hop.root_ref());
    }

    #[test]
    fn all_tools_includes_read_skill_in_compact_mode() {
        let tmp = TempDir::new().unwrap();
        let security = Arc::new(SecurityPolicy::default());
        let mem_cfg = MemoryConfig {
            backend: "markdown".into(),
            ..MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> =
            Arc::from(zeroclaw_memory::create_memory(&mem_cfg, tmp.path(), None).unwrap());

        let browser = BrowserConfig::default();
        let http = zeroclaw_config::schema::HttpRequestConfig::default();
        let mut cfg = test_config(&tmp);
        cfg.skills.prompt_injection_mode =
            zeroclaw_config::schema::SkillsPromptInjectionMode::Compact;

        let tools = all_tools(
            Arc::new(cfg.clone()),
            &security,
            &zeroclaw_config::schema::RiskProfileConfig::default(),
            "test-agent",
            mem,
            None,
            None,
            &browser,
            &http,
            &zeroclaw_config::schema::WebFetchConfig::default(),
            tmp.path(),
            &HashMap::new(),
            None,
            &cfg,
            None,
            false,
            None,
        )
        .tools;
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"read_skill"));
    }

    #[test]
    fn all_tools_excludes_read_skill_in_full_mode() {
        let tmp = TempDir::new().unwrap();
        let security = Arc::new(SecurityPolicy::default());
        let mem_cfg = MemoryConfig {
            backend: "markdown".into(),
            ..MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> =
            Arc::from(zeroclaw_memory::create_memory(&mem_cfg, tmp.path(), None).unwrap());

        let browser = BrowserConfig::default();
        let http = zeroclaw_config::schema::HttpRequestConfig::default();
        let mut cfg = test_config(&tmp);
        cfg.skills.prompt_injection_mode = zeroclaw_config::schema::SkillsPromptInjectionMode::Full;

        let tools = all_tools(
            Arc::new(cfg.clone()),
            &security,
            &zeroclaw_config::schema::RiskProfileConfig::default(),
            "test-agent",
            mem,
            None,
            None,
            &browser,
            &http,
            &zeroclaw_config::schema::WebFetchConfig::default(),
            tmp.path(),
            &HashMap::new(),
            None,
            &cfg,
            None,
            false,
            None,
        )
        .tools;
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(!names.contains(&"read_skill"));
    }

    #[test]
    fn retired_operator_tools_absent_from_every_assembly_path() {
        // Totality check for the operator/admin retirement: the retired
        // names must not appear in the assembled registry (`tools`) NOR in
        // the pre-policy `unfiltered_tool_arcs` (the vector skill builtin
        // elevation resolves targets against), whatever re-admits them:
        //
        // - absent `composition` (resolves as full),
        // - explicit `composition = "full"`,
        // - a SubAgent caller (the registry factory the spawned-child path
        //   shares with the top level; inheritance is downstream of the
        //   cut, so there is nothing to inherit),
        // - config sections explicitly enabling the retired tools.
        fn assembled(
            tmp: &TempDir,
            composition: Option<zeroclaw_config::composition::Composition>,
            is_subagent_caller: bool,
            enable_retired_sections: bool,
        ) -> (Vec<String>, Vec<String>) {
            let security = Arc::new(SecurityPolicy::default());
            let mem: Arc<dyn Memory> = Arc::from(
                zeroclaw_memory::create_memory(
                    &MemoryConfig {
                        backend: "markdown".into(),
                        ..MemoryConfig::default()
                    },
                    tmp.path(),
                    None,
                )
                .unwrap(),
            );
            let mut cfg = test_config(tmp);
            cfg.composition = composition;
            if enable_retired_sections {
                cfg.security_ops.enabled = true;
                cfg.backup.enabled = true;
                cfg.data_retention.enabled = true;
            }
            let result = all_tools(
                Arc::new(cfg.clone()),
                &security,
                &zeroclaw_config::schema::RiskProfileConfig::default(),
                "test-agent",
                mem,
                None,
                None,
                &BrowserConfig::default(),
                &zeroclaw_config::schema::HttpRequestConfig::default(),
                &zeroclaw_config::schema::WebFetchConfig::default(),
                tmp.path(),
                &HashMap::new(),
                None,
                &cfg,
                None,
                is_subagent_caller,
                None,
            );
            (
                result.tools.iter().map(|t| t.name().to_string()).collect(),
                result
                    .unfiltered_tool_arcs
                    .iter()
                    .map(|t| t.name().to_string())
                    .collect(),
            )
        }

        for (label, composition, is_subagent, enable) in [
            ("absent composition", None, false, false),
            (
                "explicit full composition",
                Some(zeroclaw_config::composition::Composition::Full),
                false,
                false,
            ),
            (
                "subagent caller, full composition",
                Some(zeroclaw_config::composition::Composition::Full),
                true,
                false,
            ),
            (
                "enabled retired sections, absent composition",
                None,
                false,
                true,
            ),
        ] {
            let tmp = TempDir::new().unwrap();
            let (tools, arcs) = assembled(&tmp, composition, is_subagent, enable);
            for retired in RETIRED_OPERATOR_TOOL_NAMES {
                assert!(
                    !tools.iter().any(|n| n == retired),
                    "retired tool {retired} leaked into the registry under {label}: {tools:?}"
                );
                assert!(
                    !arcs.iter().any(|n| n == retired),
                    "retired tool {retired} leaked into the unfiltered arcs under {label}: {arcs:?}"
                );
            }
        }
    }

    #[test]
    fn all_tools_registers_read_skill_for_compact_agent_override_over_global_full() {
        let tmp = TempDir::new().unwrap();
        let security = Arc::new(SecurityPolicy::default());
        let mem_cfg = MemoryConfig {
            backend: "markdown".into(),
            ..MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> =
            Arc::from(zeroclaw_memory::create_memory(&mem_cfg, tmp.path(), None).unwrap());

        let browser = BrowserConfig::default();
        let http = zeroclaw_config::schema::HttpRequestConfig::default();
        let mut cfg = test_config(&tmp);
        // Global stays Full; a runtime profile flips this agent to Compact and
        // the agent selects it via `runtime_profile`.
        cfg.skills.prompt_injection_mode = zeroclaw_config::schema::SkillsPromptInjectionMode::Full;
        cfg.runtime_profiles.insert(
            "compact_profile".to_string(),
            zeroclaw_config::schema::RuntimeProfileConfig {
                prompt_injection_mode: Some(
                    zeroclaw_config::schema::SkillsPromptInjectionMode::Compact,
                ),
                ..Default::default()
            },
        );
        cfg.agents.insert(
            "test-agent".to_string(),
            zeroclaw_config::schema::AliasedAgentConfig {
                runtime_profile: "compact_profile".into(),
                ..Default::default()
            },
        );

        let tools = all_tools(
            Arc::new(cfg.clone()),
            &security,
            &zeroclaw_config::schema::RiskProfileConfig::default(),
            "test-agent",
            mem,
            None,
            None,
            &browser,
            &http,
            &zeroclaw_config::schema::WebFetchConfig::default(),
            tmp.path(),
            &HashMap::new(),
            None,
            &cfg,
            None,
            false,
            None,
        )
        .tools;
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(
            names.contains(&"read_skill"),
            "compact runtime-profile override should register read_skill even when global is full"
        );
    }

    #[test]
    fn all_tools_omits_read_skill_for_full_agent_override_over_global_compact() {
        let tmp = TempDir::new().unwrap();
        let security = Arc::new(SecurityPolicy::default());
        let mem_cfg = MemoryConfig {
            backend: "markdown".into(),
            ..MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> =
            Arc::from(zeroclaw_memory::create_memory(&mem_cfg, tmp.path(), None).unwrap());

        let browser = BrowserConfig::default();
        let http = zeroclaw_config::schema::HttpRequestConfig::default();
        let mut cfg = test_config(&tmp);
        // Global is Compact; a runtime profile pins this agent to Full and the
        // agent selects it via `runtime_profile`.
        cfg.skills.prompt_injection_mode =
            zeroclaw_config::schema::SkillsPromptInjectionMode::Compact;
        cfg.runtime_profiles.insert(
            "full_profile".to_string(),
            zeroclaw_config::schema::RuntimeProfileConfig {
                prompt_injection_mode: Some(
                    zeroclaw_config::schema::SkillsPromptInjectionMode::Full,
                ),
                ..Default::default()
            },
        );
        cfg.agents.insert(
            "test-agent".to_string(),
            zeroclaw_config::schema::AliasedAgentConfig {
                runtime_profile: "full_profile".into(),
                ..Default::default()
            },
        );

        let tools = all_tools(
            Arc::new(cfg.clone()),
            &security,
            &zeroclaw_config::schema::RiskProfileConfig::default(),
            "test-agent",
            mem,
            None,
            None,
            &browser,
            &http,
            &zeroclaw_config::schema::WebFetchConfig::default(),
            tmp.path(),
            &HashMap::new(),
            None,
            &cfg,
            None,
            false,
            None,
        )
        .tools;
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(
            !names.contains(&"read_skill"),
            "full runtime-profile override should omit read_skill even when global is compact"
        );
    }

    /// `vi_verify` checked caller-supplied constraints against a caller-supplied
    /// fulfillment with nothing establishing that either came from a signed
    /// credential. Until a chain verifier exists the tool must not reach the model
    /// even when an operator opts in.
    #[test]
    fn vi_verify_is_not_registered_even_when_verifiable_intent_is_enabled() {
        let tmp = TempDir::new().unwrap();
        let security = Arc::new(SecurityPolicy::default());
        let mem_cfg = MemoryConfig {
            backend: "markdown".into(),
            ..MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> =
            Arc::from(zeroclaw_memory::create_memory(&mem_cfg, tmp.path(), None).unwrap());

        let mut cfg = test_config(&tmp);
        cfg.verifiable_intent.enabled = true;

        let tools = all_tools(
            Arc::new(cfg.clone()),
            &security,
            &zeroclaw_config::schema::RiskProfileConfig::default(),
            "test-agent",
            mem,
            None,
            None,
            &BrowserConfig::default(),
            &zeroclaw_config::schema::HttpRequestConfig::default(),
            &zeroclaw_config::schema::WebFetchConfig::default(),
            tmp.path(),
            &HashMap::new(),
            None,
            &cfg,
            None,
            false,
            None,
        )
        .tools;
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();

        assert!(
            !names.contains(&"vi_verify"),
            "vi_verify must not be model-callable while no chain verifier exists"
        );
        assert!(
            names.contains(&"shell"),
            "positive control: the registry must still be populated"
        );
    }
}

#[cfg(test)]
mod todo_registration_tests {
    #[test]
    fn todo_write_tool_name_is_stable() {
        use zeroclaw_api::tool::Tool;
        assert_eq!(super::todo_write::TodoWriteTool::new().name(), "TodoWrite");
    }
}

#[cfg(test)]
mod wrapper_spec_forwarding_tests {
    use super::*;
    use async_trait::async_trait;
    use zeroclaw_api::tool::ToolSpec;

    /// Stand-in for `McpToolWrapper`: stores its schema once and overrides
    /// `spec()` to hand out `Arc::clone`, so tests can assert wrappers
    /// preserve `Arc` identity instead of falling back to the trait
    /// default (which would deep-clone via `parameters_schema()`).
    struct ArcSchemaTool {
        schema: Arc<serde_json::Value>,
    }

    impl ArcSchemaTool {
        fn new() -> Self {
            Self {
                schema: Arc::new(serde_json::json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } }
                })),
            }
        }
    }

    impl ::zeroclaw_api::attribution::Attributable for ArcSchemaTool {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Tool(::zeroclaw_api::attribution::ToolKind::Plugin)
        }
        fn alias(&self) -> &str {
            "arc-schema-tool"
        }
    }

    #[async_trait]
    impl Tool for ArcSchemaTool {
        fn name(&self) -> &str {
            "arc_schema_tool"
        }

        fn description(&self) -> &str {
            "test tool with Arc-shared schema"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            (*self.schema).clone()
        }

        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name().to_string(),
                description: self.description().to_string(),
                parameters: Arc::clone(&self.schema),
                output: None,
                param_domains: std::collections::BTreeMap::new(),
            }
        }

        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: "ok".into(),
                error: None,
            })
        }
    }

    #[test]
    fn arc_tool_ref_forwards_spec_arc_identity() {
        let inner: Arc<dyn Tool> = Arc::new(ArcSchemaTool::new());
        let inner_params = inner.spec().parameters;
        let wrapped = ArcToolRef(Arc::clone(&inner));

        assert!(
            Arc::ptr_eq(&wrapped.spec().parameters, &inner_params),
            "ArcToolRef must forward spec() so the inner Arc-shared schema \
             survives; the trait default deep-clones it every call"
        );
        assert!(
            Arc::ptr_eq(&wrapped.spec().parameters, &wrapped.spec().parameters),
            "repeated spec() calls must hand out the same allocation"
        );
    }

    #[test]
    fn arc_delegating_tool_forwards_spec_arc_identity() {
        let inner: Arc<dyn Tool> = Arc::new(ArcSchemaTool::new());
        let inner_params = inner.spec().parameters;
        let boxed = ArcDelegatingTool::boxed(inner);

        assert!(
            Arc::ptr_eq(&boxed.spec().parameters, &inner_params),
            "ArcDelegatingTool must forward spec() so the inner Arc-shared \
             schema survives; the trait default deep-clones it every call"
        );
    }
}

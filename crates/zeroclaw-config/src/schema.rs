// Historical schema typed lenses for migration. Each module is frozen after
// its corresponding version ships; only their `migrate(self) -> ...` methods
// are referenced at runtime by `crate::migration`.
pub mod v1;
pub mod v2;

use crate::autonomy::AutonomyLevel;
use crate::domain_matcher::DomainMatcher;
use crate::traits::{ChannelConfig, HasPropKind, PropKind};
use crate::validation_bail;
use anyhow::{Context, Result};
use directories::UserDirs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};
#[cfg(unix)]
use tokio::fs::File;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use zeroclaw_api::runtime_status::RuntimeConfigKind;
use zeroclaw_macros::Configurable;

const SUPPORTED_PROXY_SERVICE_KEYS: &[&str] = &[
    "model_provider.anthropic",
    "model_provider.compatible",
    "model_provider.copilot",
    "model_provider.gemini",
    "model_provider.glm",
    "model_provider.ollama",
    "model_provider.openai",
    "model_provider.openrouter",
    "channel.dingtalk",
    "channel.discord",
    "channel.lark",
    "channel.matrix",
    "channel.mattermost",
    "channel.nextcloud_talk",
    "channel.qq",
    "channel.signal",
    "channel.slack",
    "channel.telegram",
    "channel.wati",
    "channel.wechat",
    "channel.whatsapp",
    "tool.browser",
    "tool.composio",
    "tool.http_request",
    "tool.pushover",
    "tool.web_search",
    "memory.embeddings",
    "tunnel.custom",
    "transcription.groq",
];

const SUPPORTED_PROXY_SERVICE_SELECTORS: &[&str] = &[
    "model_provider.*",
    "channel.*",
    "tool.*",
    "memory.*",
    "tunnel.*",
    "transcription.*",
];

static RUNTIME_PROXY_CONFIG: OnceLock<RwLock<ProxyConfig>> = OnceLock::new();
static RUNTIME_PROXY_CLIENT_CACHE: OnceLock<RwLock<HashMap<String, reqwest::Client>>> =
    OnceLock::new();

// ── Top-level config ──────────────────────────────────────────────

/// Top-level ZeroClaw configuration, loaded from `config.toml`.
///
/// Resolution order: `ZEROCLAW_CONFIG_DIR` env → `ZEROCLAW_WORKSPACE` env → `~/.zeroclaw/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct Config {
    /// Shared instance data directory (databases, hygiene state, cost
    /// records, daemon state files). Computed from `ZEROCLAW_CONFIG_DIR`
    /// / `ZEROCLAW_DATA_DIR` / `ZEROCLAW_WORKSPACE` (deprecated) at
    /// load time, not serialized. Per-agent identity + markdown lives
    /// at `agent_workspace_dir(&alias)`, not here.
    #[serde(skip)]
    pub data_dir: PathBuf,
    /// Path to config.toml - computed from home, not serialized
    #[serde(skip)]
    pub config_path: PathBuf,
    /// Dotted prop-paths overridden by `ZEROCLAW_*` env vars at load time.
    /// Populated by `apply_env_overrides`; consulted by `save()` to mask the
    /// env-injected values back to disk-or-default before encryption, and by
    /// `prop_is_env_overridden` for O(1) display-layer lookup (config list,
    /// dashboard, quickstart).
    #[serde(skip)]
    pub env_overridden_paths: std::collections::HashSet<String>,
    /// Per-path snapshot of pre-override raw values, captured at apply time
    /// from the post-`decrypt_secrets` in-memory state (so secret entries
    /// hold plaintext, not the display mask). `save()` restores from this
    /// map so env-injected values never reach disk and the operator's
    /// original on-disk credentials survive any save cycle.
    #[serde(skip)]
    pub pre_override_snapshots: std::collections::HashMap<String, String>,
    /// Per-path snapshot of `op://` external secret references captured before
    /// `decrypt_secrets()` resolves them for runtime use. `save()` restores
    /// these references unless the same path was intentionally edited.
    #[serde(skip)]
    pub onepassword_reference_snapshots: std::collections::HashMap<String, String>,
    /// Dotted prop-paths mutated since the last persist; drives the
    /// per-path PATCH applied by `save_dirty()`.
    #[serde(skip)]
    pub dirty_paths: std::collections::HashSet<String>,
    /// Security-critical sections the resilient loader reset to `Default`
    /// because the on-disk block was malformed. Non-empty = posture may be
    /// weaker than intended; exposure gating should refuse to trust the
    /// instance until repaired. Never serialized — a load-time signal.
    #[serde(skip)]
    pub degraded_security: Vec<String>,
    /// Non-security sections the resilient loader reset to `Default`
    /// because the on-disk block was malformed (e.g. `[plugins.entries]`
    /// written where `[[plugins.entries]]` was meant). Never serialized;
    /// a load-time signal the CLI surfaces on stderr so a silently-dropped
    /// section is impossible to miss.
    #[serde(skip)]
    pub degraded_sections: Vec<String>,
    /// Structured deprecation warnings for retired config surfaces,
    /// observed at load time: `ZEROCLAW_gateway__pairing_dashboard__*` env
    /// overrides (ignored by the env-override tombstone) and a
    /// `[gateway.pairing_dashboard]` section left in a config file. This
    /// field is the only place those observations can live — nothing else
    /// in the loaded `Config` remembers them. Replayed by
    /// `collect_warnings()` so the stable-code warning machinery (logs,
    /// gateway API, dashboard) surfaces them like any other warning.
    /// Never serialized — a load-time signal. Sunset: these tombstones are
    /// compatibility shims and will be removed in a later announced
    /// window, after which the paths hard-error or parse-drop silently.
    #[serde(skip)]
    pub retired_surface_warnings: Vec<crate::validation_warnings::ValidationWarning>,
    /// The config file this value was populated from — set by
    /// `load_or_init` (both the existing-file read and the fresh-init
    /// write) and by loaders that re-read a config file before mutating.
    /// `Default`/programmatic construction leaves it `None`, and `save()`
    /// then refuses to overwrite an existing file this value cannot prove
    /// it read, so a near-empty snapshot cannot wipe an operator's config;
    /// `force_save()` is the explicit intentional-overwrite path. Binding
    /// provenance to the path (not a boolean) also refuses a value loaded
    /// from file A that is later repointed at a different existing file B.
    /// Never serialized — a load-time signal.
    #[serde(skip)]
    pub loaded_from: Option<PathBuf>,
    /// Config file schema version.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,

    /// All configured provider profiles, grouped by category under a
    /// single `[providers]` root. Categories today: `models`, `tts`,
    /// `transcription`. Shape: `[providers.<category>.<type>.<alias>]`,
    /// e.g. `[providers.models.anthropic.default]`,
    /// `[providers.tts.openai.default]`,
    /// `[providers.transcription.groq.default]`.
    #[serde(default)]
    #[nested]
    pub providers: crate::providers::Providers,

    /// Model-routing rules — route `hint:<name>` to specific
    /// model_provider + model combos.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[nested]
    #[natural_key = "hint"]
    #[group = "Foundation"]
    pub model_routes: Vec<ModelRouteConfig>,

    /// Embedding-routing rules — route `hint:<name>` to specific
    /// model_provider + model combos for embedding requests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[nested]
    #[natural_key = "hint"]
    #[group = "Foundation"]
    pub embedding_routes: Vec<EmbeddingRouteConfig>,

    /// Observability backend configuration (`[observability]`).
    #[serde(default)]
    #[nested]
    #[group = "Operations"]
    pub observability: ObservabilityConfig,

    /// Trust scoring and regression detection configuration (`[trust]`).
    #[serde(default)]
    #[nested]
    #[group = "Operations"]
    pub trust: crate::scattered_types::TrustConfig,

    /// Security subsystem configuration (`[security]`).
    #[serde(default)]
    #[nested]
    #[group = "Operations"]
    pub security: SecurityConfig,

    /// Backup tool configuration (`[backup]`).
    #[serde(default)]
    #[nested]
    #[group = "Operations"]
    pub backup: BackupConfig,

    /// Data retention and purge configuration (`[data_retention]`).
    #[serde(default)]
    #[nested]
    #[group = "Operations"]
    pub data_retention: DataRetentionConfig,

    /// Cloud transformation accelerator configuration (`[cloud_ops]`).
    #[serde(default)]
    #[nested]
    #[group = "Operations"]
    pub cloud_ops: CloudOpsConfig,

    /// Conversational AI agent builder configuration (`[conversational_ai]`).
    ///
    /// Experimental / future feature — not yet wired into the agent runtime.
    /// Omitted from generated config files when disabled (the default).
    /// Existing configs that already contain this section will continue to
    /// deserialize correctly thanks to `#[serde(default)]`.
    #[serde(default, skip_serializing_if = "ConversationalAiConfig::is_disabled")]
    #[nested]
    #[group = "Operations"]
    pub conversational_ai: ConversationalAiConfig,

    /// Managed cybersecurity service configuration (`[security_ops]`).
    #[serde(default)]
    #[nested]
    #[group = "Operations"]
    pub security_ops: SecurityOpsConfig,

    /// Runtime adapter configuration (`[runtime]`). Controls native vs Docker execution.
    #[serde(default)]
    #[nested]
    #[group = "Agent"]
    pub runtime: RuntimeConfig,

    /// Reliability settings: retries, backoff, key rotation (`[reliability]`).
    #[serde(default)]
    #[nested]
    #[group = "Agent"]
    pub reliability: ReliabilityConfig,

    /// Scheduler configuration for periodic task execution (`[scheduler]`).
    #[serde(default)]
    #[nested]
    #[group = "Agent"]
    pub scheduler: SchedulerConfig,

    /// Agent evaluation harness (`[eval]`) — surfaced via `zeroclaw eval`.
    /// Distinct from `[agent.eval]`, which is the in-loop response-quality scorer.
    #[serde(default)]
    #[nested]
    #[group = "Operations"]
    pub eval: crate::scattered_types::EvalHarnessConfig,

    /// Pacing controls for slow/local LLM workloads (`[pacing]`).
    #[serde(default)]
    #[nested]
    #[group = "Agent"]
    pub pacing: PacingConfig,

    /// Skills loading and community repository behavior (`[skills]`).
    #[serde(default)]
    #[nested]
    pub skills: SkillsConfig,

    /// Pipeline tool configuration (`[pipeline]`).
    #[serde(default)]
    #[nested]
    #[group = "Agent"]
    pub pipeline: PipelineConfig,

    /// Automatic query classification — maps user messages to model hints.
    #[serde(default)]
    #[nested]
    #[group = "Agent"]
    pub query_classification: QueryClassificationConfig,

    /// Heartbeat configuration for periodic health pings (`[heartbeat]`).
    #[serde(default)]
    #[nested]
    #[group = "Agent"]
    pub heartbeat: HeartbeatConfig,

    /// ZeroCode live task tracker (`[todotracker]`), the read-only
    /// TodoWrite visual tracker in the Code pane.
    #[serde(default)]
    #[nested]
    #[group = "Operations"]
    pub todotracker: TodoTrackerConfig,

    /// Declarative cron jobs (`[cron.<alias>]`), alias-keyed.
    ///
    /// Each entry is a named scheduled job synced into the database at
    /// scheduler startup. Subsystem runtime knobs (enable/disable, catch-up,
    /// run-history retention) live on `[scheduler]`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub cron: HashMap<String, CronJobDecl>,

    /// ACP (Agent Client Protocol) server configuration (`[acp]`).
    #[serde(default)]
    #[nested]
    #[group = "Integrations"]
    pub acp: AcpConfig,

    /// Channel configurations: Telegram, Discord, Slack, etc. (`[channels]`).
    #[serde(default, alias = "channels_config")]
    #[nested]
    pub channels: ChannelsConfig,

    /// Memory backend configuration: sqlite, markdown, embeddings (`[memory]`).
    #[serde(default)]
    #[nested]
    pub memory: MemoryConfig,

    /// Persistent storage model_provider configuration (`[storage]`).
    #[serde(default)]
    #[nested]
    pub storage: StorageConfig,

    /// Tunnel configuration for exposing the gateway publicly (`[tunnel]`).
    #[serde(default)]
    #[nested]
    pub tunnel: TunnelConfig,

    /// Gateway server configuration: host, port, pairing, rate limits (`[gateway]`).
    #[serde(default)]
    #[nested]
    #[group = "Network"]
    pub gateway: GatewayConfig,

    /// Inbound A2A discovery server (`[a2a.server]`). Default-closed:
    /// serves the well-known catalog card and per-alias agent cards only
    /// when `enabled = true`. See `crate::multi_agent::A2aServerConfig`.
    #[serde(default)]
    #[nested]
    #[group = "Network"]
    pub a2a: crate::multi_agent::A2aServerSection,

    /// WebSocket Secure (WSS) transport for remote TUI connections (`[wss]`).
    #[serde(default)]
    #[nested]
    pub wss: WssConfig,

    /// Composio managed OAuth tools integration (`[composio]`).
    #[serde(default)]
    #[nested]
    #[group = "Integrations"]
    pub composio: ComposioConfig,

    /// Microsoft 365 Graph API integration (`[microsoft365]`).
    #[serde(default)]
    #[nested]
    pub microsoft365: Microsoft365Config,

    /// Secrets encryption configuration (`[secrets]`).
    #[serde(default)]
    #[nested]
    #[group = "Storage"]
    pub secrets: SecretsConfig,

    /// Browser automation configuration (`[browser]`).
    #[serde(default)]
    #[nested]
    #[group = "Tools"]
    pub browser: BrowserConfig,

    /// HTTP request tool configuration (`[http_request]`).
    #[serde(default)]
    #[nested]
    #[group = "Tools"]
    pub http_request: HttpRequestConfig,

    /// Multimodal (image) handling configuration (`[multimodal]`).
    #[serde(default)]
    #[nested]
    #[group = "Tools"]
    pub multimodal: MultimodalConfig,

    /// Automatic media understanding pipeline (`[media_pipeline]`).
    #[serde(default)]
    #[nested]
    #[group = "Tools"]
    pub media_pipeline: MediaPipelineConfig,

    /// Web fetch tool configuration (`[web_fetch]`).
    #[serde(default)]
    #[nested]
    #[group = "Tools"]
    pub web_fetch: WebFetchConfig,

    /// Link enricher configuration (`[link_enricher]`).
    #[serde(default)]
    #[nested]
    #[group = "Tools"]
    pub link_enricher: LinkEnricherConfig,

    /// Text browser tool configuration (`[text_browser]`).
    #[serde(default)]
    #[nested]
    #[group = "Tools"]
    pub text_browser: TextBrowserConfig,

    /// Web search tool configuration (`[web_search]`).
    #[serde(default)]
    #[nested]
    #[group = "Tools"]
    pub web_search: WebSearchConfig,

    /// Project delivery intelligence configuration (`[project_intel]`).
    #[serde(default)]
    #[nested]
    #[group = "Tools"]
    pub project_intel: ProjectIntelConfig,

    /// Google Workspace CLI (`gws`) tool configuration (`[google_workspace]`).
    #[serde(default)]
    #[nested]
    #[group = "Integrations"]
    pub google_workspace: GoogleWorkspaceConfig,

    /// Proxy configuration for outbound HTTP/HTTPS/SOCKS5 traffic (`[proxy]`).
    #[serde(default)]
    #[nested]
    #[group = "Network"]
    pub proxy: ProxyConfig,

    /// Cost tracking and budget enforcement configuration (`[cost]`).
    /// Also hosts the operator-managed rate sheet at
    /// `[cost.rates.<type>.<model>]`.
    #[serde(default)]
    #[nested]
    #[group = "Operations"]
    pub cost: CostConfig,

    /// Peripheral board configuration for hardware integration (`[peripherals]`).
    #[serde(default)]
    #[nested]
    #[group = "Operations"]
    pub peripherals: PeripheralsConfig,

    /// Aliased agents in this install. Each entry under `[agents.<alias>]`
    /// is one user-facing agent with its own identity, channels, model
    /// provider, risk profile, workspace, and memory scope.
    #[serde(default)]
    #[nested]
    pub agents: HashMap<String, AliasedAgentConfig>,

    /// Named risk/autonomy profiles (`[risk_profiles.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub risk_profiles: HashMap<String, RiskProfileConfig>,

    /// Install-wide tool composition selector (`composition`).
    ///
    /// `"minimal"` assembles only the explicit minimal companion membership
    /// (`crate::composition::MINIMAL_TOOL_MEMBERSHIP`); individual tool
    /// `enabled` flags do not widen it back. `"full"` (alias `"legacy"`) is
    /// today's assembly — a transitional opt-in compatibility profile. An
    /// absent field resolves as `"full"` so existing installs never lose
    /// tools on upgrade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[group = "Operations"]
    pub composition: Option<crate::composition::Composition>,

    /// Named runtime/LLM execution profiles (`[runtime_profiles.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub runtime_profiles: HashMap<String, RuntimeProfileConfig>,

    /// Named agent cards (`[cards.<alias>]`) — one authored unit carrying an
    /// agent's voice, its tool grants, and the profile supplying the rest of
    /// its authority. A card owns tools; its risk profile owns everything
    /// else.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub cards: HashMap<String, crate::card::AgentCard>,

    /// Named persona dial sets (`[personas.<alias>]`) — how an agent talks.
    ///
    /// Authored, never learned, and unable to widen what an agent may do:
    /// these are delivery settings, in the same family as `reasoning_effort`.
    /// Authority lives on `[risk_profiles.<alias>]`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub personas: HashMap<String, crate::persona::PersonaKnobs>,

    /// Companion-memory store and owner gate (`[companion_memory]`).
    ///
    /// `enable` defaults to false. Store files live under `{data_dir}/companion`
    /// unless `store_dir` is set. `[companion_memory.owner]` declares who may
    /// produce `owner_authored` rows. Unmatched ingress is `shared-operator`
    /// and can never produce `owner_authored`.
    #[serde(
        default,
        skip_serializing_if = "crate::companion::CompanionMemoryConfig::is_unset"
    )]
    #[nested]
    #[group = "Storage"]
    pub companion_memory: crate::companion::CompanionMemoryConfig,

    /// Named skill bundles (`[skill_bundles.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub skill_bundles: HashMap<String, SkillBundleConfig>,

    /// Named knowledge bundles (`[knowledge_bundles.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub knowledge_bundles: HashMap<String, KnowledgeBundleConfig>,

    /// Named MCP server bundles (`[mcp_bundles.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub mcp_bundles: HashMap<String, McpBundleConfig>,

    /// Named peer groups (`[peer_groups.<name>]`). Each entry binds a
    /// channel, a list of member agents, and optional non-agent
    /// (external) members and a per-group blocklist. Mutual opt-in:
    /// two agents become peers only when both appear in the same
    /// group's `agents`. Empty by default for single-agent installs.
    /// See `crate::multi_agent::PeerGroupConfig`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub peer_groups: HashMap<String, crate::multi_agent::PeerGroupConfig>,

    /// Hooks configuration (lifecycle hooks and built-in hook toggles).
    #[serde(default)]
    #[nested]
    #[group = "Agent"]
    pub hooks: HooksConfig,

    /// Hardware configuration (wizard-driven physical world setup).
    #[serde(default)]
    #[nested]
    pub hardware: HardwareConfig,

    /// Voice transcription configuration (Whisper API via Groq).
    #[serde(default)]
    #[nested]
    #[group = "Tools"]
    pub transcription: TranscriptionConfig,

    /// Text-to-Speech configuration (`[tts]`).
    #[serde(default)]
    #[nested]
    #[group = "Tools"]
    pub tts: TtsConfig,

    /// External MCP server connections (`[mcp]`).
    #[serde(default, alias = "mcpServers")]
    #[nested]
    pub mcp: McpConfig,

    /// Dynamic node discovery configuration (`[nodes]`).
    #[serde(default)]
    #[nested]
    #[group = "Network"]
    pub nodes: NodesConfig,

    /// Meta-state for the Quickstart flow (which sections the user has
    /// already walked through). Not user-facing config (`[onboard_state]`).
    #[serde(default)]
    #[nested]
    pub onboard_state: OnboardStateConfig,

    /// Notion integration configuration (`[notion]`).
    #[serde(default)]
    #[nested]
    #[group = "Integrations"]
    pub notion: NotionConfig,

    /// Jira integration configuration (`[jira]`).
    #[serde(default)]
    #[nested]
    #[group = "Integrations"]
    pub jira: JiraConfig,

    /// Secure inter-node transport configuration (`[node_transport]`).
    #[serde(default)]
    #[nested]
    #[group = "Network"]
    pub node_transport: NodeTransportConfig,

    /// Knowledge graph configuration (`[knowledge]`).
    #[serde(default)]
    #[nested]
    #[group = "Tools"]
    pub knowledge: KnowledgeConfig,

    /// LinkedIn integration configuration (`[linkedin]`).
    #[serde(default)]
    #[nested]
    #[group = "Integrations"]
    pub linkedin: LinkedInConfig,

    /// Standalone image generation tool configuration (`[image_gen]`).
    #[serde(default)]
    #[nested]
    #[group = "Tools"]
    pub image_gen: ImageGenConfig,

    /// Standalone file upload tool configuration (`[file_upload]`).
    #[serde(default)]
    #[nested]
    pub file_upload: FileUploadConfig,

    /// Standalone multi-file bundle upload tool configuration
    /// (`[file_upload_bundle]`).
    #[serde(default)]
    #[nested]
    pub file_upload_bundle: FileUploadBundleConfig,

    /// Standalone file download tool configuration (`[file_download]`).
    #[serde(default)]
    #[nested]
    pub file_download: FileDownloadConfig,

    /// Plugin system configuration (`[plugins]`).
    #[serde(default)]
    #[nested]
    #[group = "Tools"]
    pub plugins: PluginsConfig,

    /// Locale for tool descriptions (e.g. `"en"`, `"zh-CN"`).
    ///
    /// When set, tool descriptions shown in system prompts are loaded from
    /// Fluent `.ftl` locale files. Falls back to embedded English, then to
    /// hardcoded descriptions.
    ///
    /// If omitted or empty, the locale is auto-detected from the host
    /// system's locale (defaulting to `"en"` if that can't be determined).
    #[serde(default)]
    pub locale: Option<String>,

    /// Verifiable Intent (VI) credential issuance and constraint checking
    /// (`[verifiable_intent]`). No credential chain verifier exists yet, so the
    /// `vi_verify` tool is not registered for the model and this section does not
    /// currently enable verification of anything.
    #[serde(default)]
    #[nested]
    #[group = "Agent"]
    pub verifiable_intent: VerifiableIntentConfig,

    /// Standard Operating Procedures engine configuration (`[sop]`).
    #[serde(default)]
    #[nested]
    #[group = "Agent"]
    pub sop: SopConfig,

    /// Shell tool configuration (`[shell_tool]`).
    #[serde(default)]
    #[nested]
    #[group = "Tools"]
    pub shell_tool: ShellToolConfig,

    /// Escalation routing configuration (`[escalation]`).
    #[serde(default)]
    #[nested]
    pub escalation: EscalationConfig,
}

/// Multi-client workspace isolation configuration.
///
/// When enabled, each client engagement gets an isolated workspace with
/// separate memory, audit, secrets, and tool restrictions.
#[allow(clippy::struct_excessive_bools)]
/// Opaque state the Quickstart flow writes so it can tell, on a
/// re-run, which sections the user has already walked through at least
/// once — which lets it offer "Reconfigure? [y/N]" skip gates instead of
/// forcing users through every field again.
///
/// This is meta-state about the Quickstart flow, not user-facing config.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "onboard_state"]
pub struct OnboardStateConfig {
    /// Section keys the user has completed at least once.
    /// Values are the lowercased Section variant names
    /// (`"workspace"`, `"model_providers"`, …).
    #[serde(default)]
    pub completed_sections: Vec<String>,
    /// `true` once the Quickstart has applied a `BuilderSubmission`
    /// successfully on this install. Web gateway and TUI auto-launch
    /// the Quickstart on startup iff this is `false` **and** no
    /// `agents.*` entries exist (the implicit-completion rule covers
    /// upgrades). The flag is flipped in the same atomic write that
    /// lands the Quickstart submission; re-entering the Quickstart
    /// later to add another agent does not flip it back to `false`.
    #[serde(default)]
    pub quickstart_completed: bool,
}

/// Used by `#[serde(skip_serializing_if)]` on plain `bool` fields to omit
/// them from TOML output when they carry their struct-level default (`false`).
/// Keeps fresh model_provider entries clean — a default-constructed
/// `ModelProviderConfig` for one model_provider family shouldn't write flag fields
/// that only apply to a different family.
fn is_false(value: &bool) -> bool {
    !*value
}

/// One trait per family-endpoint enum. Returns the URI template for the chosen
/// variant — a literal URL for fixed endpoints (`https://api.openai.com/v1`),
/// or a substitution template for computed endpoints (Azure's
/// `https://{resource}.openai.azure.com/...`). Substitution happens family-side
/// in the runtime constructor; for non-templated families the return value is
/// the final URL.
///
/// Resolution order at runtime is uniform across every model model_provider family:
/// operator's `cfg.uri` first; family endpoint enum's `uri()` second; loud
/// failure when neither is set.
pub trait ModelEndpoint {
    fn uri(&self) -> &'static str;
}

/// Implemented by every `*ModelProviderConfig`. Multi-region families
/// override to return `Some(self.endpoint.uri())`; single-endpoint families
/// inherit the `None` default. Drives `ModelProviders::resolved_endpoint_uri`,
/// which is itself driven by the `for_each_model_provider_slot!` macro — so
/// adding a new family without an impl is a compile error.
pub trait FamilyEndpoint {
    fn endpoint_uri(&self) -> Option<&'static str> {
        None
    }
}

/// Wire protocol flavor for the model_provider client. `responses` routes
/// through OpenAI's Responses API (`POST /v1/responses`);
/// `chat_completions` routes through the legacy `/v1/chat/completions` (or
/// the family's chat-completions-compatible endpoint). New OpenAI provider
/// slots default to `responses`; other families default to chat-completions
/// (or ignore the field) when unset.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WireApi {
    Responses,
    ChatCompletions,
}

impl WireApi {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::ChatCompletions => "chat_completions",
        }
    }
}

/// Authentication mode for model model_provider families that support more than one
/// (e.g. Qwen, Minimax can use API key OR OAuth). Families that only support a
/// single auth flow simply omit this field from their config struct.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Standard API key authentication via the `api_key` field.
    #[default]
    ApiKey,
    /// OAuth flow — credential resolution defers to the family runtime impl
    /// (typically reading a vendor-specific token cache or env var).
    OAuth,
}

/// Named model_provider profile definition.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models"]
pub struct ModelProviderConfig {
    /// Secret API token for this model_provider. Grab it from the model_provider's dashboard (OpenAI platform, Anthropic console, OpenRouter keys page, etc.). Stored via the OS keyring when possible; never commit it to config.toml directly.
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Provider implementation to instantiate for this profile. Use this
    /// when a canonical typed slot should run through a compatible
    /// implementation, e.g. `[providers.models.openai.proxy] kind =
    /// "openai-compatible"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Endpoint URI the client hits. Override the family's default endpoint when pointing at a self-hosted gateway (LiteLLM, vLLM, Ollama), a custom proxy, or any non-standard URL. Leave unset to use the family's default URI from its `ModelEndpoint` impl. Set this to the FULL endpoint URL; there is no separate path-suffix field.
    #[tab(Connection)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    /// Model identifier to send with each request: the ID string from the model_provider's catalog (e.g. `gpt-4o`, `claude-sonnet-4-5`, `llama-3.3-70b`). Must match a model the model_provider actually serves on this account.
    #[tab(Model)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Ordered list of other provider aliases to try when every model on this
    /// alias has failed. Each entry is a dotted `<type>.<alias>` reference into
    /// `providers.models` and resolves with its own credentials, endpoint, and
    /// model. A fallback never inherits this alias's key. The walk is
    /// depth-first: this alias's models are exhausted first, then each fallback
    /// alias is descended in turn (applying its own `fallback_models` and
    /// `fallback`). Empty means no provider-level fallback.
    #[tab(Model)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback: Vec<crate::providers::ModelProviderRef>,
    /// Ordered alternate models to try on THIS provider before falling over to
    /// the `fallback` aliases. Same endpoint, key, and headers as the primary
    /// `model`. Only the model identifier changes. Use this when a provider
    /// serves a backup model (e.g. a smaller or older variant) that should be
    /// tried before leaving the provider entirely. Empty means only `model` is
    /// tried.
    #[tab(Model)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_models: Vec<String>,
    /// Sampling temperature passed to the model. Lower values (0.0–0.3) give
    /// deterministic, near-verbatim output, which fits code, routing, summarization.
    /// Higher values (0.7–1.2) give more varied output, which fits open-ended chat.
    #[tab(Model)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// HTTP request timeout in seconds. Bump this for slow local model_providers (Ollama on CPU, big local models) or high-latency networks; leave unset otherwise.
    #[tab(Model)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Extra HTTP headers sent with every request. Niche: used for auth bridges, corporate proxies, or custom gateways that demand a tracing header. Most users never touch this; edit `config.toml` directly if you need it.
    #[tab(Connection)]
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[secret]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub extra_headers: HashMap<String, String>,
    /// Wire protocol flavor: `responses` for OpenAI's Responses API (`POST /v1/responses`), `chat_completions` for the legacy chat wire and most OpenAI-compatible gateways. New OpenAI provider slots default to `responses`; other families default to chat-completions (or ignore the field). Only override if you're forcing an unusual combination.
    #[tab(Advanced)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_api: Option<WireApi>,
    /// When true, the client pulls credentials from ZeroClaw's stored `openai-codex` auth profile instead of the `api_key` field above. Import an existing Codex CLI login with `zeroclaw auth login --model-provider openai-codex --import ~/.codex/auth.json`, or run `zeroclaw auth login --model-provider openai-codex`. Turn on only for the OpenAI Codex model_provider; leave off for standard API-key model_providers.
    #[tab(Connection)]
    #[serde(default, skip_serializing_if = "is_false")]
    #[credential_class = "external_auth_store"]
    pub requires_openai_auth: bool,
    /// Hard cap on response length in tokens. Most models enforce sensible built-in limits already; leave unset unless you specifically need to clip long outputs for cost or latency reasons.
    #[tab(Model)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// ModelProvider-specific quirk: fold the system prompt into the first user message instead of sending a separate system role. Only needed for models that reject (or mishandle) a standalone system role, e.g. certain older Mistral variants.
    #[tab(Advanced)]
    #[serde(default, skip_serializing_if = "is_false")]
    pub merge_system_into_user: bool,
    /// Extra JSON parameters to include in API requests.
    /// Merged at the top level of the request body, allowing provider-specific
    /// features (routing, transforms, etc.) without code changes.
    /// Example: `provider_extra = { model_provider = { only = ["Anthropic"] } }`
    #[tab(Advanced)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_extra: Option<serde_json::Value>,
    /// Per-model pricing for cost tracking, USD per 1M tokens.
    ///
    /// Free-form key/value map. Keys are user-defined model identifiers; an
    /// optional `.input` / `.output` suffix encodes pricing dimension when
    /// the operator wants to split rates. A bare key without a suffix is
    /// used as a flat per-token rate when neither dimension is specified.
    /// Default is empty: cost tracking falls back to "unknown" rates and
    /// only token usage is recorded.
    ///
    /// Example: `pricing = { opus = 15.0, sonnet = 3.0 }`
    /// Or split: `pricing = { "opus.input" = 15.0, "opus.output" = 75.0 }`
    #[tab(Advanced)]
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub pricing: HashMap<String, f64>,
    /// Whether stored assistant reasoning should be replayed on outbound
    /// assistant history messages. `Some(false)` strips `reasoning_content`
    /// and `reasoning` before sending. `None` (default) honours the provider's
    /// built-in default (true for most compat providers, false for Groq).
    #[tab(Advanced)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_assistant_reasoning: Option<bool>,
    /// Pull live token prices for this provider's models from its own
    /// OpenAI-compatible `/models` listing (the gateway is the source of truth
    /// for its prices), filling cost-tracking rates for models the operator
    /// has NOT priced under `[cost.rates]` / `pricing`. Models the gateway does
    /// not price (or providers with no HTTP `/models` listing at all, such as
    /// a subprocess gateway like `kilocli`) fall back to the public models.dev
    /// catalog. Configured rates always win; live prices only fill gaps. A
    /// background task refreshes the price snapshot hourly; the cost-recording
    /// path reads the cached snapshot and never blocks on the network. Default
    /// `false`: off means no fetching and behavior identical to a build
    /// without the feature.
    #[tab(Advanced)]
    #[serde(default, skip_serializing_if = "is_false")]
    pub live_pricing: bool,
    /// Override the provider's default for native tool calling.
    /// `None` (default) honors the provider's built-in choice. `Some(true)`
    /// forces native tool calls on, `Some(false)` forces text-fallback.
    /// Currently consulted only by the Groq factory, which defaults to
    /// text-fallback because llama-family Groq models reject native tool
    /// calls with HTTP 400. Setting `native_tools = true` re-enables native
    /// tool calling for Groq models that support it.
    #[tab(Advanced)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_tools: Option<bool>,
    /// Enable or disable chain-of-thought thinking for models that support it
    /// (e.g. Qwen3, GLM-4). `true` turns thinking on, `false` turns it off.
    /// `None` (default) lets the model decide. Forwarded as `enable_thinking`
    /// in the request body; mirrors the Ollama provider's `think` field.
    #[tab(Advanced)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub think: Option<bool>,
    /// Override the provider's vision (image input) capability.
    /// `None` (default) uses the provider family's built-in default. Several
    /// families (llama.cpp, the generic OpenAI-compatible endpoint, etc.)
    /// assume vision-capable because they can serve multimodal models. Set
    /// `vision = false` for a text-only model served by such a family (e.g. a
    /// text LLM behind llama.cpp) so image messages are routed to a configured
    /// `[multimodal] vision_model_provider` instead of being sent to a model
    /// that rejects them. `Some(true)` forces vision on.
    #[tab(Advanced)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    /// Arbitrary key/value pairs forwarded verbatim as a top-level
    /// `chat_template_kwargs` object in the request body of OpenAI-compatible
    /// providers. Consumed by chat-template-aware backends such as vLLM,
    /// SGLang, and llama.cpp to pass model-family template variables that
    /// control behaviour not exposed by other fields. Must be a JSON object
    /// (TOML inline table); non-object values are ignored with a warning.
    /// Example (Qwen3 thinking suppression):
    ///   `chat_template_kwargs = { enable_thinking = false }`
    #[tab(Advanced)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<serde_json::Value>,
    /// Context window size (max input tokens) for this model.
    /// Auto-populated on setup from provider's /models endpoint if available.
    /// Override manually for custom endpoints or when auto-detection fails.
    #[tab(Advanced)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
    /// Path to a PEM-encoded CA certificate for TLS connections to this provider.
    /// Must be an absolute path; shell expansion (e.g. `~`) is not performed.
    /// Leave unset to use the system's default trust store.
    #[tab(Connection)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca_cert_path: Option<String>,
}

// ── Per-family model model_provider configs ────────────────────────────
//
// Each family carries its own typed config (composing `ModelProviderConfig`
// via `#[serde(flatten)]`) plus a per-family `*Endpoint` enum that names the
// known endpoints and resolves them via the `ModelEndpoint` trait. Families
// that support multiple auth flows additionally carry an `auth_mode` field.
//
// Pattern reference for adding a new family:
// - Single-endpoint family with no extras: see `AnthropicModelProviderConfig`
// - Family with extras: see `OpenAIModelProviderConfig`
// - Family with computed-endpoint template: see `AzureModelProviderConfig`
// - Multi-region family with a required `endpoint` field: see `MoonshotModelProviderConfig`
//
// The `ModelProviders` container in `crates/zeroclaw-config/src/model_providers.rs`
// holds a typed slot per family; the runtime impls in zeroclaw-providers
// consume the typed configs directly.

// ── OpenAI ──

/// OpenAI canonical endpoint. Single variant — OpenAI publishes one base URL.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OpenAIEndpoint {
    #[default]
    Default,
}

impl ModelEndpoint for OpenAIEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.openai.com/v1",
        }
    }
}

/// OpenAI model model_provider config. The OpenAI-family extras (`wire_api`,
/// `requires_openai_auth`) live on the shared `ModelProviderConfig` base
/// because they're consumed by validation and runtime helpers that operate
/// on the base struct without family awareness; this wrapper is a thin
/// typed slot, no extra fields.
///
/// New OpenAI provider entries **persisted via `create_map_key` / `ensure`**
/// (quickstart, gateway/config UI, programmatic slot creation) default to
/// `wire_api = "responses"` because OpenAI has moved recent GPT models onto
/// `POST /v1/responses` as the primary wire. OpenAI-compatible families
/// (`custom`, `llamacpp`, branded vendors, …) keep the shared
/// `ModelProviderConfig` default of unset / chat-completions.
///
/// This default governs **persisted slot creation only**. Two paths keep the
/// legacy chat-completions wire for backward compatibility:
/// - Existing persisted configs that omit `wire_api` deserialize as `None`, and
///   the factory falls back to chat-completions.
/// - Implicit dispatch with no config entry at all — a bare
///   `model_provider = "openai"` reference or a dotted ref to a nonexistent
///   alias — is built from a chat-anchored fallback config (see
///   `openai_missing_entry_fallback_config` in `zeroclaw-providers`), not this
///   `Default`, and stays on the chat wire so existing bare-ref installs don't
///   flip wire + tool-calling mode on upgrade.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.openai"]
pub struct OpenAIModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

impl Default for OpenAIModelProviderConfig {
    fn default() -> Self {
        Self {
            base: ModelProviderConfig {
                // OpenAI's current default wire for new provider slots.
                wire_api: Some(WireApi::Responses),
                ..ModelProviderConfig::default()
            },
        }
    }
}

// ── Azure OpenAI ──

/// Azure OpenAI endpoint template. Single variant; the URL is computed at
/// runtime by substituting `{resource}` and `{deployment}` from the typed
/// config fields.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AzureEndpoint {
    #[default]
    Default,
}

impl ModelEndpoint for AzureEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            // Azure's URI is a template — substitution happens in the
            // AzureModelProvider runtime constructor against the typed
            // config's resource / deployment fields.
            Self::Default => "https://{resource}.openai.azure.com/openai/deployments/{deployment}",
        }
    }
}

/// Azure OpenAI model model_provider config. Carries the Azure-specific connection
/// fields (`resource`, `deployment`, `api_version`) — the URI template
/// substitutes `{resource}` and `{deployment}` at runtime. Operators can
/// still override the entire endpoint via `base.uri`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.azure"]
pub struct AzureModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
    /// Azure resource name (the `<resource>` part of `<resource>.openai.azure.com`).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "azure_openai_resource"
    )]
    pub resource: Option<String>,
    /// Azure deployment name: the deployment created in Azure AI Studio.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "azure_openai_deployment"
    )]
    pub deployment: Option<String>,
    /// Azure API version string (e.g. `2024-10-21`).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "azure_openai_api_version"
    )]
    pub api_version: Option<String>,
}

// ── Anthropic ──

/// Anthropic canonical endpoint.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AnthropicEndpoint {
    #[default]
    Default,
}

impl ModelEndpoint for AnthropicEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.anthropic.com",
        }
    }
}

/// Anthropic model model_provider config. No family-specific extras yet — typed
/// slot reserved for future Anthropic-only knobs (cache_control, beta
/// headers) so they land cleanly without another schema rework.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.anthropic"]
pub struct AnthropicModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Moonshot (multi-region exemplar) ──

/// Moonshot endpoint variants. Operators pick the region that matches their
/// account; the runtime resolves the URI from the chosen variant unless
/// overridden by `base.uri`. Code variant is intl-only.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MoonshotEndpoint {
    /// Mainland China endpoint.
    Cn,
    /// International endpoint.
    #[default]
    Intl,
    /// Code-specialist endpoint (intl).
    Code,
}

impl ModelEndpoint for MoonshotEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Cn => "https://api.moonshot.cn/v1",
            Self::Intl => "https://api.moonshot.ai/v1",
            // Kimi Code (Kimi For Coding) moved to api.kimi.com — the old api.moonshot.cn/coder/v1 returns 404.
            Self::Code => "https://api.kimi.com/coding/v1",
        }
    }
}

/// Moonshot model model_provider config. The `endpoint` field is required (no
/// implicit default) — operators must pick a region explicitly. Migration
/// fills it in from collapsed `moonshot-cn` / `moonshot-intl` outer keys.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.moonshot"]
pub struct MoonshotModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
    /// Required: pick `cn`, `intl`, or `code`. Defaults to `intl` when omitted
    /// to ease transition; operators on the China endpoint should set
    /// `endpoint = "cn"` explicitly.
    #[serde(default)]
    pub endpoint: MoonshotEndpoint,
}

impl FamilyEndpoint for MoonshotModelProviderConfig {
    fn endpoint_uri(&self) -> Option<&'static str> {
        Some(self.endpoint.uri())
    }
}

// ── Qwen (multi-region + auth_mode exemplar) ──

/// Qwen endpoint variants. Operators pick the region matching their account.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum QwenEndpoint {
    /// Mainland China (DashScope).
    Cn,
    /// International (alicloud international).
    #[default]
    Intl,
    /// United States (DashScope US).
    Us,
    /// Code-specialist endpoint.
    Code,
}

impl ModelEndpoint for QwenEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Cn => "https://dashscope.aliyuncs.com/compatible-mode/v1",
            Self::Intl => "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
            Self::Us => "https://dashscope-us.aliyuncs.com/compatible-mode/v1",
            Self::Code => {
                "https://dashscope.aliyuncs.com/api/v1/services/aigc/text-generation/generation"
            }
        }
    }
}

/// Qwen model model_provider config. Multi-region (`endpoint` required) and
/// supports both API key and OAuth flows (`auth_mode` chooses which).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.qwen"]
pub struct QwenModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
    #[serde(default)]
    pub endpoint: QwenEndpoint,
    /// Auth flow. Defaults to `api_key`; set to `oauth` to use the vendor's
    /// OAuth-cache integration instead of the `api_key` field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<AuthMode>,
    /// Long-lived Qwen OAuth refresh token. When set, the runtime
    /// exchanges it for a short-lived access token at provider
    /// construction time. Operators relying on the upstream `qwen login`
    /// tool (which writes `~/.qwen/oauth_creds.json`) leave this unset;
    /// the file-cache integration takes over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[secret(category = "model_provider")]
    pub oauth_refresh_token: Option<String>,
    /// Override of Qwen's published OAuth client_id. Most operators
    /// should leave this unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_client_id: Option<String>,
    /// Operator override of the resource URL the refreshed access token
    /// is paired with. When unset, the runtime falls back to the
    /// `endpoint`-derived URL (or the cached `resource_url` when reading
    /// from `~/.qwen/oauth_creds.json`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_resource_url: Option<String>,
}

impl FamilyEndpoint for QwenModelProviderConfig {
    fn endpoint_uri(&self) -> Option<&'static str> {
        Some(self.endpoint.uri())
    }
}

// ── OpenRouter ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OpenRouterEndpoint {
    #[default]
    Default,
}

impl ModelEndpoint for OpenRouterEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://openrouter.ai/api/v1",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.openrouter"]
pub struct OpenRouterModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Ollama (local-default endpoint) ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OllamaEndpoint {
    #[default]
    LocalDefault,
}

impl ModelEndpoint for OllamaEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::LocalDefault => "http://localhost:11434",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.ollama"]
pub struct OllamaModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
    /// Override the Ollama `num_ctx` (context window, in tokens) sent on
    /// every `/api/chat` request. Defaults to the framework constant
    /// (`OLLAMA_DEFAULT_NUM_CTX`) when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_ctx: Option<u32>,
    /// Override the Ollama `num_predict` (max output tokens) sent on every
    /// `/api/chat` request. Defaults to the framework constant
    /// (`OLLAMA_DEFAULT_NUM_PREDICT`) when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_predict: Option<i32>,
    /// Force every Ollama `/api/chat` request to use this temperature,
    /// overriding the per-call value passed through
    /// `ModelProvider::chat_with_system(.., temperature)`. When unset
    /// (`None`, the default), the per-call temperature wins: full
    /// backward compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature_override: Option<f64>,
}

// ── Together ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum TogetherEndpoint {
    #[default]
    Default,
}

impl ModelEndpoint for TogetherEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.together.xyz/v1",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.together"]
pub struct TogetherModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Fireworks ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FireworksEndpoint {
    #[default]
    Default,
}

impl ModelEndpoint for FireworksEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.fireworks.ai/inference/v1",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.fireworks"]
pub struct FireworksModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Groq ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum GroqEndpoint {
    #[default]
    Default,
}

impl ModelEndpoint for GroqEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.groq.com/openai/v1",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.groq"]
pub struct GroqModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Mistral ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MistralEndpoint {
    #[default]
    Default,
}

impl ModelEndpoint for MistralEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.mistral.ai/v1",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.mistral"]
pub struct MistralModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Atomic Chat (local OpenAI-compatible runtime, e.g. Jan) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AtomicChatEndpoint {
    #[default]
    Default,
}

impl ModelEndpoint for AtomicChatEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "http://127.0.0.1:1337/v1",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.atomic_chat"]
pub struct AtomicChatModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── DeepSeek ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DeepseekEndpoint {
    #[default]
    Default,
}

impl ModelEndpoint for DeepseekEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.deepseek.com/v1",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.deepseek"]
pub struct DeepseekModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Cohere ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CohereEndpoint {
    #[default]
    Default,
}

impl ModelEndpoint for CohereEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.cohere.ai/compatibility/v1",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.cohere"]
pub struct CohereModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Perplexity ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PerplexityEndpoint {
    #[default]
    Default,
}

impl ModelEndpoint for PerplexityEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.perplexity.ai",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.perplexity"]
pub struct PerplexityModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── xAI (Grok) ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum XaiEndpoint {
    #[default]
    Default,
}

impl ModelEndpoint for XaiEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.x.ai/v1",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.xai"]
pub struct XaiModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Cerebras ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CerebrasEndpoint {
    #[default]
    Default,
}

impl ModelEndpoint for CerebrasEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.cerebras.ai/v1",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.cerebras"]
pub struct CerebrasModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── SambaNova ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SambanovaEndpoint {
    #[default]
    Default,
}

impl ModelEndpoint for SambanovaEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.sambanova.ai/v1",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.sambanova"]
pub struct SambanovaModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Hyperbolic ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum HyperbolicEndpoint {
    #[default]
    Default,
}

impl ModelEndpoint for HyperbolicEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.hyperbolic.xyz/v1",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.hyperbolic"]
pub struct HyperbolicModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── DeepInfra ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DeepinfraEndpoint {
    #[default]
    Default,
}

impl ModelEndpoint for DeepinfraEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.deepinfra.com/v1/openai",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.deepinfra"]
pub struct DeepinfraModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Hugging Face ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum HuggingfaceEndpoint {
    #[default]
    Default,
}

impl ModelEndpoint for HuggingfaceEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://router.huggingface.co/v1",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.huggingface"]
pub struct HuggingfaceModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── AI21 ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Ai21Endpoint {
    #[default]
    Default,
}
impl ModelEndpoint for Ai21Endpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.ai21.com/studio/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.ai21"]
pub struct Ai21ModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Reka ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum RekaEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for RekaEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.reka.ai/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.reka"]
pub struct RekaModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── BaseTen ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum BasetenEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for BasetenEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://inference.baseten.co/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.baseten"]
pub struct BasetenModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── NScale ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum NscaleEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for NscaleEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://inference.api.nscale.com/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.nscale"]
pub struct NscaleModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── AnyScale ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AnyscaleEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for AnyscaleEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.endpoints.anyscale.com/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.anyscale"]
pub struct AnyscaleModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Nebius ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum NebiusEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for NebiusEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.tokenfactory.nebius.com/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.nebius"]
pub struct NebiusModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Friendli ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FriendliEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for FriendliEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.friendli.ai/serverless/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.friendli"]
pub struct FriendliModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Stepfun ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum StepfunEndpoint {
    /// Mainland China endpoint.
    Cn,
    /// International endpoint.
    #[default]
    Intl,
}
impl ModelEndpoint for StepfunEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Cn => "https://api.stepfun.com/v1",
            Self::Intl => "https://api.stepfun.ai/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.stepfun"]
pub struct StepfunModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
    #[serde(default)]
    pub endpoint: StepfunEndpoint,
}

impl FamilyEndpoint for StepfunModelProviderConfig {
    fn endpoint_uri(&self) -> Option<&'static str> {
        Some(self.endpoint.uri())
    }
}

// ── AIHubMix ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AihubmixEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for AihubmixEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://aihubmix.com/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.aihubmix"]
pub struct AihubmixModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── SiliconFlow ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SiliconflowEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for SiliconflowEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.siliconflow.com/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.siliconflow"]
pub struct SiliconflowModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Astrai ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AstraiEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for AstraiEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://as-trai.com/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.astrai"]
pub struct AstraiModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Avian ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AvianEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for AvianEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.avian.io/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.avian"]
pub struct AvianModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── DeepMyst ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DeepmystEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for DeepmystEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.deepmyst.com/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.deepmyst"]
pub struct DeepmystModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Venice ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum VeniceEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for VeniceEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.venice.ai",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.venice"]
pub struct VeniceModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── NEAR AI Cloud ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum NearaiEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for NearaiEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://cloud-api.near.ai/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.nearai"]
pub struct NearaiModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Novita ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum NovitaEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for NovitaEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.novita.ai/openai",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.novita"]
pub struct NovitaModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── NVIDIA ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum NvidiaEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for NvidiaEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://integrate.api.nvidia.com/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.nvidia"]
pub struct NvidiaModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Telnyx ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum TelnyxEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for TelnyxEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.telnyx.com/v2",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.telnyx"]
pub struct TelnyxModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Vercel AI Gateway ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum VercelEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for VercelEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://ai-gateway.vercel.sh/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.vercel"]
pub struct VercelModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Cloudflare AI Gateway ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CloudflareEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for CloudflareEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://gateway.ai.cloudflare.com/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.cloudflare"]
pub struct CloudflareModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── OVH ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OvhEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for OvhEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://oai.endpoints.kepler.ai.cloud.ovh.net/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.ovh"]
pub struct OvhModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── GitHub Copilot ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CopilotEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for CopilotEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.githubcopilot.com",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.copilot"]
pub struct CopilotModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── GLM (multi-region) ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum GlmEndpoint {
    Cn,
    #[default]
    Global,
}
impl ModelEndpoint for GlmEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Cn => "https://open.bigmodel.cn/api/paas/v4",
            Self::Global => "https://api.z.ai/api/paas/v4",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.glm"]
pub struct GlmModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
    #[serde(default)]
    pub endpoint: GlmEndpoint,
}

impl FamilyEndpoint for GlmModelProviderConfig {
    fn endpoint_uri(&self) -> Option<&'static str> {
        Some(self.endpoint.uri())
    }
}

// ── Minimax (multi-region + auth_mode) ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MinimaxEndpoint {
    Cn,
    #[default]
    Intl,
}
impl ModelEndpoint for MinimaxEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Cn => "https://api.minimaxi.com/v1",
            Self::Intl => "https://api.minimax.io/v1",
        }
    }
}

impl MinimaxEndpoint {
    /// OAuth `/oauth/token` endpoint for this region. Used by
    /// `refresh_minimax_oauth_access_token` to mint short-lived access
    /// tokens from the operator-supplied `oauth_refresh_token`.
    pub fn oauth_token_endpoint(self) -> &'static str {
        match self {
            Self::Cn => "https://api.minimaxi.com/oauth/token",
            Self::Intl => "https://api.minimax.io/oauth/token",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.minimax"]
pub struct MinimaxModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
    #[serde(default)]
    pub endpoint: MinimaxEndpoint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<AuthMode>,
    /// Long-lived OAuth refresh token issued by MiniMax. When set, the
    /// runtime exchanges it for a short-lived access token at provider
    /// construction time and uses that as the API credential. Operators
    /// who prefer dashboard-generated long-lived API keys can leave this
    /// unset and populate `api_key` directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[secret(category = "model_provider")]
    pub oauth_refresh_token: Option<String>,
    /// Override of MiniMax's published OAuth client_id. Most operators
    /// should leave this unset; the runtime defaults to the
    /// vendor-published client_id (same one MiniMax's own portal uses).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_client_id: Option<String>,
}

impl FamilyEndpoint for MinimaxModelProviderConfig {
    fn endpoint_uri(&self) -> Option<&'static str> {
        Some(self.endpoint.uri())
    }
}

// ── Z.AI (multi-region) ──

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ZaiEndpoint {
    Cn,
    #[default]
    Global,
}
impl ModelEndpoint for ZaiEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Cn => "https://open.bigmodel.cn/api/coding/paas/v4",
            Self::Global => "https://api.z.ai/api/coding/paas/v4",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.zai"]
pub struct ZaiModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
    #[serde(default)]
    pub endpoint: ZaiEndpoint,
}

impl FamilyEndpoint for ZaiModelProviderConfig {
    fn endpoint_uri(&self) -> Option<&'static str> {
        Some(self.endpoint.uri())
    }
}

// ── Doubao (Volcengine; single canonical endpoint) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DoubaoEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for DoubaoEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://ark.cn-beijing.volces.com/api/v3",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.doubao"]
pub struct DoubaoModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Yi (Lingyiwanwu; single endpoint) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum YiEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for YiEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.lingyiwanwu.com/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.yi"]
pub struct YiModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Hunyuan (Tencent; single endpoint) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum HunyuanEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for HunyuanEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.hunyuan.cloud.tencent.com/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.hunyuan"]
pub struct HunyuanModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Qianfan (Baidu) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum QianfanEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for QianfanEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://qianfan.baidubce.com/v2",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.qianfan"]
pub struct QianfanModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Baichuan ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum BaichuanEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for BaichuanEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.baichuan-ai.com/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.baichuan"]
pub struct BaichuanModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Gemini (OAuth-capable) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum GeminiEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for GeminiEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://generativelanguage.googleapis.com/v1beta",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.gemini"]
pub struct GeminiModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
    /// Auth flow. Defaults to `api_key`; `oauth` uses GeminiModelProvider's
    /// OAuth-cache integration instead of the `api_key` field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<AuthMode>,
    /// Google OAuth app `client_id`, used when this alias drives ZeroClaw's
    /// own browser/device-code login flow (`zeroclaw auth login
    /// --model-provider gemini --profile <alias>`). Operators relying on
    /// the upstream `gemini login` tool don't need this; that tool writes
    /// its own client_id / client_secret into `~/.gemini/oauth_creds.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[secret(category = "model_provider")]
    pub oauth_client_id: Option<String>,
    /// Google OAuth app `client_secret`. Set alongside `oauth_client_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[secret(category = "model_provider")]
    pub oauth_client_secret: Option<String>,
    /// Pin a specific GCP project ID for the OAuth `loadCodeAssist`
    /// discovery call. When unset, the discovery probes for an
    /// already-onboarded project on the credential's account. Replaces
    /// `GOOGLE_CLOUD_PROJECT` / `GOOGLE_CLOUD_PROJECT_ID` env vars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_project: Option<String>,
}

// ── Gemini CLI (subprocess wrapper) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum GeminiCliEndpoint {
    #[default]
    LocalSubprocess,
}
impl ModelEndpoint for GeminiCliEndpoint {
    fn uri(&self) -> &'static str {
        // Subprocess — no remote endpoint. Sentinel for trait conformity.
        "subprocess://gemini-cli"
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.gemini_cli"]
pub struct GeminiCliModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
    /// Path to the `gemini` CLI binary. Falls back to `gemini` (PATH lookup).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
}

// ── LMStudio (local default) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LmstudioEndpoint {
    #[default]
    LocalDefault,
}
impl ModelEndpoint for LmstudioEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::LocalDefault => "http://localhost:1234/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.lmstudio"]
pub struct LmstudioModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── llama.cpp (local default) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LlamacppEndpoint {
    #[default]
    LocalDefault,
}
impl ModelEndpoint for LlamacppEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::LocalDefault => "http://localhost:8080/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.llamacpp"]
pub struct LlamacppModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── SGLang (local default) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SglangEndpoint {
    #[default]
    LocalDefault,
}
impl ModelEndpoint for SglangEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::LocalDefault => "http://localhost:30000/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.sglang"]
pub struct SglangModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── vLLM (local default) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum VllmEndpoint {
    #[default]
    LocalDefault,
}
impl ModelEndpoint for VllmEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::LocalDefault => "http://localhost:8000/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.vllm"]
pub struct VllmModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Osaurus (local default) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OsaurusEndpoint {
    #[default]
    LocalDefault,
}
impl ModelEndpoint for OsaurusEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::LocalDefault => "http://localhost:1337/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.osaurus"]
pub struct OsaurusModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── LiteLLM (operator-self-hosted gateway) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LitellmEndpoint {
    #[default]
    LocalDefault,
}
impl ModelEndpoint for LitellmEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::LocalDefault => "http://localhost:4000/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.litellm"]
pub struct LitellmModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Lepton ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LeptonEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for LeptonEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://llama3-1-405b.lepton.run/api/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.lepton"]
pub struct LeptonModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Morph ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MorphEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for MorphEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.morphllm.com/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.morph"]
pub struct MorphModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Manifest ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ManifestEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for ManifestEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://app.manifest.build/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.manifest"]
pub struct ManifestModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── GitHub Models ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum GithubModelsEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for GithubModelsEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://models.github.ai/inference",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.github_models"]
pub struct GithubModelsModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Upstage ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UpstageEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for UpstageEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.upstage.ai/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.upstage"]
pub struct UpstageModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Featherless ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FeatherlessEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for FeatherlessEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.featherless.ai/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.featherless"]
pub struct FeatherlessModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Arcee ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ArceeEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for ArceeEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            // Arcee publishes its OpenAI-compatible API at the `/api/v1` path
            // (not the conventional `/v1` root). Confirmed against Arcee docs.
            Self::Default => "https://api.arcee.ai/api/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.arcee"]
pub struct ArceeModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Lambda AI ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LambdaAiEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for LambdaAiEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.lambda.ai/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.lambda_ai"]
pub struct LambdaAiModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Inception ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum InceptionEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for InceptionEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.inceptionlabs.ai/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.inception"]
pub struct InceptionModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Synthetic ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SyntheticEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for SyntheticEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.synthetic.new/openai/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.synthetic"]
pub struct SyntheticModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── OpenCode (Zen) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OpencodeEndpoint {
    #[default]
    Default,
}
impl ModelEndpoint for OpencodeEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://opencode.ai/zen/v1",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.opencode"]
pub struct OpencodeModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── KiloCli (subprocess wrapper) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum KiloCliEndpoint {
    #[default]
    LocalSubprocess,
}
impl ModelEndpoint for KiloCliEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::LocalSubprocess => "subprocess://kilocli",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.kilocli"]
pub struct KiloCliModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
    /// Path to the `kilo` CLI binary. Falls back to `kilo` (PATH lookup).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
}

// ── Kilo (AI Gateway — OpenAI-compatible) ──

/// Kilo AI Gateway endpoint. Single canonical endpoint at kilo.ai.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum KiloEndpoint {
    #[default]
    Gateway,
}
impl ModelEndpoint for KiloEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Gateway => "https://api.kilo.ai/api/gateway",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.kilo"]
pub struct KiloModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
    /// Kilo endpoint variant. Defaults to the canonical Kilo AI Gateway.
    #[serde(default, skip_serializing_if = "KiloEndpoint::is_default")]
    pub endpoint: KiloEndpoint,
}

impl KiloEndpoint {
    fn is_default(&self) -> bool {
        matches!(self, Self::Gateway)
    }
}

impl FamilyEndpoint for KiloModelProviderConfig {
    fn endpoint_uri(&self) -> Option<&'static str> {
        Some(self.endpoint.uri())
    }
}

// ── Custom (user-supplied URL, no canonical default) ──

/// Custom catch-all for operator-defined endpoints. The endpoint variant has
/// no canonical URL — operators must always set `base.uri`. The trait return
/// is a sentinel string; the runtime constructor must verify `base.uri` is
/// set for `custom` entries and fail with a clear error if not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CustomEndpoint {
    #[default]
    OperatorSupplied,
}
impl ModelEndpoint for CustomEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::OperatorSupplied => "operator-supplied:set-cfg-uri",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.custom"]
pub struct CustomModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
}

// ── Bedrock (computed-endpoint exemplar, AWS region template) ──

/// AWS Bedrock endpoint template. Single variant; the URL is computed at
/// runtime by substituting `{region}` from the typed config field.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum BedrockEndpoint {
    #[default]
    Default,
}

impl ModelEndpoint for BedrockEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            // Bedrock URI is a template — substitution happens in the
            // BedrockModelProvider runtime constructor against cfg.region.
            Self::Default => "https://bedrock-runtime.{region}.amazonaws.com",
        }
    }
}

/// AWS Bedrock model model_provider config. Carries the AWS region (the URI
/// template substitutes `{region}` from this field). Bedrock auth is
/// SigV4 — credentials come from the standard AWS credential chain
/// (env vars, instance metadata, profile), not from `api_key`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.models.bedrock"]
pub struct BedrockModelProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: ModelProviderConfig,
    /// AWS region for the Bedrock endpoint (e.g. `us-east-1`, `eu-west-1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
}

// ── FamilyEndpoint default impls (single-endpoint families) ─────
//
// Multi-endpoint families (Moonshot, Qwen, Glm, Minimax, Zai, Stepfun) define
// their own `impl FamilyEndpoint` next to the struct. Every other family
// gets the `None` default via this list. The list is exhaustive: a new
// family with no impl here AND no manual impl elsewhere will fail to
// compile against `ModelProviders::resolved_endpoint_uri`, which expands
// `endpoint_uri()` per slot through `for_each_model_provider_slot!`.

macro_rules! impl_default_family_endpoint {
    ($($t:ty),+ $(,)?) => {
        $( impl FamilyEndpoint for $t {} )+
    };
}

impl_default_family_endpoint! {
    OpenAIModelProviderConfig,
    AzureModelProviderConfig,
    AnthropicModelProviderConfig,
    AtomicChatModelProviderConfig,
    OpenRouterModelProviderConfig,
    OllamaModelProviderConfig,
    TogetherModelProviderConfig,
    FireworksModelProviderConfig,
    GroqModelProviderConfig,
    MistralModelProviderConfig,
    DeepseekModelProviderConfig,
    CohereModelProviderConfig,
    PerplexityModelProviderConfig,
    XaiModelProviderConfig,
    CerebrasModelProviderConfig,
    SambanovaModelProviderConfig,
    HyperbolicModelProviderConfig,
    DeepinfraModelProviderConfig,
    HuggingfaceModelProviderConfig,
    Ai21ModelProviderConfig,
    RekaModelProviderConfig,
    BasetenModelProviderConfig,
    NscaleModelProviderConfig,
    AnyscaleModelProviderConfig,
    NebiusModelProviderConfig,
    FriendliModelProviderConfig,
    AihubmixModelProviderConfig,
    SiliconflowModelProviderConfig,
    AstraiModelProviderConfig,
    AvianModelProviderConfig,
    DeepmystModelProviderConfig,
    VeniceModelProviderConfig,
    NearaiModelProviderConfig,
    NovitaModelProviderConfig,
    NvidiaModelProviderConfig,
    TelnyxModelProviderConfig,
    VercelModelProviderConfig,
    CloudflareModelProviderConfig,
    OvhModelProviderConfig,
    CopilotModelProviderConfig,
    DoubaoModelProviderConfig,
    YiModelProviderConfig,
    HunyuanModelProviderConfig,
    QianfanModelProviderConfig,
    BaichuanModelProviderConfig,
    GeminiModelProviderConfig,
    GeminiCliModelProviderConfig,
    LmstudioModelProviderConfig,
    LlamacppModelProviderConfig,
    SglangModelProviderConfig,
    VllmModelProviderConfig,
    OsaurusModelProviderConfig,
    LitellmModelProviderConfig,
    LeptonModelProviderConfig,
    ManifestModelProviderConfig,
    MorphModelProviderConfig,
    GithubModelsModelProviderConfig,
    UpstageModelProviderConfig,
    FeatherlessModelProviderConfig,
    ArceeModelProviderConfig,
    LambdaAiModelProviderConfig,
    InceptionModelProviderConfig,
    SyntheticModelProviderConfig,
    OpencodeModelProviderConfig,
    KiloCliModelProviderConfig,
    CustomModelProviderConfig,
    BedrockModelProviderConfig,
}

// ── Aliased Agents ───────────────────────────────────────────────

/// Runtime tunables resolved from the agent's runtime profile. Populated
/// by `Config::resolved_agent_config`; never deserialized from the agent
/// table. The runtime profile is the sole config surface for these.
#[derive(Debug, Clone)]
pub struct ResolvedRuntime {
    pub compact_context: bool,
    pub max_tool_iterations: usize,
    pub max_history_messages: usize,
    /// Token budget for preemptive context/history trimming (from runtime profile).
    /// NOT the provider `max_tokens` output limit.
    pub max_context_tokens: usize,
    /// Model's context window (max input tokens) — from provider config.
    pub model_context_window: usize,
    pub parallel_tools: bool,
    pub tool_dispatcher: String,
    pub strict_tool_parsing: bool,
    pub tool_call_dedup_exempt: Vec<String>,
    pub tool_filter_groups: Vec<ToolFilterGroup>,
    pub max_system_prompt_chars: usize,
    pub thinking: crate::scattered_types::ThinkingConfig,
    pub history_pruning: crate::scattered_types::HistoryPrunerConfig,
    pub eval: crate::scattered_types::EvalConfig,
    pub auto_classify: Option<crate::scattered_types::AutoClassifyConfig>,
    pub context_compression: crate::scattered_types::ContextCompressionConfig,
    pub max_tool_result_chars: usize,
    pub keep_tool_context_turns: usize,
    pub tool_receipts: ToolReceiptsConfig,
    pub prompt_injection_mode: SkillsPromptInjectionMode,
}

impl ResolvedRuntime {
    /// Effective token budget for preemptive whole-turn history trimming.
    /// When `history_pruning.enabled` is set, an explicit `max_tokens` floor
    /// trims earlier than the hard context ceiling; otherwise the ceiling is
    /// the only trigger. Reuses the existing `history_pruning.*` idents.
    pub fn effective_context_budget(&self) -> usize {
        if self.history_pruning.enabled && self.history_pruning.max_tokens > 0 {
            self.max_context_tokens.min(self.history_pruning.max_tokens)
        } else {
            self.max_context_tokens
        }
    }
}

impl Default for ResolvedRuntime {
    fn default() -> Self {
        Self {
            compact_context: true,
            max_tool_iterations: 10,
            max_history_messages: 50,
            max_context_tokens: 32_000,
            model_context_window: 32_000,
            parallel_tools: false,
            tool_dispatcher: default_agent_tool_dispatcher(),
            strict_tool_parsing: false,
            tool_call_dedup_exempt: Vec::new(),
            tool_filter_groups: Vec::new(),
            max_system_prompt_chars: default_max_system_prompt_chars(),
            thinking: crate::scattered_types::ThinkingConfig::default(),
            history_pruning: crate::scattered_types::HistoryPrunerConfig::default(),
            eval: crate::scattered_types::EvalConfig::default(),
            auto_classify: None,
            context_compression: crate::scattered_types::ContextCompressionConfig::default(),
            max_tool_result_chars: default_max_tool_result_chars(),
            keep_tool_context_turns: default_keep_tool_context_turns(),
            tool_receipts: ToolReceiptsConfig::default(),
            prompt_injection_mode: SkillsPromptInjectionMode::default(),
        }
    }
}

/// Configuration for an aliased agent. Each `[agents.<alias>]` TOML
/// block deserializes into one of these.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "delegate_agent"]
pub struct AliasedAgentConfig {
    /// Whether this agent is active. Set false to disable without removing the definition.
    #[tab(General)]
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Channel aliases this agent handles (e.g. `["telegram.<alias>", "discord.<alias>"]`).
    /// Each entry is a `ChannelRef` resolving through `[channels.<type>.<alias>]`;
    /// `Config::validate()` fails loud on dangling references.
    #[tab(Channels)]
    #[serde(default)]
    pub channels: Vec<crate::providers::ChannelRef>,
    /// Dotted model-provider alias (e.g. `"anthropic.<alias>"`).
    /// Resolves through `model_providers.<type>.<alias>` at runtime;
    /// `Config::validate()` fails loud on dangling references.
    #[tab(Providers)]
    #[serde(default)]
    pub model_provider: crate::providers::ModelProviderRef,
    /// Risk profile alias (e.g. `"default"`). Resolves delegation guardrails at runtime.
    #[tab(General)]
    #[serde(default)]
    pub risk_profile: crate::providers::RiskProfileRef,
    /// Which `[personas.<alias>]` dial set shapes this agent's voice. Absent
    /// leaves every dial at `medium`, which renders no persona text at all.
    #[tab(General)]
    #[serde(default)]
    pub persona: crate::providers::PersonaRef,
    /// Which `[cards.<alias>]` defines this agent. A card supersedes the
    /// separate `persona` and `risk_profile` pointers: setting both a card and
    /// either of those is refused at validation rather than resolved by a
    /// precedence rule nobody would remember.
    #[tab(General)]
    #[serde(default)]
    pub card: crate::providers::CardRef,
    /// Runtime profile alias (e.g. `"default"`). Resolves agentic/iteration settings.
    #[tab(General)]
    #[serde(default)]
    pub runtime_profile: crate::providers::RuntimeProfileRef,
    /// Skill bundle aliases. Each entry resolves to
    /// `skill_bundles[key].directory` at runtime; the agent loads every
    /// listed bundle.
    #[tab(Bundles)]
    #[serde(default)]
    pub skill_bundles: Vec<String>,
    /// Knowledge bundle aliases. Additive: the agent loads every listed
    /// bundle.
    #[tab(Bundles)]
    #[serde(default)]
    pub knowledge_bundles: Vec<String>,
    /// MCP bundle aliases. Each entry references `mcp_bundles[key]`, a named
    /// group of MCP servers. Secure by default: an agent is granted only the
    /// servers named by its bundles. An agent with no `mcp_bundles` receives
    /// no MCP servers (omission is not a grant). See
    /// `Config::mcp_servers_for_agent`.
    #[tab(Bundles)]
    #[serde(default)]
    pub mcp_bundles: Vec<String>,
    /// Initialize this agent's `mcp_bundles` tools when it serves an ACP
    /// (`session/new`) session.
    ///
    /// Off by default: MCP servers are external processes/services that can
    /// block startup while they connect, and ACP `session/new` is expected to
    /// return promptly. Enable it when this agent must call its `mcp_bundles`
    /// tools over ACP; `session/new` then pays the one-time MCP connection cost
    /// (bounded and non-fatal per server). Set per agent so each ACP profile
    /// opts in independently; when this agent is the ACP default
    /// (`acp.default_agent`, or the sole configured agent), the flag is picked
    /// up automatically for sessions that omit `agentAlias`.
    #[tab(Bundles)]
    #[serde(default)]
    pub acp_enable_mcp: bool,
    /// Cron job aliases. Each entry references `cron[key]`, a declarative
    /// scheduled job invoked by the scheduler on its configured trigger.
    /// When the cron fires, this agent is the actor that executes the job.
    #[tab(Cron)]
    #[serde(default)]
    pub cron_jobs: Vec<String>,
    /// TTS provider as a dotted alias reference (`<type>.<alias>`,
    /// e.g. `"openai.<alias>"`). Resolves through `tts_providers.<type>.<alias>`.
    /// Empty = no TTS for this agent (there is no global default-provider concept;
    /// every agent that wants TTS sets its own `tts_provider`).
    #[tab(Providers)]
    #[serde(default)]
    pub tts_provider: crate::providers::TtsProviderRef,
    /// Transcription / STT provider as a dotted alias reference
    /// (`<type>.<alias>`, e.g. `"groq.<alias>"`). Resolves through
    /// `transcription_providers.<type>.<alias>`. Empty = agent has no
    /// transcription preference; channels that ingest voice still need a
    /// resolved provider (there is no global default), so an inbound voice
    /// flow into an agent with empty `transcription_provider` errors loudly
    /// at the channel boundary.
    #[tab(Providers)]
    #[serde(default)]
    pub transcription_provider: crate::providers::TranscriptionProviderRef,

    /// Optional override for the per-message LLM reply-intent classifier
    /// (`classify_channel_reply_intent` in zeroclaw-channels). When non-empty,
    /// the channel orchestrator routes the "should this message be replied to?"
    /// classification call to `[providers.models.<type>.<alias>]` referenced
    /// here, instead of reusing the main agent's `model_provider`.
    ///
    /// Source of truth for api_key / uri / model / temperature etc. is the
    /// referenced `[providers.models.<type>.<alias>]` entry. This field is
    /// a reference only (NEVER a copy), per AGENTS.md SINGLE SOURCE OF TRUTH.
    ///
    /// Empty (`Default`) = inherit the main agent's resolved provider+model
    /// (preserves pre-PR behavior; backward compatible).
    ///
    /// Use case: classification is a cheap REPLY/NO_REPLY decision, doesn't
    /// need a high-end model. Point this at a fast/free small model
    /// (e.g. `kimi-k2.5`, `qwen-turbo`) while `model_provider` stays on the
    /// expensive answering model (e.g. `qwen3.6-plus`).
    ///
    /// Note: TOML table names cannot contain `.`, so alias `kimi-k2.5`
    /// must be written as `[providers.models.custom.kimi-k2-5]`. The
    /// underlying `model = "kimi-k2.5"` string can still contain dots.
    ///
    /// ACP channels (IDE-direct) always reply and skip the classifier
    /// entirely, so this field has no effect on ACP traffic.
    #[tab(Providers)]
    #[serde(default)]
    pub classifier_provider: crate::providers::ModelProviderRef,

    /// Per-agent reply-intent precheck controls. The classifier call reads
    /// this block at message time; model/provider selection stays on
    /// `classifier_provider` so there is only one routing source of truth.
    #[tab(Channels)]
    #[serde(default)]
    #[nested]
    pub precheck: crate::scattered_types::ChannelPrecheckConfig,

    /// Per-agent override for the context-compression summarizer provider, as
    /// a `providers.models.<type>.<alias>` reference. Empty (Default) = inherit
    /// the runtime profile's `context_compression.summary_provider`, else the
    /// agent's own resolved provider+model. Reference only, never a copy;
    /// resolved by [`Config::effective_summary_provider`]. Validated in
    /// `Config::validate()`.
    #[tab(Providers)]
    #[serde(default)]
    pub summary_provider: crate::providers::ModelProviderRef,

    // ── Resolved runtime tunables (populated by `resolved_agent_config`
    // from the runtime profile; not config-settable on the agent). ──
    #[serde(skip)]
    pub resolved: ResolvedRuntime,

    /// Per-agent workspace block (`[agents.<alias>.workspace]`).
    /// Holds the agent's filesystem path, cross-agent access allowlist,
    /// filesystem-escape boolean, and cross-agent memory allowlist.
    /// Default is fully jailed (no cross-agent access). See
    /// `crate::multi_agent::AgentWorkspaceConfig`.
    #[tab(Workspace)]
    #[serde(default)]
    #[nested]
    pub workspace: crate::multi_agent::AgentWorkspaceConfig,

    /// Per-agent memory backend selection (`[agents.<alias>.memory]`).
    /// The `backend` field is locked at agent creation and immutable on
    /// subsequent loads. Defaults to `Sqlite`. See
    /// `crate::multi_agent::AgentMemoryConfig`.
    #[tab(Memory)]
    #[serde(default)]
    #[nested]
    pub memory: crate::multi_agent::AgentMemoryConfig,

    /// Per-agent identity format (`[agents.<alias>.identity]`). Each
    /// agent renders its own IDENTITY.md / SOUL.md inside its
    /// per-agent workspace; this block selects the format (OpenClaw or
    /// AIEOS) and optional inline/file source for the agent's identity
    /// document.
    #[tab(Tuning)]
    #[serde(default)]
    #[nested]
    pub identity: IdentityConfig,

    /// Per-agent A2A publication block (`[agents.<alias>.a2a]`). Gates
    /// whether this alias is discoverable as a spec-conforming A2A agent
    /// and which resolved skills appear on its card. Default-closed
    /// (`published = false`, no exposed skills). See
    /// `crate::multi_agent::AgentA2aConfig`.
    #[tab(General)]
    #[serde(default)]
    #[nested]
    pub a2a: crate::multi_agent::AgentA2aConfig,
}

impl Default for AliasedAgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            channels: Vec::new(),
            model_provider: crate::providers::ModelProviderRef::default(),
            risk_profile: crate::providers::RiskProfileRef::default(),
            persona: crate::providers::PersonaRef::default(),
            card: crate::providers::CardRef::default(),
            runtime_profile: crate::providers::RuntimeProfileRef::default(),
            skill_bundles: Vec::new(),
            knowledge_bundles: Vec::new(),
            mcp_bundles: Vec::new(),
            acp_enable_mcp: false,
            cron_jobs: Vec::new(),
            tts_provider: crate::providers::TtsProviderRef::default(),
            transcription_provider: crate::providers::TranscriptionProviderRef::default(),
            classifier_provider: crate::providers::ModelProviderRef::default(),
            precheck: crate::scattered_types::ChannelPrecheckConfig::default(),
            summary_provider: crate::providers::ModelProviderRef::default(),
            resolved: ResolvedRuntime::default(),
            workspace: crate::multi_agent::AgentWorkspaceConfig::default(),
            memory: crate::multi_agent::AgentMemoryConfig::default(),
            identity: IdentityConfig::default(),
            a2a: crate::multi_agent::AgentA2aConfig::default(),
        }
    }
}

/// One `[channels.<type>.<alias>]` block, with the owning agent (if any)
/// resolved via `agents.<agent>.channels`. Returned by
/// `Config::channels_by_alias()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct ChannelAliasInfo {
    /// Channel type as the schema emits it (kebab; e.g. `"discord"`,
    /// `"nextcloud-talk"`).
    pub channel_type: String,
    /// Per-alias HashMap key (e.g. `"loneliness"`).
    pub alias: String,
    /// The agent whose `channels` list contains `<type>.<alias>`. `None`
    /// when the block is orphaned (config error caught at startup).
    pub owning_agent: Option<String>,
    /// Resolved value of `[channels.<type>.<alias>].enabled` at scan time.
    /// `false` when the field is unset (matches the serde bool default).
    pub enabled: bool,
}

impl Config {
    /// Resolve the configured alias values valid for an [`crate::traits::AliasSource`].
    /// Two-tier sources return dotted `<type>.<alias>` keys; flat sources
    /// return bare alias keys. The single resolver every surface uses for
    /// `PropKind::AliasRef` pickers and validation.
    #[must_use]
    pub fn resolve_alias_source(&self, source: crate::traits::AliasSource) -> Vec<String> {
        let section = source.section_path();
        if source.is_two_tier() {
            let prefix = format!("{section}.");
            let mut out: Vec<String> = Vec::new();
            for f in self.prop_fields() {
                if let Some(rest) = f.name.strip_prefix(&prefix) {
                    let mut parts = rest.splitn(3, '.');
                    if let (Some(ty), Some(alias), Some(_)) =
                        (parts.next(), parts.next(), parts.next())
                    {
                        let dotted = format!("{ty}.{alias}");
                        if !out.contains(&dotted) {
                            out.push(dotted);
                        }
                    }
                }
            }
            out.sort();
            out
        } else {
            let mut out = self.get_map_keys(section).unwrap_or_default();
            out.sort();
            out
        }
    }

    /// Return the first concrete `model` string available for use as a
    /// default. Scans every typed slot's entries (iteration order is
    /// the macro slot order) for one with `model` set. Returns `None`
    /// only when no model-provider entry has any model configured at
    /// all.
    #[must_use]
    pub fn resolve_default_model(&self) -> Option<String> {
        self.providers
            .models
            .iter_entries()
            .filter_map(|(_, _, base)| base.model.as_deref().map(str::trim))
            .find(|m| !m.is_empty())
            .map(ToString::to_string)
    }

    /// Resolve the risk profile for an explicit agent alias.
    ///
    /// Each agent's `risk_profile` field names a `[risk_profiles.<alias>]`
    /// entry that gates its actions. There is no "global" risk profile in
    /// every callsite must come through an agent. When the agent has
    /// no profile set or names a missing entry, returns `None` and the
    /// caller decides how to handle it (validation rejects this shape at
    /// load time; the runtime treating `None` as a config error).
    ///
    /// A carded agent (`agents.<alias>.card` set) has an empty
    /// `agent.risk_profile` by construction — validation forbids setting
    /// both — and instead resolves through `cards[card].risk_profile`. That
    /// is the same invariant, only relocated: see `AgentCard::risk_profile`'s
    /// doc, "the card owns tools, the profile owns the rest". This lookup
    /// follows the card so every caller asking "what governs this agent's
    /// autonomy/sandbox/shell allow-list/always_ask" gets an answer whether
    /// or not the agent is carded. Tool grants are a separate concern —
    /// carded agents' `allowed_tools` come from the card, not from whatever
    /// this returns; see `SecurityPolicy::for_agent`.
    #[must_use]
    pub fn risk_profile_for_agent(&self, agent_alias: &str) -> Option<&RiskProfileConfig> {
        let profile_alias = self.resolved_risk_profile_alias(agent_alias)?;
        self.risk_profiles.get(profile_alias)
    }

    /// Resolve the risk-profile *alias* (not the `RiskProfileConfig` itself)
    /// that governs `agent_alias`: a direct `agents.<alias>.risk_profile` if
    /// non-empty, else the card's `risk_profile` when one governs this agent,
    /// else `None`. This is the single resolution [`Self::risk_profile_for_agent`]
    /// performs — extracted so callers that need to *compare* two agents'
    /// governing profile (rather than look one profile up) can do so without
    /// re-deriving the card-follow logic and, in doing so, silently reading
    /// the raw `agent.risk_profile` field instead — the field that is empty
    /// by construction for every carded agent (see that doc comment). Any
    /// site comparing or gating on "this agent's risk profile" should call
    /// this, not `agent.risk_profile` directly.
    ///
    /// Narrower contract than `risk_profile_for_agent`, on purpose: this
    /// returns the alias WITHOUT confirming a `[risk_profiles.<alias>]`
    /// entry exists, so a dangling alias comes back `Some`. That matches
    /// what the raw comparisons it replaced did (they never checked
    /// existence either), and `risk_profile_for_agent` still fails closed
    /// by doing the map lookup on top. A caller that needs "the profile
    /// exists" must use that function, not this one.
    #[must_use]
    fn resolved_risk_profile_alias(&self, agent_alias: &str) -> Option<&str> {
        let agent = self.agents.get(agent_alias)?;
        let profile_alias = agent.risk_profile.trim();
        if !profile_alias.is_empty() {
            return Some(profile_alias);
        }
        let card = agent.card.as_str().trim();
        if card.is_empty() {
            return None;
        }
        let carded_profile = self.cards.get(card)?.risk_profile.as_str().trim();
        if carded_profile.is_empty() {
            return None;
        }
        Some(carded_profile)
    }

    /// True when `agent_alias` names a configured, enabled agent with the
    /// bindings required to dispatch a turn: non-empty `model_provider`, a
    /// risk profile that resolves via [`Self::resolved_risk_profile_alias`]
    /// (so a carded agent's card-supplied profile counts, not just the raw
    /// `agents.<alias>.risk_profile` field, which is empty by construction
    /// for every carded agent), and non-empty `runtime_profile`.
    ///
    /// This lives on `Config`, not `AliasedAgentConfig`, because the
    /// risk-profile leg needs `Config::cards` to follow a card — the same
    /// reason `resolved_risk_profile_alias` itself is a `Config` method
    /// (#21, sixth instance: `AliasedAgentConfig::is_dispatchable` used to
    /// read the raw field directly and reported every valid carded agent as
    /// undispatchable; that method has been removed, this replaces it, and
    /// both call sites in
    /// `zeroclaw-channels/src/orchestrator/acp_server.rs` now call this).
    #[must_use]
    pub fn agent_is_dispatchable(&self, agent_alias: &str) -> bool {
        let Some(agent) = self.agents.get(agent_alias) else {
            return false;
        };
        agent.enabled
            && !agent.model_provider.trim().is_empty()
            && self.resolved_risk_profile_alias(agent_alias).is_some()
            && !agent.runtime_profile.trim().is_empty()
    }

    /// Resolve the persona dial set for an explicit agent alias.
    ///
    /// Mirrors [`Self::risk_profile_for_agent`]'s resolution order exactly:
    /// a direct `agents.<alias>.persona` wins; else follow the card
    /// (`agents.<alias>.card` -> `cards[card].persona`); else `None`. A
    /// carded agent's own `agent.persona` field is empty by construction
    /// (validation forbids setting both a card and a persona), so a carded
    /// agent always resolves through the card once one names a persona.
    /// `None` means "no dials configured" — the caller renders no `## Voice`
    /// section, not an error, unlike the risk-profile case where every
    /// enabled agent must resolve to one.
    #[must_use]
    pub fn persona_for_agent(&self, agent_alias: &str) -> Option<&crate::persona::PersonaKnobs> {
        let agent = self.agents.get(agent_alias)?;
        let persona_alias = agent.persona.as_str().trim();
        if !persona_alias.is_empty() {
            return self.personas.get(persona_alias);
        }
        let card = agent.card.as_str().trim();
        if card.is_empty() {
            return None;
        }
        let carded_persona = self.cards.get(card)?.persona.as_str().trim();
        if carded_persona.is_empty() {
            return None;
        }
        self.personas.get(carded_persona)
    }

    /// Resolve the card that governs an explicit agent alias, if any.
    ///
    /// Mirrors [`Self::risk_profile_for_agent`]'s and
    /// [`Self::persona_for_agent`]'s card-following, but for the card itself
    /// rather than something the card points at: `agents.<alias>.card` names
    /// a `[cards.<alias>]` entry. Returns `None` when the agent has no card
    /// set, or when it names one that does not exist (a dangling reference —
    /// validation rejects this shape at load time; the runtime treats
    /// `None` the same as "uncarded"). This is the single accessor for
    /// "which card governs this agent" — callers that need the card's tool
    /// grants, persona, or risk profile by hand should go through this
    /// rather than re-deriving `agent.card.as_str().trim()` +
    /// `self.cards.get(..)` themselves.
    #[must_use]
    pub fn card_for_agent(&self, agent_alias: &str) -> Option<&crate::card::AgentCard> {
        let agent = self.agents.get(agent_alias)?;
        let card = agent.card.as_str().trim();
        if card.is_empty() {
            return None;
        }
        self.cards.get(card)
    }

    /// Resolve the `[runtime_profiles.<alias>]` entry owned by an agent
    /// (via `agents.<alias>.runtime_profile`). Returns `None` when the
    /// agent has no runtime profile set or names a missing entry. Unlike
    /// `risk_profile_for_agent`, the missing case is not a hard error
    /// because runtime budgets and tunables fall back to global defaults.
    #[must_use]
    pub fn runtime_profile_for_agent(&self, agent_alias: &str) -> Option<&RuntimeProfileConfig> {
        let agent = self.agents.get(agent_alias)?;
        let profile_alias = agent.runtime_profile.trim();
        if profile_alias.is_empty() {
            return None;
        }
        self.runtime_profiles.get(profile_alias)
    }

    /// Effective context-compression summarizer provider for an agent:
    /// agent-level `summary_provider` override → the runtime profile's
    /// `context_compression.summary_provider` → `None` (the caller then reuses
    /// the agent's own provider+model, optionally via the deprecated
    /// `summary_model` swap). Unlike the inert agent-inline tunables below, the
    /// agent-level override IS consulted — it's an explicit per-agent choice,
    /// mirroring `classifier_provider`'s "empty = inherit" semantics.
    #[must_use]
    pub fn effective_summary_provider(
        &self,
        agent_alias: &str,
    ) -> Option<crate::providers::ModelProviderRef> {
        if let Some(a) = self.agents.get(agent_alias)
            && !a.summary_provider.trim().is_empty()
        {
            return Some(a.summary_provider.clone());
        }
        self.runtime_profile_for_agent(agent_alias)
            .map(|p| &p.context_compression.summary_provider)
            .filter(|r| !r.trim().is_empty())
            .cloned()
    }

    // ── Effective per-agent runtime tunables ──────────────────────────
    //
    // Runtime tunables live on `[runtime_profiles.<profile>]`, referenced by an
    // agent via `agents.<alias>.runtime_profile`. A profile value that is
    // explicitly set (non-sentinel, i.e. `> 0` for `max_tool_iterations`) is
    // authoritative; otherwise the global default applies. Agent-inline copies
    // of these tunable keys are inert — superseded by runtime profiles (see the
    // `agent_level_tunable_keys_are_inert` test) — so they are deliberately not
    // consulted here as a fallback.

    #[must_use]
    pub fn effective_max_tool_iterations(&self, agent_alias: &str) -> usize {
        self.runtime_profile_for_agent(agent_alias)
            .map(|p| p.max_tool_iterations)
            .filter(|&v| v > 0)
            .unwrap_or(10)
    }

    #[must_use]
    pub fn effective_max_history_messages(&self, agent_alias: &str) -> usize {
        self.runtime_profile_for_agent(agent_alias)
            .and_then(|p| p.max_history_messages)
            .unwrap_or(50)
    }

    /// Resolve the whole-turn history cap used by structured `Agent` sessions.
    ///
    /// An explicit runtime-profile cap remains authoritative. When omitted, the
    /// cap scales with the profile's tool-iteration limit while preserving the
    /// legacy floor of 50 messages.
    #[must_use]
    pub fn effective_structured_max_history_messages(&self, agent_alias: &str) -> usize {
        if let Some(max_history_messages) = self
            .runtime_profile_for_agent(agent_alias)
            .and_then(|p| p.max_history_messages)
        {
            return max_history_messages;
        }

        // Each tool iteration adds two structural messages; user/final assistant
        // add two more, while the floor preserves the structured default cap of 50.
        self.effective_max_tool_iterations(agent_alias)
            .saturating_mul(2)
            .saturating_add(2)
            .max(50)
    }

    #[must_use]
    pub fn effective_max_context_tokens(&self, agent_alias: &str) -> usize {
        // Token budget for preemptive context/history trimming (runtime profile override).
        // This is NOT the provider max_tokens output limit and NOT the model's context window.
        self.runtime_profile_for_agent(agent_alias)
            .and_then(|p| p.max_context_tokens)
            .unwrap_or(32_000)
    }

    /// The model's context window exactly as configured, or `None` when no
    /// provider profile declares one.
    ///
    /// `None` means *unknown*, not "small". Report it to an operator as unset
    /// rather than resolving it to [`UNCONFIGURED_CONTEXT_WINDOW_FALLBACK`],
    /// which would present a stub as though it were the model's real capacity.
    /// Use [`Config::effective_model_context_window`] when a number is required
    /// for budget arithmetic.
    #[must_use]
    pub fn configured_model_context_window(&self, agent_alias: &str) -> Option<usize> {
        // Provider config (config.toml) is the source of truth for this value.
        self.resolved_model_provider_for_agent(agent_alias)
            .and_then(|(_, _, provider_config)| provider_config.context_window)
    }

    /// Returns the model's context window size (max input tokens).
    /// Source: provider config `context_window` →
    /// [`UNCONFIGURED_CONTEXT_WINDOW_FALLBACK`].
    /// Does NOT check runtime profile (that's for output budget).
    #[must_use]
    pub fn effective_model_context_window(&self, agent_alias: &str) -> usize {
        self.configured_model_context_window(agent_alias)
            .unwrap_or(UNCONFIGURED_CONTEXT_WINDOW_FALLBACK)
    }

    #[must_use]
    pub fn effective_memory_recall_limit(&self, agent_alias: &str) -> usize {
        let raw = self
            .runtime_profile_for_agent(agent_alias)
            .and_then(|p| p.memory_recall_limit)
            .unwrap_or(5);
        if raw == 0 { usize::MAX } else { raw }
    }

    #[must_use]
    pub fn effective_compact_context(&self, agent_alias: &str) -> bool {
        self.runtime_profile_for_agent(agent_alias)
            .and_then(|p| p.compact_context)
            .unwrap_or(true)
    }

    #[must_use]
    pub fn effective_parallel_tools(&self, agent_alias: &str) -> bool {
        self.runtime_profile_for_agent(agent_alias)
            .and_then(|p| p.parallel_tools)
            .unwrap_or(false)
    }

    #[must_use]
    pub fn effective_tool_dispatcher(&self, agent_alias: &str) -> String {
        self.runtime_profile_for_agent(agent_alias)
            .and_then(|p| p.tool_dispatcher.as_ref())
            .filter(|s| !s.trim().is_empty())
            .map_or_else(default_agent_tool_dispatcher, Clone::clone)
    }

    #[must_use]
    pub fn effective_tool_call_dedup_exempt(&self, agent_alias: &str) -> Vec<String> {
        self.runtime_profile_for_agent(agent_alias)
            .map(|p| p.tool_call_dedup_exempt.clone())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn effective_max_system_prompt_chars(&self, agent_alias: &str) -> usize {
        self.runtime_profile_for_agent(agent_alias)
            .and_then(|p| p.max_system_prompt_chars)
            .unwrap_or_else(default_max_system_prompt_chars)
    }

    #[must_use]
    pub fn effective_max_tool_result_chars(&self, agent_alias: &str) -> usize {
        self.runtime_profile_for_agent(agent_alias)
            .and_then(|p| p.max_tool_result_chars)
            .unwrap_or_else(default_max_tool_result_chars)
    }

    #[must_use]
    pub fn effective_keep_tool_context_turns(&self, agent_alias: &str) -> usize {
        self.runtime_profile_for_agent(agent_alias)
            .and_then(|p| p.keep_tool_context_turns)
            .unwrap_or_else(default_keep_tool_context_turns)
    }

    /// Return a clone of the named agent's `AliasedAgentConfig` with all
    /// runtime-profile overrides baked in. Use this when an `Agent` (or
    /// any other struct) needs to own a self-contained, already-resolved
    /// view of the agent's runtime knobs without holding a reference to
    /// the full `Config`.
    ///
    /// Returns `None` when `agent_alias` is not present in `agents`.
    ///
    /// Semantics: every field touched here mirrors the matching
    /// `effective_*` helper above. If you add a new runtime_profile knob,
    /// add it both to an `effective_*` helper *and* to this function so
    /// downstream consumers see consistent values regardless of which
    /// surface they read from.
    #[must_use]
    pub fn resolved_agent_config(&self, agent_alias: &str) -> Option<AliasedAgentConfig> {
        let mut out = self.agents.get(agent_alias)?.clone();
        // A card's `grants.mcp_bundles` REPLACES `agents.<alias>.mcp_bundles`,
        // it does not union with it — the same rule the card's tool grants
        // follow (a card is meant to be the complete statement of an agent's
        // reach; a union would mean the card no longer tells you what the
        // agent can reach). Fail closed: a carded agent whose card grants no
        // bundles gets no servers, even if the raw agent field (the pointer
        // the card supersedes) still has entries left over from before it
        // was carded.
        let card = out.card.as_str().trim().to_string();
        if !card.is_empty() {
            out.mcp_bundles = self
                .cards
                .get(card.as_str())
                .map(|c| c.grants.mcp_bundles.clone())
                .unwrap_or_default();
        }
        let mut resolved = ResolvedRuntime {
            max_tool_iterations: self.effective_max_tool_iterations(agent_alias),
            max_history_messages: self.effective_max_history_messages(agent_alias),
            // Token budget for context/history trimming — from runtime profile
            max_context_tokens: self.effective_max_context_tokens(agent_alias),
            // Model's context window (max input tokens) — from provider config
            model_context_window: self.effective_model_context_window(agent_alias),
            compact_context: self.effective_compact_context(agent_alias),
            parallel_tools: self.effective_parallel_tools(agent_alias),
            tool_dispatcher: self.effective_tool_dispatcher(agent_alias),
            tool_call_dedup_exempt: self.effective_tool_call_dedup_exempt(agent_alias),
            max_system_prompt_chars: self.effective_max_system_prompt_chars(agent_alias),
            max_tool_result_chars: self.effective_max_tool_result_chars(agent_alias),
            keep_tool_context_turns: self.effective_keep_tool_context_turns(agent_alias),
            prompt_injection_mode: self.effective_skills_prompt_mode(agent_alias),
            ..ResolvedRuntime::default()
        };
        if let Some(profile) = self.runtime_profile_for_agent(agent_alias) {
            resolved.strict_tool_parsing = profile.strict_tool_parsing;
            resolved.thinking = profile.thinking.clone();
            resolved.history_pruning = profile.history_pruning.clone();
            resolved.eval = profile.eval.clone();
            resolved.auto_classify = profile.auto_classify.clone();
            resolved.context_compression = profile.context_compression.clone();
            resolved.tool_receipts = profile.tool_receipts.clone();
            resolved.tool_filter_groups = profile.tool_filter_groups.clone();
        }
        out.resolved = resolved;
        Some(out)
    }

    /// Resolve the effective skills prompt-injection mode for an agent: the
    /// agent's resolved runtime profile's `prompt_injection_mode` override when
    /// set, otherwise the global `[skills] prompt_injection_mode`. Agents with
    /// no runtime profile (or an unknown alias) fall back to the global value.
    ///
    /// Keyed on the resolved runtime profile — the sanctioned surface for
    /// per-agent runtime tunables — so agent-inline knobs stay inert.
    /// Centralizing the fallback here keeps the system-prompt builder and the
    /// `read_skill` tool-registration gate in lockstep on one effective mode,
    /// so a compact prompt is never paired with a missing `read_skill` tool
    /// (or a full prompt with a spurious one).
    #[must_use]
    pub fn effective_skills_prompt_mode(&self, agent_alias: &str) -> SkillsPromptInjectionMode {
        self.runtime_profile_for_agent(agent_alias)
            .and_then(|p| p.prompt_injection_mode)
            .unwrap_or(self.skills.prompt_injection_mode)
    }

    /// Resolve an agent's `model_provider` reference (`"<type>.<alias>"`) to
    /// its concrete `ModelProviderConfig` entry. Returns `None` when the
    /// agent doesn't exist, the reference is unparseable, or the
    /// `<type>.<alias>` pair doesn't resolve in `providers.models`.
    ///
    /// This is the lookup the orchestrator uses to build per-agent
    /// model_provider runtime options via explicit `<type>.<alias>`
    /// resolution — there is no concept of a "first" or "default"
    /// provider. The matching split logic lives in
    /// `crates/zeroclaw-runtime/src/tools/delegate.rs::resolve_brain` for
    /// the delegation path; this helper exposes the same contract for the
    /// channel-server startup path.
    #[must_use]
    pub fn model_provider_for_agent(&self, agent_alias: &str) -> Option<&ModelProviderConfig> {
        let agent = self.agents.get(agent_alias)?;
        let (type_key, alias_key) = agent.model_provider.split_once('.')?;
        self.providers.models.find(type_key, alias_key)
    }

    /// Resolve `(provider_type, provider_alias, &ModelProviderConfig)` for an
    /// agent. Same lookup as `model_provider_for_agent` but also returns the
    /// `'static` type key that downstream provider factories
    /// (`create_routed_model_provider_with_options`, etc.) need. Returns
    /// `None` when the agent has no `model_provider` set, when the reference
    /// is unparseable, or when the resolved entry has been deleted from
    /// `providers.models`.
    #[must_use]
    pub fn resolved_model_provider_for_agent(
        &self,
        agent_alias: &str,
    ) -> Option<(&'static str, &str, &ModelProviderConfig)> {
        let agent = self.agents.get(agent_alias)?;
        let (type_key, alias_key) = agent.model_provider.split_once('.')?;
        self.providers
            .models
            .iter_entries()
            .find(|(ty, al, _)| *ty == type_key && *al == alias_key)
    }

    /// Reverse-lookup the agent alias that owns a configured channel
    /// (`<type>.<alias>`). Returns the first agent listing the channel in
    /// its `channels` field. `None` when no agent owns the channel —
    /// orphaned channels are a config error the orchestrator surfaces at
    /// startup.
    #[must_use]
    pub fn agent_for_channel(&self, channel_alias: &str) -> Option<&str> {
        self.agents
            .iter()
            .find(|(_, agent)| agent.enabled && agent.channels.iter().any(|c| c == channel_alias))
            .map(|(alias, _)| alias.as_str())
    }

    /// Workspace dir a channel's inbound-media handler writes into. Resolves
    /// the channel's owning agent and returns `<install>/agents/<alias>/workspace/`;
    /// falls back to `data_dir` for orphan channels (no owning agent enabled).
    #[must_use]
    pub fn channel_workspace_dir(&self, channel_ref: &str) -> PathBuf {
        self.agent_for_channel(channel_ref)
            .map_or_else(|| self.data_dir.clone(), |a| self.agent_workspace_dir(a))
    }

    /// Schema-walk: every populated `[channels.<type>.<alias>]` block.
    /// Type names come from the `prop_fields()` enumeration (kebab as the
    /// macro emits them) so adding a new channel type via the macro
    /// surfaces here without touching this code. Alias keys are HashMap
    /// keys; not kebab-converted.
    #[must_use]
    pub fn channels_by_alias(&self) -> Vec<ChannelAliasInfo> {
        use std::collections::BTreeMap;
        let mut seen: BTreeMap<(String, String), bool> = BTreeMap::new();
        for field in self.prop_fields() {
            let parts: Vec<&str> = field.name.split('.').collect();
            if parts.len() < 4 || parts[0] != "channels" {
                continue;
            }
            let key = (parts[1].to_string(), parts[2].to_string());
            let entry = seen.entry(key).or_insert(false);
            if parts.len() == 4 && parts[3] == "enabled" {
                *entry = field.display_value == "true";
            }
        }
        seen.into_iter()
            .map(|((channel_type, alias), enabled)| {
                let composite = format!("{channel_type}.{alias}");
                let owning_agent = self.agent_for_channel(&composite).map(str::to_string);
                ChannelAliasInfo {
                    channel_type,
                    alias,
                    owning_agent,
                    enabled,
                }
            })
            .collect()
    }

    /// Reverse-lookup the agent alias that owns a declaratively-configured
    /// cron job (`[cron.<alias>]`). Returns the first agent listing the
    /// alias in its `cron_jobs` field. `None` when no agent claims the
    /// job — orphaned cron jobs are skipped at scheduler time with a
    /// warning. Imperative jobs (created at runtime via `cron_add`) have
    /// UUID-shaped ids that won't match any agent's `cron_jobs`; the
    /// scheduler treats those separately (carrying their owning agent
    /// alongside the DB row is a follow-up).
    #[must_use]
    pub fn agent_for_cron_job(&self, cron_alias: &str) -> Option<&str> {
        self.agents
            .iter()
            .find(|(_, agent)| agent.enabled && agent.cron_jobs.iter().any(|c| c == cron_alias))
            .map(|(alias, _)| alias.as_str())
    }

    /// Resolve the per-agent workspace directory for `alias`.
    ///
    /// Returns the agent's `[agents.<alias>.workspace.path]` override
    /// when set (operator-explicit, e.g. for putting a workspace on a
    /// different disk), otherwise derives
    /// `<install>/agents/<alias>/workspace/` from the install root
    /// (the directory containing `config.toml`).
    ///
    /// Per-agent workspaces live under
    /// `<install>/agents/<alias>/workspace/` and hold the agent's
    /// markdown memory (MEMORY.md), identity files (IDENTITY.md,
    /// SOUL.md), and any other per-agent plaintext state. Shared
    /// databases (SQLite memory, sessions, cost records) live under
    /// `config.data_dir` instead and partition by agent at the row
    /// level. Per-agent overrides via `[agents.<alias>.workspace.path]`
    /// pin an arbitrary filesystem path (e.g. a different mount).
    #[must_use]
    pub fn agent_workspace_dir(&self, agent_alias: &str) -> std::path::PathBuf {
        if let Some(cfg) = self.agents.get(agent_alias)
            && let Some(custom) = cfg.workspace.path.as_ref()
        {
            return custom.clone();
        }
        self.install_root_dir()
            .join("agents")
            .join(agent_alias)
            .join("workspace")
    }

    /// Effective MCP servers granted to an agent by its `mcp_bundles`.
    ///
    /// Secure by default: omission is not a grant. An agent is reached via
    /// `resolved_agent_config`; an unknown alias or one with no `mcp_bundles`
    /// receives NO MCP servers. See [`Self::mcp_servers_for_bundles`] for how
    /// a non-empty bundle list resolves to servers.
    #[must_use]
    pub fn mcp_servers_for_agent(&self, agent_alias: &str) -> Vec<McpServerConfig> {
        match self.resolved_agent_config(agent_alias) {
            Some(agent) => self.mcp_servers_for_bundles(&agent.mcp_bundles),
            None => Vec::new(),
        }
    }

    /// Resolve a set of `[mcp_bundles.<alias>]` references to the concrete
    /// `[mcp.servers]` entries they grant.
    ///
    /// Secure by default: omission is not a grant.
    /// - An empty `bundle_aliases` grants NO servers (`Vec::new()`).
    /// - The grant is the union of each referenced bundle's `servers`, in
    ///   first-seen order with duplicates dropped.
    /// - Deny wins: a server name listed in ANY referenced bundle's `exclude`
    ///   is removed from the grant, regardless of which bundle included it.
    /// - An unknown bundle alias grants nothing (it is skipped, not an error)
    ///   and a server name with no matching `[mcp.servers]` entry grants
    ///   nothing. Both fail closed: a misconfiguration narrows access, never
    ///   widens it.
    #[must_use]
    pub fn mcp_servers_for_bundles(&self, bundle_aliases: &[String]) -> Vec<McpServerConfig> {
        if bundle_aliases.is_empty() {
            return Vec::new();
        }
        // Deny wins: collect every excluded name across the referenced bundles
        // first, so an include in one bundle cannot defeat an exclude in another.
        let mut excluded: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for alias in bundle_aliases {
            if let Some(bundle) = self.mcp_bundles.get(alias) {
                for name in &bundle.exclude {
                    excluded.insert(name.as_str());
                }
            }
        }
        // Union of granted names in first-seen order, skipping excludes.
        let mut granted: Vec<&str> = Vec::new();
        for alias in bundle_aliases {
            if let Some(bundle) = self.mcp_bundles.get(alias) {
                for name in &bundle.servers {
                    let n = name.as_str();
                    if !excluded.contains(n) && !granted.contains(&n) {
                        granted.push(n);
                    }
                }
            }
        }
        // Resolve names against the configured servers. A name that matches no
        // `[mcp.servers]` entry is not granted.
        granted
            .into_iter()
            .filter_map(|name| self.mcp.servers.iter().find(|s| s.name == name).cloned())
            .collect()
    }

    /// `<install>/shared/` — directory shared across every agent on this
    /// host. Holds skills, skill bundles, knowledge bundles, and any
    /// other content not scoped to a single agent's workspace. Distinct
    /// from `agent_workspace_dir(alias)` (per-agent state) and
    /// `data_dir` (databases + runtime state).
    #[must_use]
    pub fn shared_workspace_dir(&self) -> std::path::PathBuf {
        self.install_root_dir().join("shared")
    }

    /// Install root: `<install>/` derived from `config_path`'s parent. Used
    /// to compute `<install>/shared/`, `<install>/agents/`, and the
    /// skill-bundle directory defaults. Public so consumers (gateway, CLI,
    /// SkillsService) share the same anchor.
    #[must_use]
    pub fn install_root_dir(&self) -> std::path::PathBuf {
        self.config_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("."))
    }

    /// Resolve an aliased-agent config by alias. `None` when the alias
    /// isn't configured; callers should treat this as a config error
    /// rather than synthesizing a default.
    #[must_use]
    pub fn agent(&self, agent_alias: &str) -> Option<&AliasedAgentConfig> {
        self.agents.get(agent_alias)
    }

    /// Resolve the runtime-active agent alias the orchestrator binds
    /// channels to. Mirrors the same selection logic as
    /// `start_channels()` in zeroclaw-channels: prefer the migration-
    /// synthesized `"default"` agent, otherwise fall back to the
    /// lexicographically-smallest enabled alias. Returns `None` only
    /// when no enabled agent is configured.
    ///
    /// Used by per-agent infrastructure (TtsManager, TranscriptionManager)
    /// to pick which agent's `tts_provider` / `transcription_provider`
    /// drives the manager's resolved alias. Until the per-channel
    /// dispatch refactor lands, the orchestrator runs in single-agent
    /// mode, so all manager instances share the same resolved agent.
    #[must_use]
    pub fn resolved_runtime_agent_alias(&self) -> Option<&str> {
        self.agents
            .keys()
            .find(|k| k.as_str() == "default")
            .map(String::as_str)
            .or_else(|| {
                self.agents
                    .iter()
                    .filter(|(_, a)| a.enabled)
                    .map(|(alias, _)| alias.as_str())
                    .min()
            })
    }

    /// Resolve the active storage backend for the memory subsystem.
    ///
    /// `MemoryConfig.backend` is a dotted reference (`<backend>.<alias>`) into
    /// `Config.storage.<backend>.<alias>`. Bare backend names are interpreted
    /// as `<backend>.default` for back-compat.
    ///
    /// Returns `ActiveStorage::None` when no backend is configured, when the
    /// backend is `"none"`, or when the dotted alias does not resolve to a
    /// configured entry.
    pub fn resolve_active_storage(&self) -> ActiveStorage<'_> {
        let backend = self.memory.backend.trim();
        if backend.is_empty() || backend.eq_ignore_ascii_case("none") {
            return ActiveStorage::None;
        }
        let (kind, alias) = backend.split_once('.').unwrap_or((backend, "default"));
        match kind {
            "sqlite" => self
                .storage
                .sqlite
                .get(alias)
                .map(ActiveStorage::Sqlite)
                .unwrap_or(ActiveStorage::None),
            "postgres" => self
                .storage
                .postgres
                .get(alias)
                .map(ActiveStorage::Postgres)
                .unwrap_or(ActiveStorage::None),
            "qdrant" => self
                .storage
                .qdrant
                .get(alias)
                .map(ActiveStorage::Qdrant)
                .unwrap_or(ActiveStorage::None),
            "markdown" => self
                .storage
                .markdown
                .get(alias)
                .map(ActiveStorage::Markdown)
                .unwrap_or(ActiveStorage::None),
            "lucid" => self
                .storage
                .lucid
                .get(alias)
                .map(ActiveStorage::Lucid)
                .unwrap_or(ActiveStorage::None),
            _ => ActiveStorage::None,
        }
    }
}

/// Resolved storage backend variant.
///
/// Returned from [`Config::resolve_active_storage`]. Each variant carries a
/// borrow of the typed config from the corresponding `Config.storage` map.
#[derive(Debug, Clone, Copy)]
pub enum ActiveStorage<'a> {
    /// No storage configured (`memory.backend = "none"` or unresolved alias).
    None,
    /// SQLite storage instance.
    Sqlite(&'a SqliteStorageConfig),
    /// PostgreSQL storage instance.
    Postgres(&'a PostgresStorageConfig),
    /// Qdrant storage instance.
    Qdrant(&'a QdrantStorageConfig),
    /// Markdown directory storage instance.
    Markdown(&'a MarkdownStorageConfig),
    /// Lucid CLI sync instance.
    Lucid(&'a LucidStorageConfig),
}

impl ActiveStorage<'_> {
    /// Backend type name (`"sqlite"`, `"postgres"`, etc.); `"none"` for unconfigured.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            ActiveStorage::None => "none",
            ActiveStorage::Sqlite(_) => "sqlite",
            ActiveStorage::Postgres(_) => "postgres",
            ActiveStorage::Qdrant(_) => "qdrant",
            ActiveStorage::Markdown(_) => "markdown",
            ActiveStorage::Lucid(_) => "lucid",
        }
    }
}

/// Valid temperature range for all paths (config, CLI, env override).
pub const TEMPERATURE_RANGE: std::ops::RangeInclusive<f64> = 0.0..=2.0;

/// Context window assumed when no provider profile declares one.
///
/// Deliberately conservative: it has to be safe for any model, which means it
/// is almost always *wrong* for the model actually in use — a 1M-context model
/// on a profile that omits `context_window` runs against this number. It exists
/// so budget arithmetic has an operand, not because it describes any model.
///
/// Anything reporting to an operator must call
/// [`Config::configured_model_context_window`] and say "not configured" when it
/// returns `None`, rather than echoing this value as the model's real capacity.
pub const UNCONFIGURED_CONTEXT_WINDOW_FALLBACK: usize = 32_000;

/// Defaults to 0 so configs without an explicit `schema_version` are recognized
/// as pre-versioning and get migrated.
fn default_schema_version() -> u32 {
    0
}

/// Per-channel reply-pacing accessor. Implemented by every `*Config`
/// struct that participates in outbound pacing so validation and
/// wrapper construction can walk all of them through a single
/// abstraction rather than nine duplicated method calls.
pub trait HasReplyPacing {
    fn reply_min_interval_secs(&self) -> u64;
    fn reply_queue_depth_max(&self) -> u16;
}

macro_rules! impl_reply_pacing {
    ($($ty:ty),+ $(,)?) => {
        $(impl HasReplyPacing for $ty {
            fn reply_min_interval_secs(&self) -> u64 { self.reply_min_interval_secs }
            fn reply_queue_depth_max(&self) -> u16 { self.reply_queue_depth_max }
        })+
    };
}

/// Inclusive upper bound (seconds) for per-channel `reply_min_interval_secs`.
pub const REPLY_MIN_INTERVAL_MAX_SECS: u64 = 3600;

/// Inclusive upper bound for per-channel `reply_queue_depth_max`. The lower
/// bound is `0`, where `0` means "use [`DEFAULT_REPLY_QUEUE_DEPTH`] at the
/// pacing-wrapper construction site." A non-zero value pins the bound
/// explicitly. Validator rejects values above this ceiling.
pub const REPLY_QUEUE_DEPTH_CEILING: u16 = 1024;

/// Fallback queue depth applied at the pacing-wrapper construction site
/// when a channel's `reply_queue_depth_max` is left at `0`. Sized for the
/// AI-pacing use case: a paced channel buffering more than this is a sign
/// the agent is producing replies faster than the floor will ever drain
/// them and the overflow log is the right signal.
pub const DEFAULT_REPLY_QUEUE_DEPTH: u16 = 16;

/// Idle-state LRU cap on the pacing wrapper's per-recipient rows.
/// Bounds growth when a bot legitimately serves many thousands of distinct
/// peers. Eviction only reclaims idle rows (no queued sends, no running
/// worker, no in-flight dispatch), so under a pathological all-active burst
/// the row count can temporarily exceed this target until rows become idle —
/// it is not an unconditional hard bound. Each recipient's queue depth stays
/// bounded regardless. Not exposed in config — promote to a schema field if
/// an operator reports hitting it.
pub const PACING_RECIPIENT_CAP: usize = 1024;

/// Validate that a temperature value is within the allowed range.
pub fn validate_temperature(value: f64) -> std::result::Result<f64, String> {
    if TEMPERATURE_RANGE.contains(&value) {
        Ok(value)
    } else {
        Err(format!(
            "temperature {value} is out of range (expected {}..={})",
            TEMPERATURE_RANGE.start(),
            TEMPERATURE_RANGE.end()
        ))
    }
}

fn normalize_reasoning_effort(value: &str) -> std::result::Result<String, String> {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "minimal" | "low" | "medium" | "high" | "xhigh" => Ok(normalized),
        _ => Err(format!(
            "reasoning_effort {value:?} is invalid (expected one of: minimal, low, medium, high, xhigh)"
        )),
    }
}

fn deserialize_reasoning_effort_opt<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: Option<String> = Option::deserialize(deserializer)?;
    value
        .map(|raw| normalize_reasoning_effort(&raw).map_err(serde::de::Error::custom))
        .transpose()
}

fn deserialize_enum_lenient<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let raw = serde_json::Value::deserialize(deserializer)?;
    Ok(T::deserialize(raw).unwrap_or_default())
}

/// Deserialize an `Option<String>` that maps an empty literal `""` to
/// `None`. Used by `JiraConfig::email` so a config that round-tripped
/// `email = ""` to disk (the legacy `email: String` had no
/// `skip_serializing_if`) doesn't deserialize as `Some("")` and silently
/// break Basic auth — the email-required validation was removed when
/// Server/DC Bearer-token support landed, so this is the last line of
/// defense.
fn deserialize_optional_email_skip_empty<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: Option<String> = Option::deserialize(deserializer)?;
    Ok(value.filter(|s| !s.trim().is_empty()))
}

// ── Hardware Config (wizard-driven) ─────────────────────────────

/// Hardware transport mode.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub enum HardwareTransport {
    #[default]
    None,
    Native,
    Serial,
    Probe,
}

impl std::fmt::Display for HardwareTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::Native => write!(f, "native"),
            Self::Serial => write!(f, "serial"),
            Self::Probe => write!(f, "probe"),
        }
    }
}

/// Wizard-driven hardware configuration for physical world interaction.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "hardware"]
pub struct HardwareConfig {
    /// Opt in to direct physical-hardware control — GPIO pins, USB-tethered microcontrollers (Arduino, ESP32, Nucleo), or SWD/JTAG debug probes. Leave off for software-only use; turning it on without the right transport configured does nothing.
    #[serde(default)]
    pub enabled: bool,
    /// How ZeroClaw reaches the hardware: `native` = Linux SBC with direct GPIO access (Raspberry Pi, Orange Pi); `serial` = USB-tethered microcontroller speaking over a TTY; `probe` = SWD/JTAG debug probe driving a target chip via probe-rs; `none` = disabled.
    #[serde(default)]
    pub transport: HardwareTransport,
    /// TTY path for the `serial` transport — e.g. `/dev/ttyACM0` on Linux, `/dev/tty.usbmodem1` on macOS, `COM3` on Windows. Ignored for other transports.
    #[serde(default)]
    pub serial_port: Option<String>,
    /// Baud rate negotiated on the serial link. 115200 matches the common Arduino / ESP32 bootloader default; bump to 230400+ when your firmware explicitly supports faster rates and you need the throughput.
    #[serde(default = "default_baud_rate")]
    pub baud_rate: u32,
    /// Target chip identifier for `transport = probe` (e.g. `STM32F401RE`, `nRF52840_xxAA`). Passed straight to probe-rs for flash/debug operations; must match a chip probe-rs recognizes.
    #[serde(default)]
    pub probe_target: Option<String>,
    /// Index pre-converted `.md` and `.txt` datasheets from the workspace into
    /// a local RAG store, so the agent can look up pin assignments and
    /// electrical specs inline when you ask hardware questions. PDFs are not
    /// parsed as text; download or convert them to `.md`/`.txt` first. Off by
    /// default — turn on once the workspace has relevant text datasheets.
    #[serde(default)]
    pub workspace_datasheets: bool,
}

fn default_baud_rate() -> u32 {
    115_200
}

impl HardwareConfig {
    /// Return the active transport mode.
    pub fn transport_mode(&self) -> HardwareTransport {
        self.transport.clone()
    }
}

impl Default for HardwareConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            transport: HardwareTransport::None,
            serial_port: None,
            baud_rate: default_baud_rate(),
            probe_target: None,
            workspace_datasheets: false,
        }
    }
}

// ── Transcription ────────────────────────────────────────────────

fn default_transcription_api_url() -> String {
    "https://api.groq.com/openai/v1/audio/transcriptions".into()
}

fn default_transcription_model() -> String {
    "whisper-large-v3-turbo".into()
}

fn default_transcription_max_duration_secs() -> u64 {
    120
}

fn default_openai_stt_model() -> String {
    "whisper-1".into()
}

fn default_deepgram_stt_model() -> String {
    "nova-2".into()
}

fn default_google_stt_language_code() -> String {
    "en-US".into()
}

/// Voice transcription configuration with multi-provider support.
///
/// The top-level `api_url`, `model`, and `api_key` fields remain for backward
/// compatibility with existing Groq-based configurations.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "transcription"]
pub struct TranscriptionConfig {
    /// Enable voice transcription for channels that support it.
    #[serde(default)]
    pub enabled: bool,
    /// API key used for transcription requests (Groq transcription provider).
    ///
    /// Use the schema-mirror env grammar (`ZEROCLAW_transcription__api_key=...`)
    /// for runtime env injection. Legacy `GROQ_API_KEY` constructor fallback was
    /// removed in v0.8.0.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_key: Option<String>,
    /// Whisper API endpoint URL (Groq transcription provider).
    #[serde(default = "default_transcription_api_url")]
    pub api_url: String,
    /// Whisper model name (Groq transcription provider).
    #[serde(default = "default_transcription_model")]
    pub model: String,
    /// Optional language hint (ISO-639-1, e.g. "en", "ru") for Groq transcription provider.
    #[serde(default)]
    pub language: Option<String>,
    /// Optional initial prompt to bias transcription toward expected vocabulary
    /// (proper nouns, technical terms, etc.). Sent as the `prompt` field in the
    /// Whisper API request.
    #[serde(default)]
    pub initial_prompt: Option<String>,
    /// Optional global audio size upper bound in bytes, enforced before
    /// dispatching to any transcription provider. Provider-specific caps still
    /// apply.
    #[serde(default)]
    pub max_audio_bytes: Option<usize>,
    /// Maximum voice duration in seconds (messages longer than this are skipped).
    #[serde(default = "default_transcription_max_duration_secs")]
    pub max_duration_secs: u64,
    /// OpenAI Whisper STT model_provider configuration.
    #[serde(default)]
    #[nested]
    pub openai: Option<OpenAiSttConfig>,
    /// Deepgram STT model_provider configuration.
    #[serde(default)]
    #[nested]
    pub deepgram: Option<DeepgramSttConfig>,
    /// AssemblyAI STT model_provider configuration.
    #[serde(default)]
    #[nested]
    pub assemblyai: Option<AssemblyAiSttConfig>,
    /// Google Cloud Speech-to-Text model_provider configuration.
    #[serde(default)]
    #[nested]
    pub google: Option<GoogleSttConfig>,
    /// Local/self-hosted Whisper-compatible STT model_provider.
    #[serde(default)]
    #[nested]
    pub local_whisper: Option<LocalWhisperConfig>,
    /// Also transcribe non-PTT (forwarded/regular) audio messages on WhatsApp,
    /// not just voice notes. Default: `false` (preserves legacy behavior).
    #[serde(default)]
    pub transcribe_non_ptt_audio: bool,
}

impl Default for TranscriptionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: None,
            api_url: default_transcription_api_url(),
            model: default_transcription_model(),
            language: None,
            initial_prompt: None,
            max_audio_bytes: None,
            max_duration_secs: default_transcription_max_duration_secs(),
            openai: None,
            deepgram: None,
            assemblyai: None,
            google: None,
            local_whisper: None,
            transcribe_non_ptt_audio: false,
        }
    }
}

// ── MCP ─────────────────────────────────────────────────────────

/// Transport type for MCP server connections.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    /// Spawn a local process and communicate over stdin/stdout.
    #[default]
    Stdio,
    /// Connect via HTTP POST.
    Http,
    /// Connect via HTTP + Server-Sent Events.
    Sse,
}

impl McpTransport {
    /// Wire token for this transport, matching the `rename_all = "lowercase"`
    /// serde representation (`stdio`/`http`/`sse`). The exhaustive match makes
    /// a new variant a compile error here rather than a silent miss.
    pub fn wire_name(self) -> &'static str {
        match self {
            McpTransport::Stdio => "stdio",
            McpTransport::Http => "http",
            McpTransport::Sse => "sse",
        }
    }

    /// The `McpServerConfig` leaf this transport requires to be non-empty.
    /// Single source of truth shared by `validate_mcp_config` (runtime
    /// enforcement) and the config-form `x-required-by-transport` schema
    /// metadata the Operator Console reads to mark the right field Required
    /// per transport. Changing the relationship here updates both consumers.
    pub fn required_leaf(self) -> &'static str {
        match self {
            McpTransport::Stdio => "command",
            McpTransport::Http | McpTransport::Sse => "url",
        }
    }
}

/// Every transport, enumerated on-demand from the schema rather than a
/// hand-maintained list, so the set is derived from the enum itself and a new
/// variant shows up here automatically (its `required_leaf` arm then forces a
/// decision). schemars emits a fieldless enum as a top-level `enum` array, or
/// as `oneOf` of `const`s once the variants carry doc comments; handle both,
/// matching the existing `enum_variants` helper. The wire tokens honor the
/// serde rename, so they round-trip straight back to the enum.
#[cfg(feature = "schema-export")]
fn mcp_transports() -> Vec<McpTransport> {
    let schema = serde_json::to_value(schemars::schema_for!(McpTransport)).unwrap_or_default();
    let wire_values: Vec<serde_json::Value> =
        if let Some(values) = schema.get("enum").and_then(serde_json::Value::as_array) {
            values.clone()
        } else if let Some(variants) = schema.get("oneOf").and_then(serde_json::Value::as_array) {
            variants
                .iter()
                .filter_map(|variant| variant.get("const").cloned())
                .collect()
        } else {
            Vec::new()
        };
    wire_values
        .into_iter()
        .filter_map(|value| serde_json::from_value::<McpTransport>(value).ok())
        .collect()
}

/// Per-transport required-leaf map, projected from [`McpTransport::required_leaf`]
/// onto the schema as the `x-required-by-transport` extension on
/// `McpServerConfig`. The Operator Console config form reads it so the Required
/// badge tracks the selected transport (`stdio` needs `command`, `http`/`sse`
/// need `url`) instead of hardcoding the relationship in the web surface.
#[cfg(feature = "schema-export")]
fn mcp_required_by_transport() -> serde_json::Value {
    let map: serde_json::Map<String, serde_json::Value> = mcp_transports()
        .into_iter()
        .map(|transport| {
            (
                transport.wire_name().to_string(),
                serde_json::Value::String(transport.required_leaf().to_string()),
            )
        })
        .collect();
    serde_json::Value::Object(map)
}

/// Configuration for a single external MCP server.
#[derive(Debug, Clone, Serialize, Deserialize, Default, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[cfg_attr(
    feature = "schema-export",
    schemars(extend("x-required-by-transport" = mcp_required_by_transport()))
)]
#[prefix = "mcp.servers"]
pub struct McpServerConfig {
    /// Display name used as a tool prefix (`<server>__<tool>`). Filled in
    /// from the supplied `map_key` when the entry is created via
    /// `create_map_key("mcp.servers", "<name>")`; `#[serde(default)]` lets
    /// the macro default-construct from `{}` before the name gets injected.
    #[serde(default)]
    pub name: String,
    /// Transport type (default: stdio).
    #[serde(default)]
    pub transport: McpTransport,
    /// URL for HTTP/SSE transports.
    #[serde(default)]
    pub url: Option<String>,
    /// Executable to spawn for stdio transport.
    #[serde(default)]
    pub command: String,
    /// Command arguments for stdio transport.
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional environment variables for stdio transport.
    #[serde(default)]
    #[secret]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub env: HashMap<String, String>,
    /// Optional HTTP headers for HTTP/SSE transports. Treated as secret:
    /// the values commonly carry Bearer tokens for the upstream MCP server.
    #[serde(default)]
    #[secret]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub headers: HashMap<String, String>,
    /// Optional per-call timeout in seconds (hard capped in validation).
    #[serde(default)]
    pub tool_timeout_secs: Option<u64>,
    /// Resource URIs to read once at agent startup and inject into the system
    /// prompt as untrusted, server-origin context. Each is read via
    /// `resources/read` on this server; pins on a server that does not advertise
    /// resources, or that the agent's tool policy denies, are skipped with a
    /// warning. Read once per run (not refreshed; no subscriptions).
    #[serde(default)]
    pub pinned_resources: Vec<String>,
}

/// External MCP client configuration (`[mcp]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "mcp"]
pub struct McpConfig {
    /// Enable MCP tool loading.
    #[tab(Settings)]
    #[serde(default = "default_mcp_enabled")]
    pub enabled: bool,
    /// Load MCP tool schemas on-demand via `tool_search` instead of eagerly
    /// including them in the LLM context window. When enabled, only tool names
    /// are listed in the system prompt; the LLM must call `tool_search` to fetch
    /// full schemas before invoking a deferred tool.
    #[tab(Settings)]
    #[serde(default = "default_deferred_loading")]
    pub deferred_loading: bool,
    /// Configured MCP servers. The `#[nested]` annotation makes the macro
    /// expose this as a List section in `map_key_sections()`, so the
    /// dashboard's `+ Add MCP server` affordance and the `POST
    /// /api/config/map-key?path=mcp.servers&key=<name>` endpoint pick it
    /// up automatically (no hand-table on the gateway side).
    ///
    /// `#[natural_key = "name"]` opts the Vec into per-element property
    /// routing (see `route_vec_path` and the `Configurable` derive's
    /// `#[natural_key]` arm). With it, `set_prop("mcp.servers.<name>.url",
    /// ...)` and `get_prop("mcp.servers.<name>.transport")` resolve to
    /// the matching element's own `set_prop` / `get_prop`, and the
    /// `mcp.servers` section behaves like a `HashMap<String,
    /// McpServerConfig>` from the dashboard / TUI's point of view.
    /// `name` itself becomes read-only via `set_prop`; rename goes
    /// through `rename_map_key` which mutates the `name` field in place.
    #[tab(Servers)]
    #[serde(default, alias = "mcpServers")]
    #[nested]
    #[natural_key = "name"]
    pub servers: Vec<McpServerConfig>,
}

fn default_mcp_enabled() -> bool {
    true
}

fn default_deferred_loading() -> bool {
    false
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: default_mcp_enabled(),
            deferred_loading: default_deferred_loading(),
            servers: Vec::new(),
        }
    }
}

/// Verifiable Intent (VI) credential issuance and constraint checking
/// (`[verifiable_intent]` section).
///
/// ZeroClaw implements issuance, crypto, types and constraint checking, but not
/// a credential chain verifier. Until one exists the `vi_verify` tool is
/// withheld from the model-visible registry, so neither key below enables
/// verification of a credential. The library paths are unaffected.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "verifiable_intent"]
pub struct VerifiableIntentConfig {
    /// Opt in to the VI section (default: false).
    ///
    /// While the tool is withheld this does not enable credential verification
    /// on commerce tool calls. It currently causes a warning naming that gap,
    /// emitted once per config application: at process startup, and again when
    /// a daemon reload re-reads config from disk.
    #[serde(default)]
    pub enabled: bool,

    /// Intended strictness mode for constraint evaluation.
    ///
    /// Accepts `"strict"` or `"permissive"`, and defaults to `"strict"`. No
    /// production code reads it while the tool is withheld.
    #[serde(default = "default_vi_strictness")]
    pub strictness: String,
}

fn default_vi_strictness() -> String {
    "strict".to_owned()
}

impl Default for VerifiableIntentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            strictness: default_vi_strictness(),
        }
    }
}

// ── Nodes (Dynamic Node Discovery) ───────────────────────────────

fn default_mdns_announce_interval_secs() -> u64 {
    30
}

fn default_mdns_peer_ttl_secs() -> u64 {
    90
}

fn default_mdns_max_peers() -> usize {
    16
}

/// Configuration for LAN-local mDNS peer discovery (`[nodes.mdns]`).
///
/// This config controls only discovery behavior. The advertised gateway
/// endpoint is derived from the running gateway's actual host, port, and path
/// prefix at startup so `[nodes.mdns]` does not duplicate gateway listen state.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "nodes.mdns"]
pub struct MdnsConfig {
    /// Enable mDNS local peer discovery.
    #[serde(default)]
    pub enabled: bool,
    /// Human-readable node name advertised to LAN peers. Defaults to a stable
    /// local fallback when unset.
    #[serde(default)]
    pub node_name: Option<String>,
    /// Maximum number of unauthenticated LAN peer hints retained in memory.
    #[serde(default = "default_mdns_max_peers")]
    pub max_peers: usize,
    /// How often this node re-broadcasts its presence, in seconds.
    #[serde(default = "default_mdns_announce_interval_secs")]
    pub announce_interval_secs: u64,
    /// Seconds after the last announcement before a peer is evicted.
    #[serde(default = "default_mdns_peer_ttl_secs")]
    pub peer_ttl_secs: u64,
}

impl Default for MdnsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            node_name: None,
            max_peers: default_mdns_max_peers(),
            announce_interval_secs: default_mdns_announce_interval_secs(),
            peer_ttl_secs: default_mdns_peer_ttl_secs(),
        }
    }
}

/// Configuration for the dynamic node discovery system (`[nodes]`).
///
/// When enabled, external processes/devices can connect via WebSocket
/// at `/ws/nodes` and advertise their capabilities at runtime.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "nodes"]
pub struct NodesConfig {
    /// Enable dynamic node discovery endpoint.
    #[serde(default)]
    pub enabled: bool,
    /// Maximum number of concurrent node connections.
    #[serde(default = "default_max_nodes")]
    pub max_nodes: usize,
    /// Optional bearer token for node authentication.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub auth_token: Option<String>,
    /// LAN-local mDNS peer discovery.
    #[serde(default)]
    #[nested]
    pub mdns: MdnsConfig,
}

fn default_max_nodes() -> usize {
    16
}

impl Default for NodesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_nodes: default_max_nodes(),
            auth_token: None,
            mdns: MdnsConfig::default(),
        }
    }
}

// ── TTS (Text-to-Speech) ─────────────────────────────────────────

fn default_tts_voice() -> String {
    "alloy".into()
}

fn default_tts_format() -> String {
    "mp3".into()
}

fn default_tts_max_text_length() -> usize {
    4096
}

/// Text-to-Speech subsystem configuration (`[tts]`).
///
/// Per-instance TTS configs live under `[tts_providers.<type>.<alias>]`
/// (parallel to `providers.models`). What remains here are the global
/// runtime knobs that apply to every model_provider invocation.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "tts"]
pub struct TtsConfig {
    /// Enable TTS synthesis.
    #[serde(default)]
    pub enabled: bool,
    /// Default voice ID passed to the selected tts provider.
    #[serde(default = "default_tts_voice")]
    pub default_voice: String,
    /// Default audio output format (`"mp3"`, `"opus"`, `"wav"`).
    #[serde(default = "default_tts_format")]
    pub default_format: String,
    /// Maximum input text length in characters (default 4096).
    #[serde(default = "default_tts_max_text_length")]
    pub max_text_length: usize,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_voice: default_tts_voice(),
            default_format: default_tts_format(),
            max_text_length: default_tts_max_text_length(),
        }
    }
}

/// Per-instance TTS model_provider configuration (`[tts_providers.<type>.<alias>]`).
///
/// Mirrors `ModelProviderConfig` in shape — one struct holds the union of
/// fields across backends. Only the fields relevant to the selected backend
/// (determined by the outer `<type>` map key) are read at runtime; others
/// are quietly ignored.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "tts_provider"]
#[serde(default)]
pub struct TtsProviderConfig {
    /// API key (openai, elevenlabs, google).
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_key: Option<String>,
    /// Model name. OpenAI uses this for `tts-1`/`tts-1-hd`; elevenlabs uses
    /// it as the model_id (e.g. `eleven_monolingual_v1`).
    pub model: Option<String>,
    /// Voice override for this instance. When empty, falls back to
    /// `[tts].default_voice`.
    pub voice: Option<String>,
    /// Playback speed multiplier (openai only; default `1.0`).
    pub speed: Option<f64>,
    /// Voice stability for elevenlabs (0.0-1.0; default `0.5`).
    pub stability: Option<f64>,
    /// Similarity boost for elevenlabs (0.0-1.0; default `0.5`).
    pub similarity_boost: Option<f64>,
    /// Language code for google (e.g. `en-US`).
    pub language_code: Option<String>,
    /// Path to backend binary (edge-tts subprocess; piper local server).
    pub binary_path: Option<String>,
    /// Audio response format sent to the TTS backend (e.g. `"opus"`, `"mp3"`,
    /// `"wav"`). Defaults to `"opus"` for the OpenAI family. Override to
    /// `"wav"` for Orpheus-class models (e.g. `canopylabs/orpheus-v1-english`
    /// on Groq) or `"mp3"` for broader compatibility.
    pub response_format: Option<String>,
    /// Endpoint URI for HTTP-based backends. Overrides the family default
    /// when pointing at a compatible third-party API (Groq, Azure, self-hosted
    /// proxies). Set to the **full** URL — there is no separate path-suffix
    /// field. Renamed from `api_url` for parity with `ModelProviderConfig.uri`.
    #[serde(alias = "api_url")]
    pub uri: Option<String>,
}

// ── TTS endpoint trait + per-family typed configs ──────────────────────────
//
// Mirrors the model provider typed-family pattern. Each TTS family carries
// its own typed config (composing TtsProviderConfig as the shared base via
// `#[serde(flatten)]`) and a single-variant `*TtsEndpoint` enum impl'ing
// `TtsEndpoint`. Edge and Piper skip the base — they're subprocess / local
// runtimes with no shared `api_key` / `voice` defaults.

/// One trait per family-endpoint enum. Returns the URI for the chosen
/// variant. Mirrors `ModelEndpoint` for parity across model and TTS.
pub trait TtsEndpoint {
    fn uri(&self) -> &'static str;
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OpenAITtsEndpoint {
    #[default]
    Default,
}
impl TtsEndpoint for OpenAITtsEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.openai.com/v1/audio/speech",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.tts.openai"]
pub struct OpenAITtsProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: TtsProviderConfig,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ElevenLabsTtsEndpoint {
    #[default]
    Default,
}
impl TtsEndpoint for ElevenLabsTtsEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.elevenlabs.io/v1/text-to-speech",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.tts.elevenlabs"]
pub struct ElevenLabsTtsProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: TtsProviderConfig,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum GoogleTtsEndpoint {
    #[default]
    Default,
}
impl TtsEndpoint for GoogleTtsEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://texttospeech.googleapis.com/v1/text:synthesize",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.tts.google"]
pub struct GoogleTtsProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: TtsProviderConfig,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum EdgeTtsEndpoint {
    /// Subprocess — no remote endpoint. Sentinel for trait conformity.
    #[default]
    LocalSubprocess,
}
impl TtsEndpoint for EdgeTtsEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::LocalSubprocess => "subprocess://edge-tts",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.tts.edge"]
pub struct EdgeTtsProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: TtsProviderConfig,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PiperTtsEndpoint {
    #[default]
    LocalDefault,
}
impl TtsEndpoint for PiperTtsEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::LocalDefault => "http://127.0.0.1:5000/v1/audio/speech",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.tts.piper"]
pub struct PiperTtsProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: TtsProviderConfig,
}

// ── Transcription providers (typed-family split, mirrors models/tts) ────
//
// Six family slots: `groq`, `openai`, `deepgram`, `assemblyai`, `google`,
// `local_whisper`. Each is a `HashMap<String, *TranscriptionProviderConfig>`
// keyed by operator-chosen alias. The shared `TranscriptionProviderConfig`
// base carries `api_key` + `language` since every cloud STT family takes
// both; `local_whisper` skips the base because it's a self-hosted endpoint
// with its own auth token, not a vendor API key.

/// Shared base for cloud transcription providers. Each cloud family
/// composes this via `#[serde(flatten)] base: TranscriptionProviderConfig`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.transcription"]
pub struct TranscriptionProviderConfig {
    /// API key for the transcription provider.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_key: Option<String>,
    /// Optional language hint passed to the provider (ISO-639-1 like `"en"` /
    /// `"ru"`, or BCP-47 like `"en-US"` for Google). Most providers auto-detect
    /// when this is unset.
    #[serde(default)]
    pub language: Option<String>,
    /// Whisper-style initial prompt to bias the model toward expected
    /// vocabulary (proper nouns, technical terms). Provider-specific support;
    /// silently ignored where not applicable.
    #[serde(default)]
    pub initial_prompt: Option<String>,
}

/// Trait that every transcription endpoint enum implements. Mirrors
/// `ModelEndpoint` / `TtsEndpoint` for parity.
pub trait TranscriptionEndpoint {
    fn uri(&self) -> &'static str;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum GroqTranscriptionEndpoint {
    #[default]
    Default,
}
impl TranscriptionEndpoint for GroqTranscriptionEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.groq.com/openai/v1/audio/transcriptions",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.transcription.groq"]
pub struct GroqTranscriptionProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: TranscriptionProviderConfig,
    /// Whisper model name (default: `"whisper-large-v3-turbo"`).
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OpenAiTranscriptionEndpoint {
    #[default]
    Default,
}
impl TranscriptionEndpoint for OpenAiTranscriptionEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.openai.com/v1/audio/transcriptions",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.transcription.openai"]
pub struct OpenAiTranscriptionProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: TranscriptionProviderConfig,
    /// Whisper model name (default: `"whisper-1"`).
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DeepgramTranscriptionEndpoint {
    #[default]
    Default,
}
impl TranscriptionEndpoint for DeepgramTranscriptionEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.deepgram.com/v1/listen",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.transcription.deepgram"]
pub struct DeepgramTranscriptionProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: TranscriptionProviderConfig,
    /// Deepgram model name (default: `"nova-2"`).
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AssemblyAiTranscriptionEndpoint {
    #[default]
    Default,
}
impl TranscriptionEndpoint for AssemblyAiTranscriptionEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://api.assemblyai.com/v2/transcript",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.transcription.assemblyai"]
pub struct AssemblyAiTranscriptionProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: TranscriptionProviderConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum GoogleTranscriptionEndpoint {
    #[default]
    Default,
}
impl TranscriptionEndpoint for GoogleTranscriptionEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::Default => "https://speech.googleapis.com/v1/speech:recognize",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.transcription.google"]
pub struct GoogleTranscriptionProviderConfig {
    #[nested]
    #[serde(flatten)]
    pub base: TranscriptionProviderConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LocalWhisperTranscriptionEndpoint {
    /// Self-hosted endpoint — no remote URL. Sentinel for trait conformity.
    /// The actual URL lives on `LocalWhisperTranscriptionProviderConfig.uri`.
    #[default]
    SelfHosted,
}
impl TranscriptionEndpoint for LocalWhisperTranscriptionEndpoint {
    fn uri(&self) -> &'static str {
        match self {
            Self::SelfHosted => "self-hosted",
        }
    }
}

/// Local / self-hosted Whisper-compatible transcription endpoint. Skips the
/// shared `TranscriptionProviderConfig` base because it uses a bearer-token
/// scheme and a per-instance URL rather than a vendor API key.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "providers.transcription.local_whisper"]
pub struct LocalWhisperTranscriptionProviderConfig {
    /// Endpoint URL, e.g. `"http://10.10.0.1:8001/v1/transcribe"`.
    pub uri: String,
    /// Bearer token for endpoint authentication. Omit for unauthenticated
    /// local endpoints.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub bearer_token: Option<String>,
    /// Optional language hint (passed through to the local endpoint).
    #[serde(default)]
    pub language: Option<String>,
    /// Maximum audio file size in bytes accepted by this endpoint.
    /// Defaults to 25 MB to match the cloud cap; raise as needed.
    #[serde(default = "default_local_whisper_max_audio_bytes")]
    pub max_audio_bytes: usize,
    /// Request timeout in seconds.
    #[serde(default = "default_local_whisper_timeout_secs")]
    pub timeout_secs: u64,
}

// `#[derive(Default)]` would leave `max_audio_bytes = 0` and `timeout_secs = 0`
// (Rust's `usize`/`u64` defaults), even though those fields have serde defaults
// pointing at `default_local_whisper_max_audio_bytes` /
// `default_local_whisper_timeout_secs`. `#[serde(default = ...)]` only fires for
// *deserialization* — Rust's `Default::default()` bypasses it.
//
// Without this manual impl, the `Configurable` macro-generated
// `create_map_key(...)` path inserts `LocalWhisperTranscriptionProviderConfig::default()`
// for any newly scaffolded `[providers.transcription.local_whisper.<alias>]`
// entry. The `Configurable` macro exposes `HashMap<String, T>` sections through
// `map_key_sections()` / `create_map_key()`, and the generated create path
// inserts `<T as Default>::default()` for new map entries — leaving
// `max_audio_bytes = 0`, `timeout_secs = 0`. `LocalWhisperProvider::from_typed_config`
// bridges those fields into `LocalWhisperProvider::from_config`, which still
// rejects zero for both fields. Defaults here mirror `LocalWhisperConfig` so
// scaffolded entries load without operator intervention.
impl Default for LocalWhisperTranscriptionProviderConfig {
    fn default() -> Self {
        Self {
            uri: String::new(),
            bearer_token: None,
            language: None,
            max_audio_bytes: default_local_whisper_max_audio_bytes(),
            timeout_secs: default_local_whisper_timeout_secs(),
        }
    }
}

/// Determines when a `ToolFilterGroup` is active.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ToolFilterGroupMode {
    /// Tools in this group are always included in every turn.
    Always,
    /// Tools in this group are included only when the user message contains
    /// at least one of the configured `keywords` (case-insensitive substring match).
    #[default]
    Dynamic,
}

/// A named group of MCP tool patterns with an activation mode.
///
/// Each group lists glob patterns for MCP tool names and an optional set of
/// keywords that trigger inclusion in `dynamic` mode. MCP tools are named
/// `<server>__<tool>` (double-underscore separator, e.g. `filesystem__read_file`).
///
/// Classification is by origin, not name shape: only tools that actually came
/// from the MCP registry are subject to these groups. Built-in tools AND skill
/// tools — which share the `<x>__<y>` naming convention (`{skill}__{tool}`) —
/// always pass through and are never affected by `tool_filter_groups`.
///
/// Under `[mcp] deferred_loading = true`, `mode = "always"` entries are
/// pre-activated at assembly time so matching tools are live from the first
/// turn without a `tool_search` round-trip.
///
/// # Example
/// ```toml
/// [[runtime_profiles.default.tool_filter_groups]]
/// mode = "always"
/// tools = ["filesystem__*"]
/// keywords = []
///
/// [[runtime_profiles.default.tool_filter_groups]]
/// mode = "dynamic"
/// tools = ["browser__*"]
/// keywords = ["browse", "website", "url", "search"]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct ToolFilterGroup {
    /// Activation mode: `"always"` or `"dynamic"`.
    #[serde(default)]
    pub mode: ToolFilterGroupMode,
    /// Glob patterns matching MCP tool names (single `*` wildcard supported).
    #[serde(default)]
    pub tools: Vec<String>,
    /// Keywords that activate this group in `dynamic` mode (case-insensitive substring).
    /// Ignored when `mode = "always"`.
    #[serde(default)]
    pub keywords: Vec<String>,
}

/// OpenAI Whisper STT model_provider configuration (`[transcription.openai]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "transcription.openai"]
pub struct OpenAiSttConfig {
    /// OpenAI API key for Whisper transcription.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_key: Option<String>,
    /// Whisper model name (default: "whisper-1").
    #[serde(default = "default_openai_stt_model")]
    pub model: String,
}

/// Deepgram STT model_provider configuration (`[transcription.deepgram]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "transcription.deepgram"]
pub struct DeepgramSttConfig {
    /// Deepgram API key.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_key: Option<String>,
    /// Deepgram model name (default: "nova-2").
    #[serde(default = "default_deepgram_stt_model")]
    pub model: String,
}

/// AssemblyAI STT model_provider configuration (`[transcription.assemblyai]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "transcription.assemblyai"]
pub struct AssemblyAiSttConfig {
    /// AssemblyAI API key.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_key: Option<String>,
}

/// Google Cloud Speech-to-Text model_provider configuration (`[transcription.google]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "transcription.google"]
pub struct GoogleSttConfig {
    /// Google Cloud API key.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_key: Option<String>,
    /// BCP-47 language code (default: "en-US").
    #[serde(default = "default_google_stt_language_code")]
    pub language_code: String,
}

/// Local/self-hosted Whisper-compatible STT endpoint (`[transcription.local_whisper]`).
///
/// Configures a self-hosted STT endpoint. Can be on localhost, a private network host, or any reachable URL.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "transcription.local_whisper"]
pub struct LocalWhisperConfig {
    /// HTTP or HTTPS endpoint URL, e.g. `"http://10.10.0.1:8001/v1/transcribe"`.
    pub url: String,
    /// Bearer token for endpoint authentication.
    /// Omit for unauthenticated local endpoints.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub bearer_token: Option<String>,
    /// Maximum audio file size in bytes accepted by this endpoint.
    /// Defaults to 25 MB — matching the cloud API cap for a safe out-of-the-box
    /// experience. Self-hosted endpoints can accept much larger files; raise this
    /// as needed, but note that each transcription call clones the audio buffer
    /// into a multipart payload, so peak memory per request is ~2× this value.
    #[serde(default = "default_local_whisper_max_audio_bytes")]
    pub max_audio_bytes: usize,
    /// Request timeout in seconds. Defaults to 300 (large files on local GPU).
    #[serde(default = "default_local_whisper_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_local_whisper_max_audio_bytes() -> usize {
    25 * 1024 * 1024
}

fn default_local_whisper_timeout_secs() -> u64 {
    300
}

// `#[derive(Default)]` would leave `max_audio_bytes = 0` and `timeout_secs = 0`
// (Rust's `usize`/`u64` defaults), even though those fields have serde defaults
// pointing at the helpers above. `#[serde(default = ...)]` only fires for
// *deserialization* — Rust's `Default::default()` bypasses it. Without this
// manual impl, `Config::init_defaults` materializes
// `transcription.local_whisper = Some(LocalWhisperConfig { max_audio_bytes: 0,
// timeout_secs: 0, .. })`; `LocalWhisperProvider::from_config` then rejects it
// at load (`max_audio_bytes must be greater than zero`), the failure poisons
// the parent `[transcription]` block's deserialization, and the daemon logs
// `dropped_config: transcription` while running with `transcription.enabled =
// false` regardless of operator intent.
impl Default for LocalWhisperConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            bearer_token: None,
            max_audio_bytes: default_local_whisper_max_audio_bytes(),
            timeout_secs: default_local_whisper_timeout_secs(),
        }
    }
}

/// HMAC tool execution receipt configuration, per agent
/// (`[agents.<alias>.tool_receipts]`).
///
/// Receipts are short HMAC-SHA256 tags appended to tool results so the model
/// cannot claim it ran a tool that never actually executed. See
/// `docs/book/src/security/tool-receipts.md`.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "delegate_agent.tool_receipts"]
pub struct ToolReceiptsConfig {
    /// Generate HMAC receipts on every tool execution. Default: `false`.
    /// When false, the entire receipt subsystem is inert (no key, no
    /// generation, no append, no system-prompt addendum).
    #[serde(default)]
    pub enabled: bool,
    /// Append a trailing `Tool receipts:` block to user-visible replies so
    /// receipts are auditable from the channel surface, not just the
    /// internal history. Default: `false`.
    #[serde(default)]
    pub show_in_response: bool,
    /// Inject the receipt-echo instruction into the system prompt so the
    /// model carries receipts verbatim into its response. Default: `true`.
    /// No effect when `enabled = false`.
    #[serde(default = "default_inject_system_prompt")]
    pub inject_system_prompt: bool,
}

fn default_inject_system_prompt() -> bool {
    true
}

impl Default for ToolReceiptsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            show_in_response: false,
            inject_system_prompt: default_inject_system_prompt(),
        }
    }
}

fn default_max_tool_result_chars() -> usize {
    50_000
}

fn default_keep_tool_context_turns() -> usize {
    2
}

fn default_agent_tool_dispatcher() -> String {
    "auto".into()
}

fn default_max_system_prompt_chars() -> usize {
    0
}

// ── Pacing ────────────────────────────────────────────────────────

/// Pacing controls for slow/local LLM workloads (`[pacing]` section).
///
/// All fields are optional and default to values that preserve existing
/// behavior. When set, they extend — not replace — the existing timeout
/// and loop-detection subsystems.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "pacing"]
pub struct PacingConfig {
    /// Per-step timeout in seconds: the maximum time allowed for a single
    /// LLM inference turn, independent of the total message budget.
    /// `None` means no per-step timeout (existing behavior).
    #[serde(default)]
    pub step_timeout_secs: Option<u64>,

    /// Minimum elapsed seconds before loop detection activates.
    /// Tasks completing under this threshold get aggressive loop protection;
    /// longer-running tasks receive a grace period before the detector starts
    /// counting. `None` means loop detection is always active (existing behavior).
    #[serde(default)]
    pub loop_detection_min_elapsed_secs: Option<u64>,

    /// Tool names excluded from identical-output / alternating-pattern loop
    /// detection. Useful for browser workflows where `browser_screenshot`
    /// structurally resembles a loop even when making progress.
    #[serde(default)]
    pub loop_ignore_tools: Vec<String>,

    /// Override for the hardcoded timeout scaling cap (default: 4).
    /// The channel message timeout budget is computed as:
    ///   `message_timeout_secs * min(max_tool_iterations, message_timeout_scale_max)`
    /// Raising this value lets long multi-step tasks with slow local models
    /// receive a proportionally larger budget without inflating the base timeout.
    #[serde(default)]
    pub message_timeout_scale_max: Option<u64>,

    /// Enable pattern-based loop detection (exact repeat, ping-pong,
    /// no-progress). Defaults to `true`.
    #[serde(default = "default_loop_detection_enabled")]
    pub loop_detection_enabled: bool,

    /// Sliding window size for the pattern-based loop detector.
    /// Defaults to 20.
    #[serde(default = "default_loop_detection_window_size")]
    pub loop_detection_window_size: usize,

    /// Number of consecutive identical tool+args calls before the first
    /// escalation (Warning). Defaults to 3.
    #[serde(default = "default_loop_detection_max_repeats")]
    pub loop_detection_max_repeats: usize,
}

fn default_loop_detection_enabled() -> bool {
    true
}

fn default_loop_detection_window_size() -> usize {
    20
}

fn default_loop_detection_max_repeats() -> usize {
    3
}

impl Default for PacingConfig {
    fn default() -> Self {
        Self {
            step_timeout_secs: None,
            loop_detection_min_elapsed_secs: None,
            loop_ignore_tools: Vec::new(),
            message_timeout_scale_max: None,
            loop_detection_enabled: default_loop_detection_enabled(),
            loop_detection_window_size: default_loop_detection_window_size(),
            loop_detection_max_repeats: default_loop_detection_max_repeats(),
        }
    }
}

/// Skills loading configuration (`[skills]` section).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SkillsPromptInjectionMode {
    /// Inline full skill instructions and tool metadata into the system prompt.
    #[default]
    Full,
    /// Inline only compact skill metadata (name/description/location) and load details on demand.
    Compact,
}

/// An external, user-configured skill registry ZeroClaw can install from.
///
/// Reuses the same git-clone mechanism as the default `zeroclaw-skills`
/// registry. Install a skill from it with `registry:<name>/<skill>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ExternalRegistryKind {
    #[default]
    Git,
    Unsupported(String),
}

impl ExternalRegistryKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Git => "git",
            Self::Unsupported(kind) => kind.as_str(),
        }
    }
}

impl From<String> for ExternalRegistryKind {
    fn from(kind: String) -> Self {
        if kind == "git" {
            Self::Git
        } else {
            Self::Unsupported(kind)
        }
    }
}

impl From<ExternalRegistryKind> for String {
    fn from(kind: ExternalRegistryKind) -> Self {
        kind.as_str().to_string()
    }
}

impl std::fmt::Display for ExternalRegistryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ExternalRegistryKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ExternalRegistryKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(String::deserialize(deserializer)?.into())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct ExternalRegistry {
    /// Short alias used in `registry:<name>/<skill>` install specs.
    pub name: String,
    /// Git repository URL of the registry (must expose a top-level `skills/` dir).
    pub url: String,
    /// Registry protocol. Only `"git"` is supported today; other protocols
    /// are reserved for a future additive release.
    #[serde(default)]
    #[cfg_attr(feature = "schema-export", schemars(with = "String"))]
    pub kind: ExternalRegistryKind,
    /// Whether this registry is eligible for installs. Default: `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl ExternalRegistry {
    /// Returns true when `name` can be addressed by `registry:<name>/<skill>`.
    ///
    /// Keep this as the single registry-alias rule used by both config
    /// validation and runtime install-spec parsing. Lowercase aliases also
    /// avoid clone-directory collisions on case-insensitive filesystems.
    pub fn is_valid_name(name: &str) -> bool {
        !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    }
}

/// Skills loading configuration (`[skills]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Default, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "skills"]
pub struct SkillsConfig {
    /// Enable loading and syncing the community open-skills repository.
    /// Default: `false` (opt-in).
    #[serde(default)]
    pub open_skills_enabled: bool,
    /// Optional path to a local open-skills repository.
    /// If unset, defaults to `$HOME/open-skills` when enabled.
    #[serde(default)]
    pub open_skills_dir: Option<String>,
    /// Allow script-like files in skills (`.sh`, `.bash`, `.ps1`, shebang shell files).
    /// Default: `false` (secure by default).
    #[serde(default)]
    pub allow_scripts: bool,
    /// URL of the skills registry repository for bare-name installs.
    /// Default: `https://github.com/zeroclaw-labs/zeroclaw-skills`
    #[serde(default)]
    pub registry_url: Option<String>,
    /// Additional user-configured skill registries, installed via
    /// `registry:<name>/<skill>`. Each reuses the git-clone registry path and
    /// is cloned to its own `extra-registry-<name>/` workspace directory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_registries: Vec<ExternalRegistry>,
    /// Controls how skills are injected into the system prompt.
    /// `full` preserves legacy behavior. `compact` keeps context small and loads skills on demand.
    #[serde(default)]
    pub prompt_injection_mode: SkillsPromptInjectionMode,
    /// Autonomous skill creation from successful multi-step task executions.
    #[serde(default)]
    #[nested]
    pub skill_creation: SkillCreationConfig,
    /// Prompt-triggered install suggestions for missing skills.
    #[serde(default, alias = "install-suggestions")]
    #[nested]
    pub install_suggestions: SkillInstallSuggestionsConfig,
    /// Automatic skill self-improvement after successful skill usage.
    #[serde(default)]
    #[nested]
    pub skill_improvement: SkillImprovementConfig,
}

/// Autonomous skill creation configuration (`[skills.skill_creation]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "skills.skill_creation"]
#[serde(default)]
pub struct SkillCreationConfig {
    /// Enable automatic skill creation after successful multi-step tasks.
    /// Default: `false`.
    pub enabled: bool,
    /// Maximum number of auto-generated skills to keep.
    /// When exceeded, the oldest auto-generated skill is removed (LRU eviction).
    pub max_skills: usize,
    /// Embedding similarity threshold for deduplication.
    /// Skills with descriptions more similar than this value are skipped.
    pub similarity_threshold: f64,
    /// Synthesize a canonical `SKILL.md` from the execution trace via a
    /// bounded model-provider reflection call instead of the deterministic
    /// `SKILL.toml` generator. Requires `enabled = true`. Falls back to
    /// `SKILL.toml` whenever the reflection call or its output is invalid, so
    /// turning this on never leaves a skill un-created. This is distinct from
    /// the `[skills.skill_improvement]` background review fork, which patches
    /// existing skills after use rather than creating new ones from a trace.
    /// Default: `false`.
    #[serde(default = "default_reflection_enabled")]
    pub reflection_enabled: bool,
    /// Maximum characters of the final assistant answer fed into the
    /// reflection prompt. Bounds prompt size so a long answer cannot blow up
    /// the reflection request. Default: `2000`.
    #[serde(default = "default_max_final_answer_chars")]
    pub max_final_answer_chars: usize,
    /// Maximum characters of the rendered tool-call trace fed into the
    /// reflection prompt. Default: `4000`.
    #[serde(default = "default_max_tool_trace_chars")]
    pub max_tool_trace_chars: usize,
    /// Maximum characters of the task description fed into the reflection
    /// prompt. Default: `1000`.
    #[serde(default = "default_max_task_chars")]
    pub max_task_chars: usize,
}

fn default_reflection_enabled() -> bool {
    false
}

fn default_max_final_answer_chars() -> usize {
    2000
}

fn default_max_tool_trace_chars() -> usize {
    4000
}

fn default_max_task_chars() -> usize {
    1000
}

impl Default for SkillCreationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_skills: 500,
            similarity_threshold: 0.85,
            reflection_enabled: default_reflection_enabled(),
            max_final_answer_chars: default_max_final_answer_chars(),
            max_tool_trace_chars: default_max_tool_trace_chars(),
            max_task_chars: default_max_task_chars(),
        }
    }
}

/// Prompt-triggered skill install suggestions (`[skills.install_suggestions]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "skills.install_suggestions"]
#[serde(default)]
pub struct SkillInstallSuggestionsConfig {
    /// Enable suggestions for installable skills before normal agent turns.
    /// Default: `false`.
    pub enabled: bool,
}

/// Skill self-improvement configuration (`[skills.skill-improvement]` section).
///
/// Controls the post-turn background review fork that may patch, expand, or
/// archive skills based on what the conversation revealed. The fork runs in a
/// restricted toolset (only `skills_list`, `skill_view`, `skill_manage`) and
/// never touches the user-visible conversation.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "skills.skill_improvement"]
pub struct SkillImprovementConfig {
    /// Enable the background skill-review fork. Default: `false`.
    /// Opt-in: users must explicitly enable this to allow post-turn skill
    /// mutations.
    #[serde(default)]
    pub enabled: bool,
    /// Minimum interval (in seconds) between reviews for the same skill.
    /// Acts as a durable rate limit on patches to any one skill. Default: `3600`.
    #[serde(default = "default_skill_improvement_cooldown")]
    pub cooldown_secs: u64,
    /// Spawn a review fork once at least this many tool-call iterations have
    /// accumulated in the current run. `0` disables iteration-based triggering.
    /// Default: `10`.
    #[serde(default = "default_nudge_interval_iterations")]
    pub nudge_interval_iterations: u32,
    /// Maximum tool-call iterations the review fork itself is allowed to make.
    /// Caps the fork's LLM cost. Default: `8`.
    #[serde(default = "default_max_review_iterations")]
    pub max_review_iterations: u32,
}

fn default_skill_improvement_cooldown() -> u64 {
    3600
}

fn default_nudge_interval_iterations() -> u32 {
    10
}

fn default_max_review_iterations() -> u32 {
    8
}

impl Default for SkillImprovementConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cooldown_secs: 3600,
            nudge_interval_iterations: 10,
            max_review_iterations: 8,
        }
    }
}

/// Pipeline tool configuration (`[pipeline]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "pipeline"]
pub struct PipelineConfig {
    /// Enable the `execute_pipeline` meta-tool.
    /// Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Maximum number of steps allowed in a single pipeline invocation.
    /// Default: `20`.
    #[serde(default = "default_pipeline_max_steps")]
    pub max_steps: usize,
    /// Tools allowed in pipeline steps. Steps referencing tools not on this
    /// list are rejected before execution.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
}

fn default_pipeline_max_steps() -> usize {
    20
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_steps: 20,
            allowed_tools: Vec::new(),
        }
    }
}

/// Multimodal (image) handling configuration (`[multimodal]` section).
///
/// # Privacy and cost note
///
/// Tool results that print real local image paths (e.g. shell tools doing
/// `ls /pictures` or `find . -name '*.png'`) are canonicalized into
/// `[IMAGE:...]` markers and base64-inlined into the next provider request.
/// This means image bytes that previously stayed local will be uploaded to
/// the configured provider when surfaced by a tool.
///
/// `max_images` (and the `trim_old_images` LRU policy) bounds the per-request
/// image budget, but operators running shell-style tools over directories of
/// personal or sensitive images should be aware of the upload semantics. See
/// `docs/book/src/contributing/privacy.md` for the project's privacy stance.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "multimodal"]
pub struct MultimodalConfig {
    /// Maximum number of image attachments accepted per request.
    ///
    /// Caps the total number of `[IMAGE:...]` markers that survive into the
    /// provider request after multimodal preprocessing. Older images are
    /// dropped first when the cumulative count exceeds this limit. Acts as
    /// the upper bound on per-turn upload cost when tool outputs surface
    /// local image paths.
    #[serde(default = "default_multimodal_max_images")]
    pub max_images: usize,
    /// Maximum image payload size in MiB before base64 encoding.
    #[serde(default = "default_multimodal_max_image_size_mb")]
    pub max_image_size_mb: usize,
    /// Maximum age of images in conversation turns.
    ///
    /// When non-zero, images in user messages that are more than this many
    /// turns back from the end of history are stripped before the request is
    /// sent to the provider. This prevents a single screenshot from being
    /// re-encoded and re-uploaded on every subsequent turn indefinitely.
    /// Tool-result images are already managed by the stale-tool-result
    /// mechanism and are not affected by this setting.
    ///
    /// `0` (the default) disables age-based trimming entirely — images are
    /// only evicted by the `max_images` count cap.
    #[serde(default)]
    pub max_image_turns: usize,
    /// Allow fetching remote image URLs (http/https). Disabled by default.
    #[serde(default)]
    pub allow_remote_fetch: bool,
    /// ModelProvider name to use for vision/image messages (e.g. `"ollama"`).
    /// When set, messages containing `[IMAGE:]` markers are routed to this
    /// model_provider instead of the default text model_provider.
    #[serde(default)]
    pub vision_model_provider: Option<String>,
    /// Model to use when routing to the vision model_provider (e.g. `"llava:7b"`).
    /// Only used when `vision_model_provider` is set.
    #[serde(default)]
    pub vision_model: Option<String>,
}

fn default_multimodal_max_images() -> usize {
    4
}

fn default_multimodal_max_image_size_mb() -> usize {
    5
}

impl MultimodalConfig {
    /// Clamp configured values to safe runtime bounds.
    pub fn effective_limits(&self) -> (usize, usize) {
        let max_images = self.max_images.clamp(1, 16);
        let max_image_size_mb = self.max_image_size_mb.clamp(1, 20);
        (max_images, max_image_size_mb)
    }
}

impl Default for MultimodalConfig {
    fn default() -> Self {
        Self {
            max_images: default_multimodal_max_images(),
            max_image_size_mb: default_multimodal_max_image_size_mb(),
            max_image_turns: 0,
            allow_remote_fetch: false,
            vision_model_provider: None,
            vision_model: None,
        }
    }
}

// ── Media Pipeline ──────────────────────────────────────────────

/// Automatic media understanding pipeline configuration (`[media_pipeline]`).
///
/// When enabled, inbound channel messages with media attachments are
/// pre-processed before reaching the agent: audio is transcribed, images are
/// annotated, and videos are summarised.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "media_pipeline"]
pub struct MediaPipelineConfig {
    /// Master toggle for the media pipeline (default: false).
    #[serde(default)]
    pub enabled: bool,

    /// Transcribe audio attachments using the configured transcription model_provider.
    #[serde(default = "default_true")]
    pub transcribe_audio: bool,

    /// Add image descriptions when a vision-capable model is active.
    #[serde(default = "default_true")]
    pub describe_images: bool,

    /// Summarize video attachments (placeholder — requires external API).
    #[serde(default = "default_true")]
    pub summarize_video: bool,
}

impl Default for MediaPipelineConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            transcribe_audio: true,
            describe_images: true,
            summarize_video: true,
        }
    }
}

// ── Identity (AIEOS / OpenClaw format) ──────────────────────────

/// Identity format configuration (`[identity]` section).
///
/// Supports `"openclaw"` (default) or `"aieos"` identity documents.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "identity"]
pub struct IdentityConfig {
    /// Identity format: "openclaw" (default) or "aieos"
    #[serde(default = "default_identity_format")]
    pub format: String,
    /// Path to AIEOS JSON file (relative to workspace)
    #[serde(default)]
    pub aieos_path: Option<String>,
    /// Inline AIEOS JSON (alternative to file path)
    #[serde(default)]
    pub aieos_inline: Option<String>,
}

fn default_identity_format() -> String {
    "openclaw".into()
}

impl Default for IdentityConfig {
    fn default() -> Self {
        Self {
            format: default_identity_format(),
            aieos_path: None,
            aieos_inline: None,
        }
    }
}

// ── Cost tracking and budget enforcement ───────────────────────────

/// Cost tracking and budget enforcement configuration (`[cost]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "cost"]
pub struct CostConfig {
    /// Enable cost tracking (default: true)
    #[tab(Limits)]
    #[serde(default = "default_cost_enabled")]
    pub enabled: bool,

    /// Daily spending limit in USD (default: 10.00)
    #[tab(Limits)]
    #[serde(default = "default_daily_limit")]
    pub daily_limit_usd: f64,

    /// Monthly spending limit in USD (default: 100.00)
    #[tab(Limits)]
    #[serde(default = "default_monthly_limit")]
    pub monthly_limit_usd: f64,

    /// Warn when spending reaches this percentage of limit (default: 80)
    #[tab(Limits)]
    #[serde(default = "default_warn_percent")]
    pub warn_at_percent: u8,

    /// Allow requests to exceed budget with --override flag (default: false)
    #[tab(Limits)]
    #[serde(default)]
    pub allow_override: bool,

    /// Cost enforcement behavior when budget limits are approached or exceeded.
    #[tab(Limits)]
    #[serde(default)]
    #[nested]
    pub enforcement: CostEnforcementConfig,

    /// Stamp each recorded cost entry with the originating agent alias so
    /// `/api/cost?agent=<alias>` and CLI rollups can attribute spend to a
    /// specific agent. Disable on high-volume deployments if the extra
    /// HashMap aggregation shows up in profiles (default: true).
    #[tab(Limits)]
    #[serde(default = "default_track_per_agent")]
    pub track_per_agent: bool,

    /// Operator-managed rate sheet at `[cost.rates.*]`. Sections mirror
    /// the `[providers.*]` dotted-path exactly with the trailing `alias`
    /// segment replaced by the resource the rate applies to (model id,
    /// tool name, …). Layout:
    ///
    /// ```toml
    /// [cost.rates.providers.models.anthropic."claude-opus-4-7"]
    /// input_per_mtok = 15.0
    /// output_per_mtok = 75.0
    /// cached_input_per_mtok = 1.5
    ///
    /// [cost.rates.providers.tts.openai."tts-1-hd"]
    /// per_mchar = 30.0
    ///
    /// [cost.rates.providers.transcription.openai.whisper-1]
    /// per_minute = 0.006
    ///
    /// [cost.rates.tools.web_search]
    /// per_call = 0.005
    /// ```
    #[tab(Costs)]
    #[serde(default)]
    #[nested]
    pub rates: CostRatesConfig,
}

/// Configuration for cost enforcement behavior when budget limits are reached.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "cost.enforcement"]
pub struct CostEnforcementConfig {
    /// Enforcement mode: "warn", "block", or "route_down".
    #[serde(default = "default_cost_enforcement_mode")]
    pub mode: String,
    /// Model hint to route to when budget is exceeded (used with "route_down" mode).
    #[serde(default)]
    pub route_down_model: Option<String>,
    /// Reserve this percentage of budget for critical operations.
    #[serde(default = "default_reserve_percent")]
    pub reserve_percent: u8,
}

fn default_cost_enforcement_mode() -> String {
    "warn".to_string()
}

fn default_reserve_percent() -> u8 {
    10
}

impl Default for CostEnforcementConfig {
    fn default() -> Self {
        Self {
            mode: default_cost_enforcement_mode(),
            route_down_model: None,
            reserve_percent: default_reserve_percent(),
        }
    }
}

fn default_daily_limit() -> f64 {
    10.0
}

fn default_monthly_limit() -> f64 {
    100.0
}

fn default_warn_percent() -> u8 {
    80
}

fn default_cost_enabled() -> bool {
    true
}

fn default_track_per_agent() -> bool {
    true
}

/// `[cost.rates]` — top-level rate-sheet namespace. Mirrors the
/// `[providers.*]` shape so each subsection here points at the same
/// kind of resource its `[providers.*]` counterpart configures.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "cost.rates"]
pub struct CostRatesConfig {
    /// `[cost.rates.providers.*]` — rates for everything under
    /// `[providers.*]` (models, TTS, transcription, …).
    #[serde(default)]
    #[nested]
    pub providers: ProviderCostRates,

    /// `[cost.rates.tools.<name>]` — per-call rates for tools that
    /// hit paid APIs. Keyed by the tool's registered name.
    #[serde(default)]
    #[nested]
    #[resource_key]
    pub tools: std::collections::HashMap<String, ToolCostRates>,
}

impl CostRatesConfig {
    /// Lookup model token rates by `(provider_type, model)`. Dispatch
    /// lives on the typed wrapper — see [`crate::providers::ModelCostRatesByProvider`].
    #[must_use]
    pub fn model_rates(&self, provider_type: &str, model: &str) -> Option<&ModelCostRates> {
        self.providers.models.get(provider_type, model)
    }

    /// Lookup TTS rates by `(provider_type, voice)`.
    #[must_use]
    pub fn tts_rates(&self, provider_type: &str, voice: &str) -> Option<&TtsCostRates> {
        self.providers.tts.get(provider_type, voice)
    }

    /// Lookup transcription rates by `(provider_type, model)`.
    #[must_use]
    pub fn transcription_rates(
        &self,
        provider_type: &str,
        model: &str,
    ) -> Option<&TranscriptionCostRates> {
        self.providers.transcription.get(provider_type, model)
    }

    /// Lookup tool per-call rate by registered name.
    #[must_use]
    pub fn tool_rates(&self, tool_name: &str) -> Option<&ToolCostRates> {
        self.tools.get(tool_name)
    }
}

/// `[cost.rates.providers.*]` — provider-shaped rate sheets. Each field
/// here mirrors a corresponding field on `[providers.*]` with the
/// trailing alias segment replaced by the resource the rate prices.
/// The inner typed wrappers carry the per-provider-type slot layout
/// and own dispatch (their slot list is the single source of truth,
/// shared with their providers counterpart via the `for_each_*_provider_slot!`
/// macros in [`crate::providers`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "cost.rates.providers"]
pub struct ProviderCostRates {
    /// `[cost.rates.providers.models.<type>.<model>]`.
    #[serde(default)]
    #[nested]
    pub models: crate::providers::ModelCostRatesByProvider,
    /// `[cost.rates.providers.tts.<type>.<voice>]`.
    #[serde(default)]
    #[nested]
    pub tts: crate::providers::TtsCostRatesByProvider,
    /// `[cost.rates.providers.transcription.<type>.<model>]`.
    #[serde(default)]
    #[nested]
    pub transcription: crate::providers::TranscriptionCostRatesByProvider,
}

/// The provider families that carry `[cost.rates.providers.*]` rate sheets, as
/// the trailing segment shared by their `[providers.<category>]` section. One
/// entry per `#[nested]` field on `ProviderCostRates`; this list is the registry
/// the TUI and web surfaces resolve cost tabs from, never a per-surface literal.
pub const PROVIDER_COST_CATEGORIES: [&str; 3] = ["models", "tts", "transcription"];

/// Cost-rate category for a `providers.<category>` section key, or `None` for
/// sections that carry no rate sheets. Resolves against
/// [`PROVIDER_COST_CATEGORIES`] so a new rate-bearing family is added in one
/// place.
#[must_use]
pub fn cost_category_for_provider_section(section_key: &str) -> Option<&'static str> {
    let suffix = section_key.strip_prefix("providers.")?;
    PROVIDER_COST_CATEGORIES
        .into_iter()
        .find(|category| *category == suffix)
}

/// Token-cost rates for a single chat / completion model, in USD per
/// 1M tokens. Every field optional so partial sheets work without
/// ceremony (an operator who only knows the input rate can record it).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "cost.rates.providers.models"]
pub struct ModelCostRates {
    /// Input tokens (USD per 1M).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_per_mtok: Option<f64>,
    /// Output tokens (USD per 1M).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_per_mtok: Option<f64>,
    /// Cached input tokens (USD per 1M). Optional — leave unset on
    /// providers that don't charge separately for prompt cache hits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_per_mtok: Option<f64>,
}

/// Rates for a TTS model, in USD per 1M characters.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "cost.rates.providers.tts"]
pub struct TtsCostRates {
    /// Characters synthesised (USD per 1M).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_mchar: Option<f64>,
}

/// Rates for a transcription model, in USD per minute of audio.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "cost.rates.providers.transcription"]
pub struct TranscriptionCostRates {
    /// Audio transcribed (USD per minute).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_minute: Option<f64>,
}

/// Rates for a tool that hits a paid external API. Keyed in
/// `[cost.rates.tools.<name>]` by the tool's registered name.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "cost.rates.tools"]
pub struct ToolCostRates {
    /// Per-call cost (USD).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_call: Option<f64>,
}

impl Default for CostConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            daily_limit_usd: default_daily_limit(),
            monthly_limit_usd: default_monthly_limit(),
            warn_at_percent: default_warn_percent(),
            allow_override: false,
            enforcement: CostEnforcementConfig::default(),
            track_per_agent: default_track_per_agent(),
            rates: CostRatesConfig::default(),
        }
    }
}

// ── Peripherals (hardware: STM32, RPi GPIO, etc.) ────────────────────────

/// Peripheral board integration configuration (`[peripherals]` section).
///
/// Boards become agent tools when enabled.
#[derive(Debug, Clone, Serialize, Deserialize, Default, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "peripherals"]
pub struct PeripheralsConfig {
    /// Enable peripheral support (boards become agent tools)
    #[serde(default)]
    pub enabled: bool,
    /// Board configurations (nucleo-f401re, rpi-gpio, etc.)
    #[serde(default)]
    pub boards: Vec<PeripheralBoardConfig>,
    /// Path to datasheet docs (relative to workspace) for RAG retrieval.
    /// Place .md/.txt files named by board (e.g. nucleo-f401re.md, rpi-gpio.md).
    #[serde(default)]
    pub datasheet_dir: Option<String>,
}

/// Configuration for a single peripheral board (e.g. STM32, RPi GPIO).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct PeripheralBoardConfig {
    /// Board type: "nucleo-f401re", "rpi-gpio", "esp32", etc.
    pub board: String,
    /// Transport: "serial", "native", "websocket"
    #[serde(default = "default_peripheral_transport")]
    pub transport: String,
    /// Path for serial: "/dev/ttyACM0", "/dev/ttyUSB0"
    #[serde(default)]
    pub path: Option<String>,
    /// Baud rate for serial (default: 115200)
    #[serde(default = "default_peripheral_baud")]
    pub baud: u32,
}

fn default_peripheral_transport() -> String {
    "serial".into()
}

fn default_peripheral_baud() -> u32 {
    115_200
}

impl Default for PeripheralBoardConfig {
    fn default() -> Self {
        Self {
            board: String::new(),
            transport: default_peripheral_transport(),
            path: None,
            baud: default_peripheral_baud(),
        }
    }
}

// ── Gateway security ─────────────────────────────────────────────

/// Gateway server configuration (`[gateway]` section).
///
/// Controls the HTTP gateway for webhook and pairing endpoints.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "gateway"]
#[allow(clippy::struct_excessive_bools)]
pub struct GatewayConfig {
    /// Gateway port (default: 42617)
    #[serde(default = "default_gateway_port")]
    pub port: u16,
    /// Gateway host (default: 127.0.0.1)
    #[serde(default = "default_gateway_host")]
    pub host: String,
    /// Require pairing before accepting requests (default: true)
    #[serde(default = "default_true")]
    pub require_pairing: bool,
    /// Allow binding to non-localhost without a tunnel (default: false)
    #[serde(default)]
    pub allow_public_bind: bool,
    /// Allow authenticated remote callers to use admin endpoints that are
    /// otherwise localhost-only. Currently this gates `POST /admin/reload`.
    /// When false (default), those endpoints reject any non-loopback peer.
    /// When true, a non-loopback request is accepted only if it also passes
    /// pairing authentication — which requires `require_pairing = true`; with
    /// pairing off a remote caller cannot be authenticated and is rejected, so
    /// this flag never exposes an anonymous remote reload. `/admin/shutdown`
    /// and the pairing-code endpoints stay localhost-only regardless.
    /// (default: false)
    #[serde(default)]
    pub allow_remote_admin: bool,
    /// Paired bearer tokens (managed automatically, not user-edited)
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub paired_tokens: Vec<String>,

    /// Max `/pair` requests per minute per client key.
    #[serde(default = "default_pair_rate_limit")]
    pub pair_rate_limit_per_minute: u32,

    /// Max `/webhook` requests per minute per client key.
    #[serde(default = "default_webhook_rate_limit")]
    pub webhook_rate_limit_per_minute: u32,

    /// Trust proxy-forwarded client IP headers (`X-Forwarded-For`, `X-Real-IP`).
    /// Disabled by default; enable only behind a trusted reverse proxy.
    #[serde(default)]
    #[credential_class = "public_value"]
    pub trust_forwarded_headers: bool,

    /// Optional URL path prefix for reverse-proxy deployments.
    /// When set, all gateway routes are served under this prefix.
    /// Must start with `/` and must not end with `/`.
    #[serde(default)]
    pub path_prefix: Option<String>,

    /// Maximum distinct client keys tracked by gateway rate limiter maps.
    #[serde(default = "default_gateway_rate_limit_max_keys")]
    pub rate_limit_max_keys: usize,

    /// TTL for webhook idempotency keys.
    #[serde(default = "default_idempotency_ttl_secs")]
    pub idempotency_ttl_secs: u64,

    /// Maximum distinct idempotency keys retained in memory.
    #[serde(default = "default_gateway_idempotency_max_keys")]
    pub idempotency_max_keys: usize,

    /// Persist gateway WebSocket chat sessions to SQLite. Default: true.
    #[serde(default = "default_true")]
    pub session_persistence: bool,

    /// Auto-archive stale gateway sessions older than N hours. 0 = disabled. Default: 0.
    #[serde(default)]
    pub session_ttl_hours: u32,

    /// Path to the web dashboard `dist` directory. When set, the gateway
    /// serves the compiled frontend from the filesystem instead of requiring
    /// it to be embedded in the binary. Accepts absolute paths or paths
    /// relative to the working directory. When omitted the gateway runs in
    /// API-only mode (no web dashboard) unless auto-detection finds it.
    #[serde(default)]
    pub web_dist_dir: Option<String>,

    /// TLS configuration for the gateway server (`[gateway.tls]`).
    #[serde(default)]
    #[nested]
    pub tls: Option<GatewayTlsConfig>,

    /// HTTP request timeout (seconds) for gateway routes other than the
    /// long-running cron-trigger endpoint. Default: 30s.
    #[serde(default = "default_gateway_request_timeout_secs")]
    pub request_timeout_secs: u64,

    /// HTTP request timeout (seconds) for `POST /api/cron/{id}/run`, which
    /// runs jobs synchronously and routinely exceeds the 30s default.
    /// Default: 600s (10 minutes).
    #[serde(default = "default_gateway_long_running_request_timeout_secs")]
    pub long_running_request_timeout_secs: u64,

    /// Poll GitHub for newer releases and show an "update available" indicator
    /// on the dashboard version tag. Read-only; does not install anything.
    /// (default: true)
    #[serde(default = "default_true")]
    pub check_updates: bool,

    /// Allow triggering a self-upgrade (binary swap via `zeroclaw update`) from
    /// the dashboard. This is a remote-code-execution-adjacent surface: any
    /// authenticated dashboard user could replace the running binary. Keep off
    /// unless you trust every paired client. (default: false)
    #[serde(default)]
    pub allow_self_upgrade: bool,
}

fn default_gateway_port() -> u16 {
    42617
}

fn default_gateway_request_timeout_secs() -> u64 {
    30
}

fn default_gateway_long_running_request_timeout_secs() -> u64 {
    600
}

fn default_gateway_host() -> String {
    "127.0.0.1".into()
}

fn default_pair_rate_limit() -> u32 {
    10
}

fn default_webhook_rate_limit() -> u32 {
    60
}

fn default_idempotency_ttl_secs() -> u64 {
    300
}

fn default_gateway_rate_limit_max_keys() -> usize {
    10_000
}

fn default_gateway_idempotency_max_keys() -> usize {
    10_000
}

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

fn default_memory_backend() -> String {
    "sqlite".into()
}

fn default_tunnel_provider() -> String {
    "none".into()
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            port: default_gateway_port(),
            host: default_gateway_host(),
            require_pairing: true,
            allow_public_bind: false,
            allow_remote_admin: false,
            paired_tokens: Vec::new(),
            pair_rate_limit_per_minute: default_pair_rate_limit(),
            webhook_rate_limit_per_minute: default_webhook_rate_limit(),
            trust_forwarded_headers: false,
            path_prefix: None,
            rate_limit_max_keys: default_gateway_rate_limit_max_keys(),
            idempotency_ttl_secs: default_idempotency_ttl_secs(),
            idempotency_max_keys: default_gateway_idempotency_max_keys(),
            session_persistence: true,
            session_ttl_hours: 0,
            web_dist_dir: None,
            tls: None,
            request_timeout_secs: default_gateway_request_timeout_secs(),
            long_running_request_timeout_secs: default_gateway_long_running_request_timeout_secs(),
            check_updates: true,
            allow_self_upgrade: false,
        }
    }
}

/// TLS configuration for the gateway server (`[gateway.tls]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "gateway.tls"]
pub struct GatewayTlsConfig {
    /// Enable TLS for the gateway (default: false).
    #[serde(default)]
    pub enabled: bool,
    /// Path to the PEM-encoded server certificate file.
    pub cert_path: String,
    /// Path to the PEM-encoded server private key file.
    pub key_path: String,
    /// Client certificate authentication (mutual TLS) settings.
    #[serde(default)]
    #[nested]
    pub client_auth: Option<GatewayClientAuthConfig>,
}

/// Client certificate authentication (mTLS) configuration (`[gateway.tls.client_auth]`).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "gateway.tls.client_auth"]
pub struct GatewayClientAuthConfig {
    /// Enable client certificate verification (default: false).
    #[serde(default)]
    pub enabled: bool,
    /// Path to the PEM-encoded CA certificate used to verify client certs.
    #[serde(default)]
    pub ca_cert_path: String,
    /// Reject connections that do not present a valid client certificate (default: true).
    #[serde(default = "default_true")]
    pub require_client_cert: bool,
    /// Optional SHA-256 fingerprints for certificate pinning.
    /// When non-empty, only client certs matching one of these fingerprints are accepted.
    #[serde(default)]
    pub pinned_certs: Vec<String>,
}

impl Default for GatewayClientAuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ca_cert_path: String::new(),
            require_client_cert: default_true(),
            pinned_certs: Vec::new(),
        }
    }
}

/// WebSocket Secure (WSS) transport for remote TUI-to-daemon connections (`[wss]`).
///
/// When enabled, the daemon listens for TLS-encrypted WebSocket connections
/// on the configured bind address and port. TUI clients connect via
/// `--connect wss://host:port`.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "wss"]
pub struct WssConfig {
    /// Enable the WSS listener (default: false).
    #[serde(default)]
    pub enabled: bool,
    /// Bind address for the WSS listener (default: "0.0.0.0").
    #[serde(default = "default_wss_bind")]
    pub bind: String,
    /// Port for the WSS listener (default: 9781).
    #[serde(default = "default_wss_port")]
    pub port: u16,
    /// Path to the PEM-encoded server certificate file.
    #[serde(default)]
    pub cert_path: String,
    /// Path to the PEM-encoded server private key file.
    #[serde(default)]
    pub key_path: String,
}

impl Default for WssConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_wss_bind(),
            port: default_wss_port(),
            cert_path: String::new(),
            key_path: String::new(),
        }
    }
}

fn default_wss_bind() -> String {
    "0.0.0.0".into()
}

fn default_wss_port() -> u16 {
    9781
}

/// Secure transport configuration for inter-node communication (`[node_transport]`).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "node_transport"]
pub struct NodeTransportConfig {
    /// Enable the secure transport layer.
    #[serde(default = "default_node_transport_enabled")]
    pub enabled: bool,
    /// Shared secret for HMAC authentication between nodes.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub shared_secret: String,
    /// Maximum age of signed requests in seconds (replay protection).
    #[serde(default = "default_max_request_age")]
    pub max_request_age_secs: i64,
    /// Require HTTPS for all node communication.
    #[serde(default = "default_require_https")]
    pub require_https: bool,
    /// Allow specific node IPs/CIDRs.
    #[serde(default)]
    pub allowed_peers: Vec<String>,
    /// Path to TLS certificate file.
    #[serde(default)]
    pub tls_cert_path: Option<String>,
    /// Path to TLS private key file.
    #[serde(default)]
    pub tls_key_path: Option<String>,
    /// Require client certificates (mutual TLS).
    #[serde(default)]
    pub mutual_tls: bool,
    /// Maximum number of connections per peer.
    #[serde(default = "default_connection_pool_size")]
    pub connection_pool_size: usize,
}

fn default_node_transport_enabled() -> bool {
    true
}
fn default_max_request_age() -> i64 {
    300
}
fn default_require_https() -> bool {
    true
}
fn default_connection_pool_size() -> usize {
    4
}

impl Default for NodeTransportConfig {
    fn default() -> Self {
        Self {
            enabled: default_node_transport_enabled(),
            shared_secret: String::new(),
            max_request_age_secs: default_max_request_age(),
            require_https: default_require_https(),
            allowed_peers: Vec::new(),
            tls_cert_path: None,
            tls_key_path: None,
            mutual_tls: false,
            connection_pool_size: default_connection_pool_size(),
        }
    }
}

// ── Composio (managed tool surface) ─────────────────────────────

/// Composio managed OAuth tools integration (`[composio]` section).
///
/// Provides access to 1000+ OAuth-connected tools via the Composio platform.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "composio"]
pub struct ComposioConfig {
    /// Enable Composio integration for 1000+ OAuth tools
    #[serde(default, alias = "enable")]
    pub enabled: bool,
    /// Composio API key (stored encrypted when secrets.encrypt = true)
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_key: Option<String>,
    /// Default entity ID for multi-user setups
    #[serde(default = "default_entity_id")]
    pub entity_id: String,
}

fn default_entity_id() -> String {
    "default".into()
}

impl Default for ComposioConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: None,
            entity_id: default_entity_id(),
        }
    }
}

// ── Microsoft 365 (Graph API integration) ───────────────────────

/// Microsoft 365 integration via Microsoft Graph API (`[microsoft365]` section).
///
/// Provides access to Outlook mail, Teams messages, Calendar events,
/// OneDrive files, and SharePoint search.
#[derive(Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "ms365"]
pub struct Microsoft365Config {
    /// Enable Microsoft 365 integration
    #[serde(default, alias = "enable")]
    pub enabled: bool,
    /// Azure AD tenant ID
    #[serde(default)]
    pub tenant_id: Option<String>,
    /// Azure AD application (client) ID
    #[serde(default)]
    pub client_id: Option<String>,
    /// Azure AD client secret (stored encrypted when secrets.encrypt = true)
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub client_secret: Option<String>,
    /// Authentication flow: "client_credentials" or "device_code"
    #[serde(default = "default_ms365_auth_flow")]
    pub auth_flow: String,
    /// OAuth scopes to request
    #[serde(default = "default_ms365_scopes")]
    pub scopes: Vec<String>,
    /// Encrypt the token cache file on disk
    #[serde(default = "default_true")]
    pub token_cache_encrypted: bool,
    /// User principal name or "me" (for delegated flows)
    #[serde(default)]
    pub user_id: Option<String>,
}

fn default_ms365_auth_flow() -> String {
    "client_credentials".to_string()
}

fn default_ms365_scopes() -> Vec<String> {
    vec!["https://graph.microsoft.com/.default".to_string()]
}

impl std::fmt::Debug for Microsoft365Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Microsoft365Config")
            .field("enabled", &self.enabled)
            .field("tenant_id", &self.tenant_id)
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret.as_ref().map(|_| "***"))
            .field("auth_flow", &self.auth_flow)
            .field("scopes", &self.scopes)
            .field("token_cache_encrypted", &self.token_cache_encrypted)
            .field("user_id", &self.user_id)
            .finish()
    }
}

impl Default for Microsoft365Config {
    fn default() -> Self {
        Self {
            enabled: false,
            tenant_id: None,
            client_id: None,
            client_secret: None,
            auth_flow: default_ms365_auth_flow(),
            scopes: default_ms365_scopes(),
            token_cache_encrypted: true,
            user_id: None,
        }
    }
}

// ── Secrets (encrypted credential store) ────────────────────────

/// Secrets encryption configuration (`[secrets]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "secrets"]
pub struct SecretsConfig {
    /// Enable encryption for API keys and tokens at rest
    #[serde(default = "default_true")]
    #[credential_class = "public_value"]
    pub encrypt: bool,
}

impl Default for SecretsConfig {
    fn default() -> Self {
        Self { encrypt: true }
    }
}

// ── Browser (friendly-service browsing only) ───────────────────

/// Computer-use sidecar configuration (`[browser.computer_use]` section).
///
/// Delegates OS-level mouse, keyboard, and screenshot actions to a local sidecar.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "browser.computer_use"]
pub struct BrowserComputerUseConfig {
    /// Sidecar endpoint for computer-use actions (OS-level mouse/keyboard/screenshot)
    #[serde(default = "default_browser_computer_use_endpoint")]
    pub endpoint: String,
    /// Optional bearer token for computer-use sidecar
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_key: Option<String>,
    /// Per-action request timeout in milliseconds
    #[serde(default = "default_browser_computer_use_timeout_ms")]
    pub timeout_ms: u64,
    /// Allow remote/public endpoint for computer-use sidecar (default: false)
    #[serde(default)]
    pub allow_remote_endpoint: bool,
    /// Optional window title/process allowlist forwarded to sidecar policy
    #[serde(default)]
    pub window_allowlist: Vec<String>,
    /// Optional X-axis boundary for coordinate-based actions
    #[serde(default)]
    pub max_coordinate_x: Option<i64>,
    /// Optional Y-axis boundary for coordinate-based actions
    #[serde(default)]
    pub max_coordinate_y: Option<i64>,
}

fn default_browser_computer_use_endpoint() -> String {
    "http://127.0.0.1:8787/v1/actions".into()
}

fn default_browser_computer_use_timeout_ms() -> u64 {
    15_000
}

impl Default for BrowserComputerUseConfig {
    fn default() -> Self {
        Self {
            endpoint: default_browser_computer_use_endpoint(),
            api_key: None,
            timeout_ms: default_browser_computer_use_timeout_ms(),
            allow_remote_endpoint: false,
            window_allowlist: Vec::new(),
            max_coordinate_x: None,
            max_coordinate_y: None,
        }
    }
}

/// Browser automation configuration (`[browser]` section).
///
/// Controls the `browser_open` tool and browser automation backends.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "browser"]
#[integration(
    category = "ToolsAutomation",
    display_name = "Browser",
    description = "Chrome/Chromium control",
    status_field = "enabled"
)]
pub struct BrowserConfig {
    /// Enable `browser_open` tool (opens URLs in the system browser without scraping)
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Allowed domains for `browser_open` (exact or subdomain match)
    #[serde(default = "default_browser_allowed_domains")]
    pub allowed_domains: Vec<String>,
    /// Browser session name (for agent-browser automation)
    #[serde(default)]
    pub session_name: Option<String>,
    /// Browser automation backend: "agent_browser" | "rust_native" | "computer_use" | "auto"
    #[serde(default = "default_browser_backend")]
    pub backend: String,
    /// Show browser window for agent_browser backend. When unset, inherits AGENT_BROWSER_HEADED.
    #[serde(default)]
    pub headed: Option<bool>,
    /// Headless mode for rust-native backend
    #[serde(default = "default_true")]
    pub native_headless: bool,
    /// WebDriver endpoint URL for rust-native backend (e.g. `http://127.0.0.1:9515`)
    #[serde(default = "default_browser_webdriver_url")]
    pub native_webdriver_url: String,
    /// Optional Chrome/Chromium executable path for rust-native backend
    #[serde(default)]
    pub native_chrome_path: Option<String>,
    /// Computer-use sidecar configuration
    #[serde(default)]
    #[nested]
    pub computer_use: BrowserComputerUseConfig,
    /// Private/internal hosts allowed to bypass SSRF protection.
    /// Exact and subdomain matches are supported; `["*"]` permits **all** private/local
    /// hosts (RFC 1918, loopback, link-local, `.local`). Default: empty (deny).
    /// Listed hosts also bypass `allowed_domains`. Both the `browser` tool and
    /// `browser_open` accept `http://` for listed hosts — internal services
    /// frequently lack a public TLS cert.
    /// Warning: `["*"]` also reaches link-local addresses, including the cloud metadata
    /// endpoint (`169.254.169.254`) — list specific hosts unless you accept that exposure.
    #[serde(default)]
    pub allowed_private_hosts: Vec<String>,
}

fn default_browser_allowed_domains() -> Vec<String> {
    vec!["*".into()]
}

fn default_browser_backend() -> String {
    "agent_browser".into()
}

fn default_browser_webdriver_url() -> String {
    "http://127.0.0.1:9515".into()
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_domains: vec!["*".into()],
            session_name: None,
            backend: default_browser_backend(),
            headed: None,
            native_headless: default_true(),
            native_webdriver_url: default_browser_webdriver_url(),
            native_chrome_path: None,
            computer_use: BrowserComputerUseConfig::default(),
            allowed_private_hosts: vec![],
        }
    }
}

// ── HTTP request tool ───────────────────────────────────────────

/// HTTP request tool configuration (`[http_request]` section).
///
/// Domain filtering: `allowed_domains` controls which hosts are reachable (use `["*"]`
/// for all public hosts, which is the default). If `allowed_domains` is empty, all
/// requests are rejected.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "http_request"]
pub struct HttpRequestConfig {
    /// Enable `http_request` tool for API interactions
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Allowed domains for HTTP requests (exact or subdomain match)
    #[serde(default = "default_allowed_domains_star")]
    pub allowed_domains: Vec<String>,
    /// Maximum response size in bytes (default: 1MB, 0 = unlimited)
    #[serde(default = "default_http_max_response_size")]
    pub max_response_size: usize,
    /// Request timeout in seconds (default: 30)
    #[serde(default = "default_http_timeout_secs")]
    pub timeout_secs: u64,
    /// Allow requests to private/LAN hosts (RFC 1918, loopback, link-local, .local).
    /// Default: false (deny private hosts for SSRF protection).
    #[serde(default)]
    pub allow_private_hosts: bool,
    /// Private/internal hosts explicitly allowed to bypass SSRF protection.
    /// Exact and subdomain matches are supported; `*` permits all private/local hosts.
    #[serde(default)]
    pub allowed_private_hosts: Vec<String>,
    /// Named authorization secrets for `auth_secret` requests.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub secrets: HashMap<String, String>,
}

impl Default for HttpRequestConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_domains: vec!["*".into()],
            max_response_size: default_http_max_response_size(),
            timeout_secs: default_http_timeout_secs(),
            allow_private_hosts: false,
            allowed_private_hosts: vec![],
            secrets: HashMap::new(),
        }
    }
}

fn default_http_max_response_size() -> usize {
    1_000_000 // 1MB
}

fn default_http_timeout_secs() -> u64 {
    30
}

fn default_allowed_domains_star() -> Vec<String> {
    vec!["*".into()]
}

// ── Web fetch ────────────────────────────────────────────────────

/// Web fetch tool configuration (`[web_fetch]` section).
///
/// Fetches web pages and converts HTML to plain text for LLM consumption.
/// Domain filtering: `allowed_domains` controls which hosts are reachable (use `["*"]`
/// for all public hosts). `blocked_domains` takes priority over `allowed_domains`.
/// If `allowed_domains` is empty, all requests are rejected (deny-by-default).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "web_fetch"]
pub struct WebFetchConfig {
    /// Enable `web_fetch` tool for fetching web page content
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Allowed domains for web fetch (exact or subdomain match; `["*"]` = all public hosts)
    #[serde(default = "default_web_fetch_allowed_domains")]
    pub allowed_domains: Vec<String>,
    /// Blocked domains (exact or subdomain match; always takes priority over allowed_domains)
    #[serde(default)]
    pub blocked_domains: Vec<String>,
    /// Private/internal hosts allowed to bypass SSRF protection (e.g. `["192.168.1.10", "internal.local"]`).
    /// Exact and subdomain matches are supported. Listing a host skips the
    /// resolved-IP SSRF check for it; a *literal* private/local host (IP literal,
    /// localhost, .local) listed here also bypasses `allowed_domains`. The `*`
    /// wildcard permits any literal private/local host and lets a name already in
    /// `allowed_domains` resolve to a private IP, but it does NOT widen
    /// `allowed_domains` — a non-private host (matched explicitly or via `*`) still
    /// needs `allowed_domains`, so `*` cannot reach an arbitrary public host.
    #[serde(default)]
    pub allowed_private_hosts: Vec<String>,
    /// Maximum response size in bytes (default: 500KB, plain text is much smaller than raw HTML)
    #[serde(default = "default_web_fetch_max_response_size")]
    pub max_response_size: usize,
    /// Request timeout in seconds (default: 30)
    #[serde(default = "default_web_fetch_timeout_secs")]
    pub timeout_secs: u64,
    /// Firecrawl fallback configuration (`[web_fetch.firecrawl]`)
    #[serde(default)]
    #[nested]
    pub firecrawl: FirecrawlConfig,
}

/// Firecrawl fallback mode: scrape a single page or crawl linked pages.
#[derive(
    Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum FirecrawlMode {
    #[default]
    Scrape,
    /// Reserved for future multi-page crawl support. Accepted in config
    /// deserialization to avoid breaking existing files, but not yet
    /// implemented — `fetch_via_firecrawl` always uses the `/scrape` endpoint.
    Crawl,
}

/// Firecrawl fallback configuration for JS-heavy and bot-blocked sites.
///
/// When enabled, if the standard web fetch fails (HTTP error, empty body, or
/// body shorter than 100 characters suggesting a JS-only page), the tool
/// falls back to the Firecrawl API for stealth content extraction.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "web_fetch.firecrawl"]
pub struct FirecrawlConfig {
    /// Enable Firecrawl fallback
    #[serde(default)]
    pub enabled: bool,
    /// Environment variable name for the Firecrawl API key
    #[serde(default = "default_firecrawl_api_key_env")]
    #[credential_class = "legacy_env_path"]
    pub api_key_env: String,
    /// Firecrawl API base URL
    #[serde(default = "default_firecrawl_api_url")]
    pub api_url: String,
    /// Firecrawl extraction mode
    #[serde(default)]
    pub mode: FirecrawlMode,
}

fn default_firecrawl_api_key_env() -> String {
    "FIRECRAWL_API_KEY".into()
}

fn default_firecrawl_api_url() -> String {
    "https://api.firecrawl.dev/v1".into()
}

impl Default for FirecrawlConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key_env: default_firecrawl_api_key_env(),
            api_url: default_firecrawl_api_url(),
            mode: FirecrawlMode::default(),
        }
    }
}

fn default_web_fetch_max_response_size() -> usize {
    500_000 // 500KB
}

fn default_web_fetch_timeout_secs() -> u64 {
    30
}

fn default_web_fetch_allowed_domains() -> Vec<String> {
    vec!["*".into()]
}

impl Default for WebFetchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_domains: vec!["*".into()],
            blocked_domains: vec![],
            allowed_private_hosts: vec![],
            max_response_size: default_web_fetch_max_response_size(),
            timeout_secs: default_web_fetch_timeout_secs(),
            firecrawl: FirecrawlConfig::default(),
        }
    }
}

// ── Link enricher ─────────────────────────────────────────────────

/// Automatic link understanding for inbound channel messages (`[link_enricher]`).
///
/// When enabled, URLs in incoming messages are automatically fetched and
/// summarised. The summary is prepended to the message before the agent
/// processes it, giving the LLM context about linked pages without an
/// explicit tool call.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "link_enricher"]
pub struct LinkEnricherConfig {
    /// Enable the link enricher pipeline stage (default: false)
    #[serde(default)]
    pub enabled: bool,
    /// Maximum number of links to fetch per message (default: 3)
    #[serde(default = "default_link_enricher_max_links")]
    pub max_links: usize,
    /// Per-link fetch timeout in seconds (default: 10)
    #[serde(default = "default_link_enricher_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_link_enricher_max_links() -> usize {
    3
}

fn default_link_enricher_timeout_secs() -> u64 {
    10
}

impl Default for LinkEnricherConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_links: default_link_enricher_max_links(),
            timeout_secs: default_link_enricher_timeout_secs(),
        }
    }
}

// ── Text browser ─────────────────────────────────────────────────

/// Text browser tool configuration (`[text_browser]` section).
///
/// Uses text-based browsers (lynx, links, w3m) to render web pages as plain
/// text. Designed for headless/SSH environments without graphical browsers.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "text_browser"]
pub struct TextBrowserConfig {
    /// Enable `text_browser` tool
    #[serde(default)]
    pub enabled: bool,
    /// Preferred text browser ("lynx", "links", or "w3m"). If unset, auto-detects.
    #[serde(default)]
    pub preferred_browser: Option<String>,
    /// Request timeout in seconds (default: 30)
    #[serde(default = "default_text_browser_timeout_secs")]
    pub timeout_secs: u64,
    /// Private/internal hosts allowed to bypass SSRF protection.
    /// Exact and subdomain matches are supported; `["*"]` permits **all** private/local
    /// hosts (RFC 1918, loopback, link-local, `.local`). Default: empty (deny).
    /// Warning: `["*"]` also reaches link-local addresses, including the cloud metadata
    /// endpoint (`169.254.169.254`) — list specific hosts unless you accept that exposure.
    #[serde(default)]
    pub allowed_private_hosts: Vec<String>,
}

fn default_text_browser_timeout_secs() -> u64 {
    30
}

impl Default for TextBrowserConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            preferred_browser: None,
            timeout_secs: default_text_browser_timeout_secs(),
            allowed_private_hosts: vec![],
        }
    }
}

// ── Shell tool ───────────────────────────────────────────────────

/// Shell tool configuration (`[shell_tool]` section).
///
/// Controls the behaviour of the `shell` execution tool. The main
/// tunable is `timeout_secs` — the maximum wall-clock time a single
/// shell command may run before it is killed.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "shell_tool"]
pub struct ShellToolConfig {
    /// Maximum shell command execution time in seconds (default: 60).
    #[serde(default = "default_shell_tool_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_shell_tool_timeout_secs() -> u64 {
    60
}

impl Default for ShellToolConfig {
    fn default() -> Self {
        Self {
            timeout_secs: default_shell_tool_timeout_secs(),
        }
    }
}

// ── Escalation routing ───────────────────────────────────────────

/// Escalation routing configuration (`[escalation]` section).
///
/// Controls which channels receive alert notifications when
/// `escalate_to_human` is called with high or critical urgency.
/// Channels are identified by name (e.g. `"telegram"`, `"slack"`).
/// Alerts are sent best-effort and do not block the escalation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "escalation"]
pub struct EscalationConfig {
    /// Channel names to alert on high/critical escalations (default: empty).
    ///
    /// Each name must match a configured channel. Unrecognised names are
    /// logged at WARN level and skipped.
    #[serde(default)]
    pub alert_channels: Vec<String>,
}

// ── Web search ───────────────────────────────────────────────────

/// Web search tool configuration (`[web_search]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "web_search"]
pub struct WebSearchConfig {
    /// Enable `web_search_tool` for web searches
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Search provider: "duckduckgo" (free), "brave" (requires API key), "tavily" (requires API key), "searxng" (self-hosted), "jina" (requires API key), or "bocha" (Bocha AI, requires API key — Chinese-friendly, <https://open.bochaai.com>)
    #[serde(default = "default_web_search_provider")]
    pub search_provider: String,
    /// Brave Search API key (required if search_provider is "brave")
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub brave_api_key: Option<String>,
    /// Tavily Search API key (required if search_provider is "tavily")
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub tavily_api_key: Option<String>,
    /// Jina AI API key (required if search_provider is "jina")
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub jina_api_key: Option<String>,
    /// Bocha AI Web Search API key (required if search_provider is `"bocha"`). Obtain at <https://open.bochaai.com>.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub bocha_api_key: Option<String>,
    /// SearXNG instance URL (required if search_provider is `"searxng"`), e.g. `"https://searx.example.com"`.
    #[serde(default)]
    pub searxng_instance_url: Option<String>,
    /// Maximum results per search (1-10)
    #[serde(default = "default_web_search_max_results")]
    pub max_results: usize,
    /// Request timeout in seconds
    #[serde(default = "default_web_search_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_web_search_provider() -> String {
    "duckduckgo".into()
}

fn default_web_search_max_results() -> usize {
    5
}

fn default_web_search_timeout_secs() -> u64 {
    15
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            search_provider: default_web_search_provider(),
            brave_api_key: None,
            tavily_api_key: None,
            jina_api_key: None,
            bocha_api_key: None,
            searxng_instance_url: None,
            max_results: default_web_search_max_results(),
            timeout_secs: default_web_search_timeout_secs(),
        }
    }
}

// ── Project Intelligence ────────────────────────────────────────

/// Project delivery intelligence configuration (`[project_intel]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "project_intel"]
pub struct ProjectIntelConfig {
    /// Enable the project_intel tool. Default: false.
    #[serde(default)]
    pub enabled: bool,
    /// Default report language (en, de, fr, it). Default: "en".
    #[serde(default = "default_project_intel_language")]
    pub default_language: String,
    /// Output directory for generated reports.
    #[serde(default = "default_project_intel_report_dir")]
    pub report_output_dir: String,
    /// Optional custom templates directory.
    #[serde(default)]
    pub templates_dir: Option<String>,
    /// Risk detection sensitivity: low, medium, high. Default: "medium".
    #[serde(default = "default_project_intel_risk_sensitivity")]
    pub risk_sensitivity: String,
    /// Include git log data in reports. Default: true.
    #[serde(default = "default_true")]
    pub include_git_data: bool,
    /// Include Jira data in reports. Default: false.
    #[serde(default)]
    pub include_jira_data: bool,
    /// Jira instance base URL (required if include_jira_data is true).
    #[serde(default)]
    pub jira_base_url: Option<String>,
}

fn default_project_intel_language() -> String {
    "en".into()
}

fn default_project_intel_report_dir() -> String {
    default_path_under_config_dir("project-reports")
}

fn default_project_intel_risk_sensitivity() -> String {
    "medium".into()
}

impl Default for ProjectIntelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_language: default_project_intel_language(),
            report_output_dir: default_project_intel_report_dir(),
            templates_dir: None,
            risk_sensitivity: default_project_intel_risk_sensitivity(),
            include_git_data: true,
            include_jira_data: false,
            jira_base_url: None,
        }
    }
}

// ── Backup ──────────────────────────────────────────────────────

/// Backup tool configuration (`[backup]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "backup"]
pub struct BackupConfig {
    /// Kept for config compatibility. This no longer registers a
    /// model-callable tool: backup create/list/verify/restore live on the
    /// operator-only gateway API (`/api/agents/{alias}/backup*`), whose
    /// authorization is the operator bearer gate — this flag does not
    /// gate that API. The section's parameters (`max_keep`,
    /// `include_dirs`) configure it; enabling the flag never widens the
    /// model-visible tool registry.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Maximum number of backups to keep (oldest are pruned).
    #[serde(default = "default_backup_max_keep")]
    pub max_keep: usize,
    /// Workspace subdirectories to include in backups.
    #[serde(default = "default_backup_include_dirs")]
    pub include_dirs: Vec<String>,
    /// Output directory for backup archives (relative to workspace root).
    #[serde(default = "default_backup_destination_dir")]
    pub destination_dir: String,
    /// Optional cron expression for scheduled automatic backups.
    #[serde(default)]
    pub schedule_cron: Option<String>,
    /// IANA timezone for `schedule_cron`.
    #[serde(default)]
    pub schedule_timezone: Option<String>,
    /// Compress backup archives.
    #[serde(default = "default_true")]
    pub compress: bool,
    /// Encrypt backup archives (requires a configured secret store key).
    #[serde(default)]
    pub encrypt: bool,
}

fn default_backup_max_keep() -> usize {
    10
}

fn default_backup_include_dirs() -> Vec<String> {
    vec![
        "config".into(),
        "memory".into(),
        "audit".into(),
        "knowledge".into(),
    ]
}

fn default_backup_destination_dir() -> String {
    "state/backups".into()
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_keep: default_backup_max_keep(),
            include_dirs: default_backup_include_dirs(),
            destination_dir: default_backup_destination_dir(),
            schedule_cron: None,
            schedule_timezone: None,
            compress: true,
            encrypt: false,
        }
    }
}

// ── Data Retention ──────────────────────────────────────────────

/// Data retention and purge configuration (`[data_retention]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "data_retention"]
pub struct DataRetentionConfig {
    /// Kept for config compatibility. This no longer registers a
    /// model-callable tool: retention status/stats/purge live on the
    /// operator-only gateway API
    /// (`/api/agents/{alias}/data-retention*`), whose authorization is
    /// the operator bearer gate — this flag does not gate that API. The
    /// section's parameters (`retention_days`) configure it; enabling
    /// the flag never widens the model-visible tool registry.
    #[serde(default)]
    pub enabled: bool,
    /// Days of data to retain before purge eligibility.
    #[serde(default = "default_retention_days")]
    pub retention_days: u64,
    /// Preview what would be deleted without actually removing anything.
    #[serde(default)]
    pub dry_run: bool,
    /// Limit retention enforcement to specific data categories (empty = all).
    #[serde(default)]
    pub categories: Vec<String>,
}

fn default_retention_days() -> u64 {
    90
}

impl Default for DataRetentionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            retention_days: default_retention_days(),
            dry_run: false,
            categories: Vec::new(),
        }
    }
}

// ── Google Workspace ─────────────────────────────────────────────

/// Built-in default service allowlist for the `google_workspace` tool.
///
/// Applied when `allowed_services` is empty. Defined here (not in the tool layer)
/// so that config validation can cross-check `allowed_operations` entries against
/// the effective service set in all cases, including when the operator relies on
/// the default.
pub const DEFAULT_GWS_SERVICES: &[&str] = &[
    "drive",
    "sheets",
    "gmail",
    "calendar",
    "docs",
    "slides",
    "tasks",
    "people",
    "chat",
    "classroom",
    "forms",
    "keep",
    "meet",
    "events",
];

/// Google Workspace CLI (`gws`) tool configuration (`[google_workspace]` section).
///
/// ## Defaults
/// - `enabled`: `false` (tool is not registered unless explicitly opted-in).
/// - `allowed_services`: empty vector, which grants access to the full default
///   service set: `drive`, `sheets`, `gmail`, `calendar`, `docs`, `slides`,
///   `tasks`, `people`, `chat`, `classroom`, `forms`, `keep`, `meet`, `events`.
/// - `credentials_path`: `None` (uses default `gws` credential discovery).
/// - `default_account`: `None` (uses the `gws` active account).
/// - `rate_limit_per_minute`: `60`.
/// - `timeout_secs`: `30`.
/// - `audit_log`: `false`.
/// - `credentials_path`: `None` (uses default `gws` credential discovery).
/// - `default_account`: `None` (uses the `gws` active account).
/// - `rate_limit_per_minute`: `60`.
/// - `timeout_secs`: `30`.
/// - `audit_log`: `false`.
///
/// ## Compatibility
/// Configs that omit the `[google_workspace]` section entirely are treated as
/// `GoogleWorkspaceConfig::default()` (disabled, all defaults allowed). Adding
/// the section is purely opt-in and does not affect other config sections.
///
/// ## Rollback / Migration
/// To revert, remove the `[google_workspace]` section from the config file (or
/// set `enabled = false`). No data migration is required; the tool simply stops
/// being registered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct GoogleWorkspaceAllowedOperation {
    /// Google Workspace service ID (for example `gmail` or `drive`).
    pub service: String,
    /// Top-level resource name for the service (for example `users` for Gmail or `files` for Drive).
    pub resource: String,
    /// Optional sub-resource for 4-segment gws commands
    /// (for example `messages` or `drafts` under `gmail users`).
    /// When present, the entry only matches calls that include this exact sub_resource.
    /// When absent, the entry only matches calls with no sub_resource.
    #[serde(default)]
    pub sub_resource: Option<String>,
    /// Allowed methods for the service/resource/sub_resource combination.
    #[serde(default)]
    pub methods: Vec<String>,
}

/// Google Workspace CLI (`gws`) tool configuration (`[google_workspace]` section).
///
/// ## Defaults
/// - `enabled`: `false` (tool is not registered unless explicitly opted-in).
/// - `allowed_services`: empty vector, which grants access to the full default
///   service set: `drive`, `sheets`, `gmail`, `calendar`, `docs`, `slides`,
///   `tasks`, `people`, `chat`, `classroom`, `forms`, `keep`, `meet`, `events`.
/// - `allowed_operations`: empty vector, which preserves the legacy behavior of
///   allowing any resource/method under the allowed service set.
/// - `credentials_path`: `None` (uses default `gws` credential discovery).
/// - `default_account`: `None` (uses the `gws` active account).
/// - `rate_limit_per_minute`: `60`.
/// - `timeout_secs`: `30`.
/// - `audit_log`: `false`.
///
/// ## Compatibility
/// Configs that omit the `[google_workspace]` section entirely are treated as
/// `GoogleWorkspaceConfig::default()` (disabled, all defaults allowed). Adding
/// the section is purely opt-in and does not affect other config sections.
///
/// ## Rollback / Migration
/// To revert, remove the `[google_workspace]` section from the config file (or
/// set `enabled = false`). No data migration is required; the tool simply stops
/// being registered.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "google_workspace"]
#[integration(
    category = "ToolsAutomation",
    display_name = "Google Workspace",
    description = "Drive, Gmail, Calendar, Sheets, Docs via gws CLI",
    status_field = "enabled"
)]
pub struct GoogleWorkspaceConfig {
    /// Enable the `google_workspace` tool. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Restrict which Google Workspace services the agent can access.
    ///
    /// When empty (the default), the full default service set is allowed (see
    /// struct-level docs). When non-empty, only the listed service IDs are
    /// permitted. Each entry must be non-empty, lowercase alphanumeric with
    /// optional underscores/hyphens, and unique.
    #[serde(default)]
    pub allowed_services: Vec<String>,
    /// Restrict which resource/method combinations the agent can access.
    ///
    /// When empty (the default), all methods under `allowed_services` remain
    /// available for backward compatibility. When non-empty, the runtime denies
    /// any `(service, resource, sub_resource, method)` combination that is not
    /// explicitly listed. `sub_resource` is optional per entry: an entry without
    /// it matches only 3-segment `gws` calls; an entry with it matches only calls
    /// that supply that exact sub_resource value.
    ///
    /// Each entry's `service` must appear in `allowed_services` when that list is
    /// non-empty; config validation rejects entries that would never match at
    /// runtime.
    #[serde(default)]
    pub allowed_operations: Vec<GoogleWorkspaceAllowedOperation>,
    /// Path to service account JSON or OAuth client credentials file.
    ///
    /// When `None`, the tool relies on the default `gws` credential discovery
    /// (`gws auth login`). Set this to point at a service-account key or an
    /// OAuth client-secrets JSON for headless / CI environments.
    #[serde(default)]
    pub credentials_path: Option<String>,
    /// Default Google account email to pass to `gws --account`.
    ///
    /// When `None`, the currently active `gws` account is used.
    #[serde(default)]
    pub default_account: Option<String>,
    /// Maximum number of `gws` API calls allowed per minute. Default: `60`.
    #[serde(default = "default_gws_rate_limit")]
    pub rate_limit_per_minute: u32,
    /// Command execution timeout in seconds. Default: `30`.
    #[serde(default = "default_gws_timeout_secs")]
    pub timeout_secs: u64,
    /// Enable audit logging of every `gws` invocation (service, resource,
    /// method, timestamp). Default: `false`.
    #[serde(default)]
    pub audit_log: bool,
}

fn default_gws_rate_limit() -> u32 {
    60
}

fn default_gws_timeout_secs() -> u64 {
    30
}

impl Default for GoogleWorkspaceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allowed_services: Vec::new(),
            allowed_operations: Vec::new(),
            credentials_path: None,
            default_account: None,
            rate_limit_per_minute: default_gws_rate_limit(),
            timeout_secs: default_gws_timeout_secs(),
            audit_log: false,
        }
    }
}

// ── Knowledge ───────────────────────────────────────────────────

/// Knowledge graph configuration for capturing and reusing expertise.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "knowledge"]
pub struct KnowledgeConfig {
    /// Enable the knowledge graph tool. Default: false.
    #[serde(default)]
    pub enabled: bool,
    /// Path to the knowledge graph SQLite database.
    #[serde(default = "default_knowledge_db_path")]
    pub db_path: String,
    /// Maximum number of knowledge nodes. Default: 100000.
    #[serde(default = "default_knowledge_max_nodes")]
    pub max_nodes: usize,
    /// Automatically capture knowledge from conversations. Default: false.
    #[serde(default)]
    pub auto_capture: bool,
    /// Proactively suggest relevant knowledge on queries. Default: true.
    #[serde(default = "default_true")]
    pub suggest_on_query: bool,
}

fn default_knowledge_db_path() -> String {
    default_path_under_config_dir("knowledge.db")
}

fn default_knowledge_max_nodes() -> usize {
    100_000
}

impl Default for KnowledgeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            db_path: default_knowledge_db_path(),
            max_nodes: default_knowledge_max_nodes(),
            auto_capture: false,
            suggest_on_query: true,
        }
    }
}

// ── LinkedIn ────────────────────────────────────────────────────

/// LinkedIn integration configuration (`[linkedin]` section).
///
/// When enabled, the `linkedin` tool is registered in the agent tool surface.
/// Requires `LINKEDIN_*` credentials in the workspace `.env` file.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "linkedin"]
pub struct LinkedInConfig {
    /// Enable the LinkedIn tool.
    #[serde(default)]
    pub enabled: bool,

    /// LinkedIn REST API version header (YYYYMM format).
    #[serde(default = "default_linkedin_api_version")]
    pub api_version: String,

    /// Content strategy for automated posting.
    #[serde(default)]
    #[nested]
    pub content: LinkedInContentConfig,

    /// Image generation for posts (`[linkedin.image]`).
    #[serde(default)]
    #[nested]
    pub image: LinkedInImageConfig,
}

impl Default for LinkedInConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_version: default_linkedin_api_version(),
            content: LinkedInContentConfig::default(),
            image: LinkedInImageConfig::default(),
        }
    }
}

fn default_linkedin_api_version() -> String {
    "202602".to_string()
}

/// Per-plugin config section keyed by plugin alias; values are secret so they
/// encrypt at rest under the same adjacent `.secret_key` as every other secret.
#[derive(Debug, Clone, Serialize, Deserialize, Default, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "plugins.entries"]
pub struct PluginEntryConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    #[secret]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub config: HashMap<String, String>,
}

/// Plugin system configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "plugins"]
pub struct PluginsConfig {
    /// Enable the plugin system (default: false)
    #[serde(default)]
    pub enabled: bool,
    /// Directory where plugins are stored
    #[serde(default = "default_plugins_dir")]
    pub plugins_dir: String,
    /// Auto-discover and load plugins on startup
    #[serde(default)]
    pub auto_discover: bool,
    /// Maximum number of plugins that can be loaded
    #[serde(default = "default_max_plugins")]
    pub max_plugins: usize,
    /// Plugin signature verification security settings
    #[serde(default)]
    #[nested]
    pub security: PluginSecurityConfig,
    /// Per-call WASM execution limits
    #[serde(default)]
    #[nested]
    pub limits: PluginLimitsConfig,
    #[serde(default)]
    #[nested]
    #[natural_key = "name"]
    pub entries: Vec<PluginEntryConfig>,
}

impl PluginsConfig {
    #[must_use]
    pub fn entry_config(&self, alias: &str) -> Option<&HashMap<String, String>> {
        self.entries
            .iter()
            .find(|e| e.name == alias)
            .map(|e| &e.config)
    }
}

impl PluginsConfig {
    /// Resolve `plugins_dir` to an absolute path, expanding a leading `~/`.
    ///
    /// This is the single source of truth for the plugin directory: the CLI,
    /// runtime tool/skill discovery, and the gateway all resolve through it so
    /// that `zeroclaw plugin install` and the agent agree on one location.
    /// Pure — performs no filesystem I/O.
    #[must_use]
    pub fn resolved_plugins_dir(&self) -> PathBuf {
        expand_tilde_path(&self.plugins_dir)
    }
}

/// Plugin signature verification configuration (`[plugins.security]`).
///
/// Controls Ed25519 signature verification for plugin manifests.
/// In `strict` mode, only plugins signed by a trusted publisher key are loaded.
/// In `permissive` mode, unsigned or untrusted plugins produce warnings but are
/// still loaded. In `disabled` mode (the default), no signature checking occurs.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "plugins.security"]
pub struct PluginSecurityConfig {
    /// Signature enforcement mode: "disabled", "permissive", or "strict".
    #[serde(default = "default_signature_mode")]
    pub signature_mode: String,
    /// Hex-encoded Ed25519 public keys of trusted plugin publishers.
    #[serde(default)]
    pub trusted_publisher_keys: Vec<String>,
}

fn default_signature_mode() -> String {
    "disabled".to_string()
}

impl Default for PluginSecurityConfig {
    fn default() -> Self {
        Self {
            signature_mode: default_signature_mode(),
            trusted_publisher_keys: Vec::new(),
        }
    }
}

/// Per-call WASM execution limits (`[plugins.limits]`).
///
/// Bounds a single plugin call so a runaway or malicious component traps
/// instead of hanging the host or exhausting memory. `call_fuel` caps
/// instructions per call; the memory, table, and instance ceilings bound a
/// store's growth. Every value is operator-tunable and validated as non-zero.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "plugins.limits"]
pub struct PluginLimitsConfig {
    /// Fuel budget per plugin call (wasmtime instruction units).
    #[serde(default = "default_plugin_call_fuel")]
    pub call_fuel: u64,
    /// Maximum linear memory a plugin store may grow to, in megabytes.
    #[serde(default = "default_plugin_max_memory_mb")]
    pub max_memory_mb: usize,
    /// Maximum table elements a plugin store may allocate.
    #[serde(default = "default_plugin_max_table_elements")]
    pub max_table_elements: usize,
    /// Maximum component instances a plugin store may create.
    #[serde(default = "default_plugin_max_instances")]
    pub max_instances: usize,
}

fn default_plugin_call_fuel() -> u64 {
    1_000_000_000
}

fn default_plugin_max_memory_mb() -> usize {
    256
}

fn default_plugin_max_table_elements() -> usize {
    100_000
}

fn default_plugin_max_instances() -> usize {
    64
}

impl Default for PluginLimitsConfig {
    fn default() -> Self {
        Self {
            call_fuel: default_plugin_call_fuel(),
            max_memory_mb: default_plugin_max_memory_mb(),
            max_table_elements: default_plugin_max_table_elements(),
            max_instances: default_plugin_max_instances(),
        }
    }
}

fn default_plugins_dir() -> String {
    default_path_under_config_dir("plugins")
}

fn default_max_plugins() -> usize {
    50
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            plugins_dir: default_plugins_dir(),
            auto_discover: false,
            max_plugins: default_max_plugins(),
            security: PluginSecurityConfig::default(),
            limits: PluginLimitsConfig::default(),
            entries: Vec::new(),
        }
    }
}

/// Content strategy configuration for LinkedIn auto-posting (`[linkedin.content]`).
///
/// The agent reads this via the `linkedin get_content_strategy` action to know
/// what feeds to check, which repos to highlight, and how to write posts.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "linkedin.content"]
pub struct LinkedInContentConfig {
    /// RSS feed URLs to monitor for topic inspiration (titles only).
    #[serde(default)]
    pub rss_feeds: Vec<String>,

    /// GitHub usernames whose public activity to reference.
    #[serde(default)]
    pub github_users: Vec<String>,

    /// GitHub repositories to highlight (format: `owner/repo`).
    #[serde(default)]
    pub github_repos: Vec<String>,

    /// Topics of expertise and interest for post themes.
    #[serde(default)]
    pub topics: Vec<String>,

    /// Professional persona description (name, role, expertise).
    #[serde(default)]
    pub persona: String,

    /// Freeform posting instructions for the AI agent.
    #[serde(default)]
    pub instructions: String,
}

/// Image generation configuration for LinkedIn posts (`[linkedin.image]`).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "linkedin.image"]
pub struct LinkedInImageConfig {
    /// Enable image generation for posts.
    #[serde(default)]
    pub enabled: bool,

    /// ModelProvider priority order. Tried in sequence; first success wins.
    #[serde(default = "default_image_providers")]
    pub providers: Vec<String>,

    /// Generate a branded SVG text card when all AI model_providers fail.
    #[serde(default = "default_true")]
    pub fallback_card: bool,

    /// Accent color for the fallback card (CSS hex).
    #[serde(default = "default_card_accent_color")]
    pub card_accent_color: String,

    /// Temp directory for generated images, relative to workspace.
    #[serde(default = "default_image_temp_dir")]
    pub temp_dir: String,

    /// Stability AI model_provider settings.
    #[serde(default)]
    #[nested]
    pub stability: ImageProviderStabilityConfig,

    /// Google Imagen (Vertex AI) model_provider settings.
    #[serde(default)]
    #[nested]
    pub imagen: ImageProviderImagenConfig,

    /// OpenAI DALL-E model_provider settings.
    #[serde(default)]
    #[nested]
    pub dalle: ImageProviderDalleConfig,

    /// Flux (fal.ai) model_provider settings.
    #[serde(default)]
    #[nested]
    pub flux: ImageProviderFluxConfig,
}

fn default_image_providers() -> Vec<String> {
    vec![
        "stability".into(),
        "imagen".into(),
        "dalle".into(),
        "flux".into(),
    ]
}

fn default_card_accent_color() -> String {
    "#0A66C2".into()
}

fn default_image_temp_dir() -> String {
    "linkedin/images".into()
}

impl Default for LinkedInImageConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            providers: default_image_providers(),
            fallback_card: true,
            card_accent_color: default_card_accent_color(),
            temp_dir: default_image_temp_dir(),
            stability: ImageProviderStabilityConfig::default(),
            imagen: ImageProviderImagenConfig::default(),
            dalle: ImageProviderDalleConfig::default(),
            flux: ImageProviderFluxConfig::default(),
        }
    }
}

/// Stability AI image generation settings (`[linkedin.image.stability]`).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "linkedin.image.stability"]
pub struct ImageProviderStabilityConfig {
    /// Environment variable name holding the API key.
    #[serde(default = "default_stability_api_key_env")]
    #[credential_class = "legacy_env_path"]
    pub api_key_env: String,
    /// Stability model identifier.
    #[serde(default = "default_stability_model")]
    pub model: String,
}

fn default_stability_api_key_env() -> String {
    "STABILITY_API_KEY".into()
}
fn default_stability_model() -> String {
    "stable-diffusion-xl-1024-v1-0".into()
}

impl Default for ImageProviderStabilityConfig {
    fn default() -> Self {
        Self {
            api_key_env: default_stability_api_key_env(),
            model: default_stability_model(),
        }
    }
}

/// Google Imagen (Vertex AI) settings (`[linkedin.image.imagen]`).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "linkedin.image.imagen"]
pub struct ImageProviderImagenConfig {
    /// Environment variable name holding the API key.
    #[serde(default = "default_imagen_api_key_env")]
    #[credential_class = "legacy_env_path"]
    pub api_key_env: String,
    /// Environment variable for the Google Cloud project ID.
    #[serde(default = "default_imagen_project_id_env")]
    #[credential_class = "legacy_env_path"]
    pub project_id_env: String,
    /// Vertex AI region.
    #[serde(default = "default_imagen_region")]
    pub region: String,
}

fn default_imagen_api_key_env() -> String {
    "GOOGLE_VERTEX_API_KEY".into()
}
fn default_imagen_project_id_env() -> String {
    "GOOGLE_CLOUD_PROJECT".into()
}
fn default_imagen_region() -> String {
    "us-central1".into()
}

impl Default for ImageProviderImagenConfig {
    fn default() -> Self {
        Self {
            api_key_env: default_imagen_api_key_env(),
            project_id_env: default_imagen_project_id_env(),
            region: default_imagen_region(),
        }
    }
}

/// OpenAI DALL-E settings (`[linkedin.image.dalle]`).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "linkedin.image.dalle"]
pub struct ImageProviderDalleConfig {
    /// Environment variable name holding the OpenAI API key.
    #[serde(default = "default_dalle_api_key_env")]
    #[credential_class = "legacy_env_path"]
    pub api_key_env: String,
    /// DALL-E model identifier.
    #[serde(default = "default_dalle_model")]
    pub model: String,
    /// Image dimensions.
    #[serde(default = "default_dalle_size")]
    pub size: String,
}

fn default_dalle_api_key_env() -> String {
    "OPENAI_API_KEY".into()
}
fn default_dalle_model() -> String {
    "dall-e-3".into()
}
fn default_dalle_size() -> String {
    "1024x1024".into()
}

impl Default for ImageProviderDalleConfig {
    fn default() -> Self {
        Self {
            api_key_env: default_dalle_api_key_env(),
            model: default_dalle_model(),
            size: default_dalle_size(),
        }
    }
}

/// Flux (fal.ai) image generation settings (`[linkedin.image.flux]`).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "linkedin.image.flux"]
pub struct ImageProviderFluxConfig {
    /// Environment variable name holding the fal.ai API key.
    #[serde(default = "default_flux_api_key_env")]
    #[credential_class = "legacy_env_path"]
    pub api_key_env: String,
    /// Flux model identifier.
    #[serde(default = "default_flux_model")]
    pub model: String,
}

fn default_flux_api_key_env() -> String {
    "FAL_API_KEY".into()
}
fn default_flux_model() -> String {
    "fal-ai/flux/schnell".into()
}

impl Default for ImageProviderFluxConfig {
    fn default() -> Self {
        Self {
            api_key_env: default_flux_api_key_env(),
            model: default_flux_model(),
        }
    }
}

// ── Standalone Image Generation ─────────────────────────────────

/// Standalone image generation tool configuration (`[image_gen]`).
///
/// When enabled, registers an `image_gen` tool that generates images via
/// fal.ai's synchronous API (Flux / Nano Banana models) and saves them
/// to the workspace `images/` directory.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "image_gen"]
pub struct ImageGenConfig {
    /// Enable the standalone image generation tool. Default: false.
    #[serde(default)]
    pub enabled: bool,

    /// Default fal.ai model identifier.
    #[serde(default = "default_image_gen_model")]
    pub default_model: String,

    /// Environment variable name holding the fal.ai API key.
    #[serde(default = "default_image_gen_api_key_env")]
    #[credential_class = "legacy_env_path"]
    pub api_key_env: String,
}

fn default_image_gen_model() -> String {
    "fal-ai/flux/schnell".into()
}

fn default_image_gen_api_key_env() -> String {
    "FAL_API_KEY".into()
}

impl Default for ImageGenConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_model: default_image_gen_model(),
            api_key_env: default_image_gen_api_key_env(),
        }
    }
}

// ── File Upload ─────────────────────────────────────────────────

/// Standalone file upload tool configuration (`[file_upload]`).
///
/// When `url` is set to a non-empty value, registers a `file_upload` tool that
/// POSTs files from the agent's local filesystem to the configured endpoint
/// using `multipart/form-data`. The LLM provides only a file path; the host
/// reads the bytes and uploads them without ever including file content in
/// the model context.
///
/// When `url` is `None` or empty, the tool is not registered.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "file_upload"]
pub struct FileUploadConfig {
    /// Upload endpoint URL. Tool is disabled when this is `None` or empty.
    #[serde(default)]
    pub url: Option<String>,

    /// HTTP method. Only `POST` (default) and `PUT` are accepted.
    #[serde(default = "default_file_upload_method")]
    pub method: String,

    /// Multipart form-field name for the file part. Default: `file`.
    #[serde(default = "default_file_upload_field_name")]
    pub field_name: String,

    /// Maximum file size in bytes. Larger files are rejected before any
    /// bytes hit the network. Default: 25 MiB.
    #[serde(default = "default_file_upload_max_size_bytes")]
    pub max_file_size_bytes: u64,

    /// Request timeout in seconds. Default: 60.
    #[serde(default = "default_file_upload_timeout_secs")]
    pub timeout_secs: u64,

    /// Static HTTP headers attached to every upload request. Same shape as
    /// `[mcp.servers.*.headers]`.
    #[serde(default)]
    #[secret]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub headers: HashMap<String, String>,
}

fn default_file_upload_method() -> String {
    "POST".into()
}

fn default_file_upload_field_name() -> String {
    "file".into()
}

fn default_file_upload_max_size_bytes() -> u64 {
    25 * 1024 * 1024
}

fn default_file_upload_timeout_secs() -> u64 {
    60
}

impl Default for FileUploadConfig {
    fn default() -> Self {
        Self {
            url: None,
            method: default_file_upload_method(),
            field_name: default_file_upload_field_name(),
            max_file_size_bytes: default_file_upload_max_size_bytes(),
            timeout_secs: default_file_upload_timeout_secs(),
            headers: HashMap::new(),
        }
    }
}

// ── File Upload Bundle ──────────────────────────────────────────

/// Standalone multi-file bundle upload tool configuration
/// (`[file_upload_bundle]`).
///
/// When `url` is set to a non-empty value, registers a `file_upload_bundle`
/// tool that POSTs N files from the agent's local filesystem to the
/// configured endpoint as a single `multipart/form-data` request. The LLM
/// provides only file paths; the host reads the bytes.
///
/// When `url` is `None` or empty, the tool is not registered.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "file-upload-bundle"]
pub struct FileUploadBundleConfig {
    /// Upload endpoint URL. Tool is disabled when this is `None` or empty.
    #[serde(default)]
    pub url: Option<String>,

    /// HTTP method. Only `POST` (default) and `PUT` are accepted.
    #[serde(default = "default_file_upload_bundle_method")]
    pub method: String,

    /// Multipart form-field name reused across every file part. Default: `file`.
    #[serde(default = "default_file_upload_bundle_field_name")]
    pub field_name: String,

    /// Maximum per-file size in bytes. Default: 10 MiB.
    #[serde(default = "default_file_upload_bundle_max_file_size_bytes")]
    pub max_file_size_bytes: u64,

    /// Maximum cumulative size across every file in one call. Default: 32 MiB.
    #[serde(default = "default_file_upload_bundle_max_total_size_bytes")]
    pub max_total_size_bytes: u64,

    /// Maximum number of files per call. Default: 16.
    #[serde(default = "default_file_upload_bundle_max_files")]
    pub max_files: u32,

    /// Request timeout in seconds. Default: 120.
    #[serde(default = "default_file_upload_bundle_timeout_secs")]
    pub timeout_secs: u64,

    /// Maximum response body bytes to read from the upload endpoint.
    /// Prevents unbounded memory use from a malicious or verbose receiver.
    /// Default: 4096 (4 KiB).
    #[serde(default = "default_file_upload_bundle_max_response_body_bytes")]
    pub max_response_body_bytes: usize,

    /// Static HTTP headers attached to every upload request.
    #[serde(default)]
    #[secret]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub headers: HashMap<String, String>,
}

fn default_file_upload_bundle_method() -> String {
    "POST".into()
}

fn default_file_upload_bundle_field_name() -> String {
    "file".into()
}

fn default_file_upload_bundle_max_file_size_bytes() -> u64 {
    10 * 1024 * 1024
}

fn default_file_upload_bundle_max_total_size_bytes() -> u64 {
    32 * 1024 * 1024
}

fn default_file_upload_bundle_max_files() -> u32 {
    16
}

fn default_file_upload_bundle_timeout_secs() -> u64 {
    120
}

fn default_file_upload_bundle_max_response_body_bytes() -> usize {
    4 * 1024
}

impl Default for FileUploadBundleConfig {
    fn default() -> Self {
        Self {
            url: None,
            method: default_file_upload_bundle_method(),
            field_name: default_file_upload_bundle_field_name(),
            max_file_size_bytes: default_file_upload_bundle_max_file_size_bytes(),
            max_total_size_bytes: default_file_upload_bundle_max_total_size_bytes(),
            max_files: default_file_upload_bundle_max_files(),
            timeout_secs: default_file_upload_bundle_timeout_secs(),
            max_response_body_bytes: default_file_upload_bundle_max_response_body_bytes(),
            headers: HashMap::new(),
        }
    }
}

// ── File Download ───────────────────────────────────────────────

/// Standalone file download tool configuration (`[file_download]`).
///
/// When `url` is set to a non-empty value, registers a `file_download` tool
/// that GETs a file from the configured endpoint and writes it to the agent's
/// workspace filesystem. The LLM supplies only a document identifier and a
/// workspace-relative destination path; the endpoint URL comes solely from this
/// config and is never model-controlled. Response bytes are streamed to disk
/// and never loaded into model context.
///
/// When `url` is `None` or empty, the tool is not registered.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "file-download"]
pub struct FileDownloadConfig {
    /// Download endpoint URL. Tool is disabled when this is `None` or empty.
    /// The file to fetch is selected by the `document_id` query parameter.
    #[serde(default)]
    pub url: Option<String>,

    /// Maximum download size in bytes. Enforced while streaming: the transfer
    /// is aborted and the partial file removed once this ceiling is exceeded,
    /// so an oversized or unbounded body never fully buffers in memory or lands
    /// on disk. Default: 25 MiB.
    #[serde(default = "default_file_download_max_size_bytes")]
    pub max_file_size_bytes: u64,

    /// Request timeout in seconds. Default: 120.
    #[serde(default = "default_file_download_timeout_secs")]
    pub timeout_secs: u64,

    /// Static HTTP headers attached to every download request — typically an
    /// `Authorization: Bearer …` token for the upstream endpoint. Same shape as
    /// `[mcp.servers.*.headers]`.
    #[serde(default)]
    #[secret]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub headers: HashMap<String, String>,
}

fn default_file_download_max_size_bytes() -> u64 {
    25 * 1024 * 1024
}

fn default_file_download_timeout_secs() -> u64 {
    120
}

impl Default for FileDownloadConfig {
    fn default() -> Self {
        Self {
            url: None,
            max_file_size_bytes: default_file_download_max_size_bytes(),
            timeout_secs: default_file_download_timeout_secs(),
            headers: HashMap::new(),
        }
    }
}

// ── Proxy ───────────────────────────────────────────────────────

/// Proxy application scope — determines which outbound traffic uses the proxy.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ProxyScope {
    /// Use system environment proxy variables only.
    Environment,
    /// Apply proxy to all ZeroClaw-managed HTTP traffic (default).
    #[default]
    Zeroclaw,
    /// Apply proxy only to explicitly listed service selectors.
    Services,
}

/// Proxy configuration for outbound HTTP/HTTPS/SOCKS5 traffic (`[proxy]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "proxy"]
pub struct ProxyConfig {
    /// Enable proxy support for selected scope.
    #[serde(default)]
    pub enabled: bool,
    /// Proxy URL for HTTP requests (supports http, https, socks5, socks5h).
    #[serde(default)]
    pub http_proxy: Option<String>,
    /// Proxy URL for HTTPS requests (supports http, https, socks5, socks5h).
    #[serde(default)]
    pub https_proxy: Option<String>,
    /// Fallback proxy URL for all schemes.
    #[serde(default)]
    pub all_proxy: Option<String>,
    /// No-proxy bypass list. Same format as NO_PROXY.
    #[serde(default)]
    pub no_proxy: Vec<String>,
    /// Proxy application scope.
    #[serde(default)]
    pub scope: ProxyScope,
    /// Service selectors used when scope = "services".
    #[serde(default)]
    pub services: Vec<String>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            http_proxy: None,
            https_proxy: None,
            all_proxy: None,
            no_proxy: Vec::new(),
            scope: ProxyScope::Zeroclaw,
            services: Vec::new(),
        }
    }
}

impl ProxyConfig {
    pub fn supported_service_keys() -> &'static [&'static str] {
        SUPPORTED_PROXY_SERVICE_KEYS
    }

    pub fn supported_service_selectors() -> &'static [&'static str] {
        SUPPORTED_PROXY_SERVICE_SELECTORS
    }

    pub fn has_any_proxy_url(&self) -> bool {
        normalize_proxy_url_option(self.http_proxy.as_deref()).is_some()
            || normalize_proxy_url_option(self.https_proxy.as_deref()).is_some()
            || normalize_proxy_url_option(self.all_proxy.as_deref()).is_some()
    }

    pub fn normalized_services(&self) -> Vec<String> {
        normalize_service_list(self.services.clone())
    }

    pub fn normalized_no_proxy(&self) -> Vec<String> {
        normalize_no_proxy_list(self.no_proxy.clone())
    }

    pub fn validate(&self) -> Result<()> {
        for (field, value) in [
            ("http_proxy", self.http_proxy.as_deref()),
            ("https_proxy", self.https_proxy.as_deref()),
            ("all_proxy", self.all_proxy.as_deref()),
        ] {
            if let Some(url) = normalize_proxy_url_option(value) {
                validate_proxy_url(field, &url)?;
            }
        }

        for selector in self.normalized_services() {
            if !is_supported_proxy_service_selector(&selector) {
                anyhow::bail!(
                    "Unsupported proxy service selector '{selector}'. Use tool `proxy_config` action `list_services` for valid values"
                );
            }
        }

        if self.enabled && !self.has_any_proxy_url() {
            anyhow::bail!(
                "Proxy is enabled but no proxy URL is configured. Set at least one of http_proxy, https_proxy, or all_proxy"
            );
        }

        if self.enabled
            && self.scope == ProxyScope::Services
            && self.normalized_services().is_empty()
        {
            anyhow::bail!(
                "proxy.scope='services' requires a non-empty proxy.services list when proxy is enabled"
            );
        }

        Ok(())
    }

    pub fn should_apply_to_service(&self, service_key: &str) -> bool {
        if !self.enabled {
            return false;
        }

        match self.scope {
            ProxyScope::Environment => false,
            ProxyScope::Zeroclaw => true,
            ProxyScope::Services => {
                let service_key = service_key.trim().to_ascii_lowercase();
                if service_key.is_empty() {
                    return false;
                }

                self.normalized_services()
                    .iter()
                    .any(|selector| service_selector_matches(selector, &service_key))
            }
        }
    }

    pub fn apply_to_reqwest_builder(
        &self,
        mut builder: reqwest::ClientBuilder,
        service_key: &str,
    ) -> reqwest::ClientBuilder {
        if !self.should_apply_to_service(service_key) {
            return builder;
        }

        let no_proxy = self.no_proxy_value();

        if let Some(url) = normalize_proxy_url_option(self.all_proxy.as_deref()) {
            match reqwest::Proxy::all(&url) {
                Ok(proxy) => {
                    builder = builder.proxy(apply_no_proxy(proxy, no_proxy.clone()));
                }
                Err(error) => {
                    ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"proxy_url": url, "service_key": service_key, "error": format!("{}", error)})), "Ignoring invalid all_proxy URL: ");
                }
            }
        }

        if let Some(url) = normalize_proxy_url_option(self.http_proxy.as_deref()) {
            match reqwest::Proxy::http(&url) {
                Ok(proxy) => {
                    builder = builder.proxy(apply_no_proxy(proxy, no_proxy.clone()));
                }
                Err(error) => {
                    ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"proxy_url": url, "service_key": service_key, "error": format!("{}", error)})), "Ignoring invalid http_proxy URL: ");
                }
            }
        }

        if let Some(url) = normalize_proxy_url_option(self.https_proxy.as_deref()) {
            match reqwest::Proxy::https(&url) {
                Ok(proxy) => {
                    builder = builder.proxy(apply_no_proxy(proxy, no_proxy));
                }
                Err(error) => {
                    ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"proxy_url": url, "service_key": service_key, "error": format!("{}", error)})), "Ignoring invalid https_proxy URL: ");
                }
            }
        }

        builder
    }

    pub fn apply_to_process_env(&self) {
        set_proxy_env_pair("HTTP_PROXY", self.http_proxy.as_deref());
        set_proxy_env_pair("HTTPS_PROXY", self.https_proxy.as_deref());
        set_proxy_env_pair("ALL_PROXY", self.all_proxy.as_deref());

        let no_proxy_joined = {
            let list = self.normalized_no_proxy();
            (!list.is_empty()).then(|| list.join(","))
        };
        set_proxy_env_pair("NO_PROXY", no_proxy_joined.as_deref());
    }

    pub fn clear_process_env() {
        clear_proxy_env_pair("HTTP_PROXY");
        clear_proxy_env_pair("HTTPS_PROXY");
        clear_proxy_env_pair("ALL_PROXY");
        clear_proxy_env_pair("NO_PROXY");
    }

    fn no_proxy_value(&self) -> Option<reqwest::NoProxy> {
        let joined = {
            let list = self.normalized_no_proxy();
            (!list.is_empty()).then(|| list.join(","))
        };
        joined.as_deref().and_then(reqwest::NoProxy::from_string)
    }
}

fn apply_no_proxy(proxy: reqwest::Proxy, no_proxy: Option<reqwest::NoProxy>) -> reqwest::Proxy {
    proxy.no_proxy(no_proxy)
}

fn normalize_proxy_url_option(raw: Option<&str>) -> Option<String> {
    let value = raw?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn normalize_no_proxy_list(values: Vec<String>) -> Vec<String> {
    normalize_comma_values(values)
}

fn normalize_service_list(values: Vec<String>) -> Vec<String> {
    let mut normalized = normalize_comma_values(values)
        .into_iter()
        .map(|value| value.to_ascii_lowercase())
        .collect::<Vec<_>>();
    normalized.sort_unstable();
    normalized.dedup();
    normalized
}

fn normalize_comma_values(values: Vec<String>) -> Vec<String> {
    let mut output = Vec::new();
    for value in values {
        for part in value.split(',') {
            let normalized = part.trim();
            if normalized.is_empty() {
                continue;
            }
            output.push(normalized.to_string());
        }
    }
    output.sort_unstable();
    output.dedup();
    output
}

fn is_supported_proxy_service_selector(selector: &str) -> bool {
    if SUPPORTED_PROXY_SERVICE_KEYS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(selector))
    {
        return true;
    }

    SUPPORTED_PROXY_SERVICE_SELECTORS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(selector))
}

fn service_selector_matches(selector: &str, service_key: &str) -> bool {
    if selector == service_key {
        return true;
    }

    if let Some(prefix) = selector.strip_suffix(".*") {
        return service_key.starts_with(prefix)
            && service_key
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.starts_with('.'));
    }

    false
}

const MCP_MAX_TOOL_TIMEOUT_SECS: u64 = 600;

fn validate_mcp_config(config: &McpConfig) -> Result<()> {
    let mut seen_names = std::collections::HashSet::new();
    for (i, server) in config.servers.iter().enumerate() {
        let name = server.name.trim();
        if name.is_empty() {
            validation_bail!(
                RequiredFieldEmpty,
                format!("mcp.servers[{i}].name"),
                "mcp.servers[{i}].name must not be empty"
            );
        }
        if !seen_names.insert(name.to_ascii_lowercase()) {
            anyhow::bail!("mcp.servers contains duplicate name: {name}");
        }

        if let Some(timeout) = server.tool_timeout_secs {
            if timeout == 0 {
                validation_bail!(
                    InvalidNumericRange,
                    format!("mcp.servers[{i}].tool_timeout_secs"),
                    "mcp.servers[{i}].tool_timeout_secs must be greater than 0"
                );
            }
            if timeout > MCP_MAX_TOOL_TIMEOUT_SECS {
                anyhow::bail!(
                    "mcp.servers[{i}].tool_timeout_secs exceeds max {MCP_MAX_TOOL_TIMEOUT_SECS}"
                );
            }
        }

        // The transport -> required-leaf relationship is owned by
        // `McpTransport::required_leaf`, which feeds the `x-required-by-transport`
        // schema metadata. The validator matches on the `McpTransport` enum so a
        // new variant is a compile error here rather than a runtime fall-through.
        match server.transport {
            McpTransport::Stdio => {
                if server.command.trim().is_empty() {
                    anyhow::bail!(
                        "mcp.servers[{i}] with transport=stdio requires non-empty command"
                    );
                }
            }
            McpTransport::Http | McpTransport::Sse => {
                let url = server
                    .url
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        let transport_str = server.transport.wire_name();
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Reject
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "index": i,
                                "transport": transport_str,
                            })),
                            "mcp.servers entry rejected: transport requires url"
                        );
                        anyhow::Error::msg(format!(
                            "mcp.servers[{i}] with transport={transport_str} requires url"
                        ))
                    })?;
                let parsed = reqwest::Url::parse(url)
                    .with_context(|| format!("mcp.servers[{i}].url is not a valid URL"))?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    anyhow::bail!("mcp.servers[{i}].url must use http/https");
                }
            }
        }
    }
    Ok(())
}

fn validate_proxy_url(field: &str, url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url)
        .with_context(|| format!("Invalid {field} URL: '{url}' is not a valid URL"))?;

    match parsed.scheme() {
        "http" | "https" | "socks5" | "socks5h" | "socks" => {}
        scheme => {
            anyhow::bail!(
                "Invalid {field} URL scheme '{scheme}'. Allowed: http, https, socks5, socks5h, socks"
            );
        }
    }

    if parsed.host_str().is_none() {
        anyhow::bail!("Invalid {field} URL: host is required");
    }

    Ok(())
}

fn validate_http_base_url(field: &str, url: &str) -> Result<()> {
    if url.trim().is_empty() {
        validation_bail!(
            RequiredFieldEmpty,
            field.to_string(),
            "{field} must not be empty"
        );
    }

    let parsed = match reqwest::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(err) => {
            validation_bail!(
                InvalidFormat,
                field.to_string(),
                "{field} must be a valid URL: {err}"
            );
        }
    };

    if !matches!(parsed.scheme(), "http" | "https") {
        validation_bail!(
            InvalidFormat,
            field.to_string(),
            "{field} must use http:// or https://"
        );
    }

    if parsed.host_str().is_none() {
        validation_bail!(
            InvalidFormat,
            field.to_string(),
            "{field} must include a host"
        );
    }

    Ok(())
}

/// Shared bot-token rule for channel structs whose `bot_token` is required
/// once the alias is enabled (Telegram, Discord). `field_path` must be the
/// `channels.<type>.<alias>.bot_token` leaf; the enabled-state message
/// derives the sibling `.enabled` path from it.
fn validate_required_bot_token(field_path: &str, enabled: bool, token: &str) -> Result<()> {
    if token.trim() == crate::traits::UNSET_DISPLAY {
        validation_bail!(
            RequiredFieldEmpty,
            field_path.to_string(),
            "{field_path} must not contain the unset display placeholder",
        );
    }
    if enabled && crate::traits::is_unset_display_value(token) {
        let enabled_path = field_path.strip_suffix("bot_token").map_or_else(
            || field_path.to_string(),
            |prefix| format!("{prefix}enabled"),
        );
        validation_bail!(
            RequiredFieldEmpty,
            field_path.to_string(),
            "{field_path} is required when {enabled_path} = true",
        );
    }
    Ok(())
}

fn set_proxy_env_pair(key: &str, value: Option<&str>) {
    let lowercase_key = key.to_ascii_lowercase();
    if let Some(value) = value.and_then(|candidate| normalize_proxy_url_option(Some(candidate))) {
        // SAFETY: called during single-threaded config init before async runtime starts.
        unsafe {
            std::env::set_var(key, &value);
            std::env::set_var(lowercase_key, value);
        }
    } else {
        // SAFETY: called during single-threaded config init before async runtime starts.
        unsafe {
            std::env::remove_var(key);
            std::env::remove_var(lowercase_key);
        }
    }
}

fn clear_proxy_env_pair(key: &str) {
    // SAFETY: called during single-threaded config init before async runtime starts.
    unsafe {
        std::env::remove_var(key);
        std::env::remove_var(key.to_ascii_lowercase());
    }
}

fn runtime_proxy_state() -> &'static RwLock<ProxyConfig> {
    RUNTIME_PROXY_CONFIG.get_or_init(|| RwLock::new(ProxyConfig::default()))
}

fn runtime_proxy_client_cache() -> &'static RwLock<HashMap<String, reqwest::Client>> {
    RUNTIME_PROXY_CLIENT_CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

fn clear_runtime_proxy_client_cache() {
    match runtime_proxy_client_cache().write() {
        Ok(mut guard) => {
            guard.clear();
        }
        Err(poisoned) => {
            poisoned.into_inner().clear();
        }
    }
}

fn runtime_proxy_cache_key(
    service_key: &str,
    timeout_secs: Option<u64>,
    connect_timeout_secs: Option<u64>,
) -> String {
    format!(
        "{}|timeout={}|connect_timeout={}",
        service_key.trim().to_ascii_lowercase(),
        timeout_secs
            .map(|value| value.to_string())
            .unwrap_or_else(|| "none".to_string()),
        connect_timeout_secs
            .map(|value| value.to_string())
            .unwrap_or_else(|| "none".to_string())
    )
}

fn runtime_proxy_cached_client(cache_key: &str) -> Option<reqwest::Client> {
    match runtime_proxy_client_cache().read() {
        Ok(guard) => guard.get(cache_key).cloned(),
        Err(poisoned) => poisoned.into_inner().get(cache_key).cloned(),
    }
}

fn set_runtime_proxy_cached_client(cache_key: String, client: reqwest::Client) {
    match runtime_proxy_client_cache().write() {
        Ok(mut guard) => {
            guard.insert(cache_key, client);
        }
        Err(poisoned) => {
            poisoned.into_inner().insert(cache_key, client);
        }
    }
}

pub fn set_runtime_proxy_config(config: ProxyConfig) {
    match runtime_proxy_state().write() {
        Ok(mut guard) => {
            *guard = config;
        }
        Err(poisoned) => {
            *poisoned.into_inner() = config;
        }
    }

    clear_runtime_proxy_client_cache();
}

pub fn runtime_proxy_config() -> ProxyConfig {
    match runtime_proxy_state().read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// Reapply the persisted canonical `[proxy]` config at process startup.
///
/// The model-visible `proxy_config` tool no longer registers, so the
/// persisted config file is the only way proxy state changes; the daemon
/// must therefore make the loaded file effective on its own:
///
/// - the runtime proxy global (read by every proxy-aware client builder)
///   is seeded unconditionally, whatever the scope, and
/// - process proxy environment variables are broadcast only when the
///   config is enabled with `scope = "environment"`, matching the
///   condition the retired `apply_env` action enforced.
///
/// A config that fails `validate()` is applied disabled (mirroring the
/// original config-load behavior): the global gets the explicit default
/// and no environment variable is written. Callers must invoke this once
/// per process boot, before the multi-threaded async runtime (and any
/// task that reads proxy environment variables) starts — process-env
/// mutation is only sound there. A daemon reload refreshes only the
/// runtime global (`set_runtime_proxy_config`), keeping live-env
/// application restart-only.
pub fn apply_persisted_proxy_on_boot(proxy: &ProxyConfig) {
    let mut effective = proxy.clone();
    if let Err(error) = effective.validate() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"error": format!("{error}")})),
            "Invalid persisted proxy configuration ignored at startup; applying disabled proxy"
        );
        effective.enabled = false;
    }
    set_runtime_proxy_config(effective.clone());
    if effective.enabled && effective.scope == ProxyScope::Environment {
        effective.apply_to_process_env();
    }
}

/// Read the persisted `[proxy]` section using the same install-directory
/// resolution as [`Config::load_or_init`], for the pre-runtime startup
/// application of proxy state. Returns `None` when the install or its
/// config file cannot be read (including a fresh install with no file
/// yet), or when the on-disk schema version is not the current one: a
/// legacy V1/V2 config is auto-migrated in memory by the full loader and
/// the disk file is deliberately left untouched, so a raw pre-read of a
/// legacy file is not authoritative — boot application for such an
/// install starts once the migration is locked into the file
/// (`zeroclaw config migrate` or any config save). Before this
/// mechanism existed nothing applied persisted proxy state at boot, so
/// never-migrated legacy installs are no worse than they were; a
/// future/unknown version is rejected by the full loader and must not
/// be applied speculatively either. Proxy fields are never
/// secret-annotated, so plain parsing is sufficient.
///
/// This is a raw file read by design: `ZEROCLAW_PROXY_*` env-var
/// overrides are NOT reflected here. The runtime global is refreshed
/// from the fully-loaded config after `load_or_init` runs; the process
/// env stays restart-only and follows the persisted file.
pub async fn read_persisted_proxy_for_boot() -> Option<ProxyConfig> {
    let (default_zeroclaw_dir, default_workspace_dir) = default_config_and_data_dirs().ok()?;
    let (zeroclaw_dir, _legacy_workspace_dir, _source) =
        resolve_runtime_config_dirs(&default_zeroclaw_dir, &default_workspace_dir)
            .await
            .ok()?;
    let raw = std::fs::read_to_string(zeroclaw_dir.join("config.toml")).ok()?;
    let value: toml::Value = toml::from_str(&raw).ok()?;
    if crate::migration::detect_version(&value).ok()
        != Some(crate::migration::CURRENT_SCHEMA_VERSION)
    {
        return None;
    }
    let proxy_table = value.get("proxy")?.clone();
    ProxyConfig::deserialize(proxy_table).ok()
}

pub fn apply_runtime_proxy_to_builder(
    builder: reqwest::ClientBuilder,
    service_key: &str,
) -> reqwest::ClientBuilder {
    runtime_proxy_config().apply_to_reqwest_builder(builder, service_key)
}

pub fn build_runtime_proxy_client(service_key: &str) -> reqwest::Client {
    let cache_key = runtime_proxy_cache_key(service_key, None, None);
    if let Some(client) = runtime_proxy_cached_client(&cache_key) {
        return client;
    }

    let builder = apply_runtime_proxy_to_builder(reqwest::Client::builder(), service_key);
    let client = builder.build().unwrap_or_else(|error| {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(
                    ::serde_json::json!({"service_key": service_key, "error": format!("{}", error)})
                ),
            "Failed to build proxied client: "
        );
        reqwest::Client::new()
    });
    set_runtime_proxy_cached_client(cache_key, client.clone());
    client
}

pub fn build_runtime_proxy_client_with_timeouts(
    service_key: &str,
    timeout_secs: u64,
    connect_timeout_secs: u64,
) -> reqwest::Client {
    let cache_key =
        runtime_proxy_cache_key(service_key, Some(timeout_secs), Some(connect_timeout_secs));
    if let Some(client) = runtime_proxy_cached_client(&cache_key) {
        return client;
    }

    let builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .connect_timeout(std::time::Duration::from_secs(connect_timeout_secs));
    let builder = apply_runtime_proxy_to_builder(builder, service_key);
    let client = builder.build().unwrap_or_else(|error| {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(
                    ::serde_json::json!({"service_key": service_key, "error": format!("{}", error)})
                ),
            "Failed to build proxied timeout client: "
        );
        reqwest::Client::new()
    });
    set_runtime_proxy_cached_client(cache_key, client.clone());
    client
}

/// Build an HTTP client for a channel, using an explicit per-channel proxy URL
/// when configured. Falls back to the global runtime proxy when `proxy_url` is
/// `None` or empty.
pub fn build_channel_proxy_client(service_key: &str, proxy_url: Option<&str>) -> reqwest::Client {
    match normalize_proxy_url_option(proxy_url) {
        Some(url) => build_explicit_proxy_client(service_key, &url, None, None),
        None => build_runtime_proxy_client(service_key),
    }
}

/// Build an HTTP client for a channel with custom timeouts, using an explicit
/// per-channel proxy URL when configured. Falls back to the global runtime
/// proxy when `proxy_url` is `None` or empty.
pub fn build_channel_proxy_client_with_timeouts(
    service_key: &str,
    proxy_url: Option<&str>,
    timeout_secs: u64,
    connect_timeout_secs: u64,
) -> reqwest::Client {
    match normalize_proxy_url_option(proxy_url) {
        Some(url) => build_explicit_proxy_client(
            service_key,
            &url,
            Some(timeout_secs),
            Some(connect_timeout_secs),
        ),
        None => build_runtime_proxy_client_with_timeouts(
            service_key,
            timeout_secs,
            connect_timeout_secs,
        ),
    }
}

/// Apply an explicit proxy URL to a `reqwest::ClientBuilder`, returning the
/// modified builder. Used by channels that specify a per-channel `proxy_url`.
pub fn apply_channel_proxy_to_builder(
    builder: reqwest::ClientBuilder,
    service_key: &str,
    proxy_url: Option<&str>,
) -> reqwest::ClientBuilder {
    match normalize_proxy_url_option(proxy_url) {
        Some(url) => apply_explicit_proxy_to_builder(builder, service_key, &url),
        None => apply_runtime_proxy_to_builder(builder, service_key),
    }
}

/// Build a client with a single explicit proxy URL (http+https via `Proxy::all`).
fn build_explicit_proxy_client(
    service_key: &str,
    proxy_url: &str,
    timeout_secs: Option<u64>,
    connect_timeout_secs: Option<u64>,
) -> reqwest::Client {
    let cache_key = format!(
        "explicit|{}|{}|timeout={}|connect_timeout={}",
        service_key.trim().to_ascii_lowercase(),
        proxy_url,
        timeout_secs
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".to_string()),
        connect_timeout_secs
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".to_string()),
    );
    if let Some(client) = runtime_proxy_cached_client(&cache_key) {
        return client;
    }

    let mut builder = reqwest::Client::builder();
    if let Some(t) = timeout_secs {
        builder = builder.timeout(std::time::Duration::from_secs(t));
    }
    if let Some(ct) = connect_timeout_secs {
        builder = builder.connect_timeout(std::time::Duration::from_secs(ct));
    }
    builder = apply_explicit_proxy_to_builder(builder, service_key, proxy_url);
    let client = builder.build().unwrap_or_else(|error| {
        ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"service_key": service_key, "proxy_url": proxy_url, "error": format!("{}", error)})), "Failed to build channel proxy client: ");
        reqwest::Client::new()
    });
    set_runtime_proxy_cached_client(cache_key, client.clone());
    client
}

/// Apply a single explicit proxy URL to a builder via `Proxy::all`.
fn apply_explicit_proxy_to_builder(
    mut builder: reqwest::ClientBuilder,
    service_key: &str,
    proxy_url: &str,
) -> reqwest::ClientBuilder {
    match reqwest::Proxy::all(proxy_url) {
        Ok(proxy) => {
            builder = builder.proxy(proxy);
        }
        Err(error) => {
            ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"proxy_url": proxy_url, "service_key": service_key, "error": format!("{}", error)})), "Ignoring invalid channel proxy_url: ");
        }
    }
    builder
}

// ── Proxy-aware WebSocket connect ────────────────────────────────
//
// `tokio_tungstenite::connect_async` does not honour proxy settings.
// The helpers below resolve the effective proxy URL for a given service
// key and, when a proxy is active, establish a tunnelled TCP connection
// (HTTP CONNECT for http/https proxies, SOCKS5 for socks5/socks5h)
// before handing the stream to `tokio_tungstenite` for the WebSocket
// handshake.

/// Combined async IO trait for boxed WebSocket transport streams.
trait AsyncReadWrite: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> AsyncReadWrite for T {}

/// A boxed async IO stream used when a WebSocket connection is tunnelled
/// through a proxy. The concrete type varies depending on the proxy
/// kind (HTTP CONNECT vs SOCKS5) and the target scheme (ws vs wss).
///
/// We wrap in a newtype so we can implement `AsyncRead` and `AsyncWrite`
/// via delegation, since Rust trait objects cannot combine multiple
/// non-auto traits.
pub struct BoxedIo(Box<dyn AsyncReadWrite>);

impl tokio::io::AsyncRead for BoxedIo {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.0).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for BoxedIo {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut *self.0).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.0).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.0).poll_shutdown(cx)
    }
}

impl Unpin for BoxedIo {}

/// Convenience alias for the WebSocket stream returned by the proxy-aware
/// connect helpers.
pub type ProxiedWsStream = tokio_tungstenite::WebSocketStream<BoxedIo>;

/// Resolve the effective proxy URL for a WebSocket connection to the
/// given `ws_url`, taking into account the per-channel `proxy_url`
/// override, the runtime proxy config, scope and no_proxy list.
fn resolve_ws_proxy_url(
    service_key: &str,
    ws_url: &str,
    channel_proxy_url: Option<&str>,
) -> Option<String> {
    // 1. Explicit per-channel proxy always wins.
    if let Some(url) = normalize_proxy_url_option(channel_proxy_url) {
        return Some(url);
    }

    // 2. Consult the runtime proxy config.
    let cfg = runtime_proxy_config();
    if !cfg.should_apply_to_service(service_key) {
        return None;
    }

    // Check the no_proxy list against the WebSocket target host.
    if let Ok(parsed) = reqwest::Url::parse(ws_url)
        && let Some(host) = parsed.host_str()
    {
        let no_proxy_entries = cfg.normalized_no_proxy();
        if !no_proxy_entries.is_empty() {
            let host_lower = host.to_ascii_lowercase();
            let matches_no_proxy = no_proxy_entries.iter().any(|entry| {
                let entry = entry.trim().to_ascii_lowercase();
                if entry == "*" {
                    return true;
                }
                if host_lower == entry {
                    return true;
                }
                // Support ".example.com" matching "foo.example.com"
                if let Some(suffix) = entry.strip_prefix('.') {
                    return host_lower.ends_with(suffix) || host_lower == suffix;
                }
                // Support "example.com" also matching "foo.example.com"
                host_lower.ends_with(&format!(".{entry}"))
            });
            if matches_no_proxy {
                return None;
            }
        }
    }

    // For wss:// prefer https_proxy, for ws:// prefer http_proxy, fall
    // back to all_proxy in both cases.
    let is_secure = ws_url.starts_with("wss://") || ws_url.starts_with("wss:");
    let preferred = if is_secure {
        normalize_proxy_url_option(cfg.https_proxy.as_deref())
    } else {
        normalize_proxy_url_option(cfg.http_proxy.as_deref())
    };
    preferred.or_else(|| normalize_proxy_url_option(cfg.all_proxy.as_deref()))
}

/// Connect a WebSocket through the configured proxy (if any).
///
/// When no proxy applies, this is a thin wrapper around
/// `tokio_tungstenite::connect_async`. When a proxy is active the
/// function tunnels the TCP connection through the proxy before
/// performing the WebSocket upgrade.
///
/// `service_key` is the proxy-service selector (e.g. `"channel.discord"`).
/// `channel_proxy_url` is the optional per-channel proxy override.
pub async fn ws_connect_with_proxy(
    ws_url: &str,
    service_key: &str,
    channel_proxy_url: Option<&str>,
) -> anyhow::Result<(
    ProxiedWsStream,
    tokio_tungstenite::tungstenite::http::Response<Option<Vec<u8>>>,
)> {
    let proxy_url = resolve_ws_proxy_url(service_key, ws_url, channel_proxy_url);

    match proxy_url {
        None => {
            // No proxy — establish TCP+TLS manually, wrap in BoxedIo, then
            // perform the WebSocket handshake over the wrapped stream.
            //
            // Previous implementation used `connect_async` followed by
            // `into_inner()` + `from_raw_socket` to normalize the return
            // type. That pattern discards data already buffered by the
            // tungstenite frame codec, causing channels (Slack Socket Mode,
            // Discord, etc.) to silently miss the first frames sent by the
            // server and all subsequent events.
            use tokio::net::TcpStream;

            let target = reqwest::Url::parse(ws_url)
                .with_context(|| format!("Invalid WebSocket URL: {ws_url}"))?;
            let target_host = target
                .host_str()
                .ok_or_else(|| {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({"ws_url": ws_url})),
                        "WebSocket URL has no host"
                    );
                    anyhow::Error::msg(format!("WebSocket URL has no host: {ws_url}"))
                })?
                .to_string();
            let target_port = target
                .port_or_known_default()
                .unwrap_or(if target.scheme() == "wss" { 443 } else { 80 });

            let tcp = TcpStream::connect(format!("{target_host}:{target_port}"))
                .await
                .with_context(|| format!("TCP connect to {target_host}:{target_port}"))?;

            let is_secure = target.scheme() == "wss";
            let stream: BoxedIo = if is_secure {
                let mut root_store = rustls::RootCertStore::empty();
                root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                let tls_config = std::sync::Arc::new(
                    rustls::ClientConfig::builder()
                        .with_root_certificates(root_store)
                        .with_no_client_auth(),
                );
                let connector = tokio_rustls::TlsConnector::from(tls_config);
                let server_name = rustls_pki_types::ServerName::try_from(target_host.clone())
                    .with_context(|| format!("Invalid TLS server name: {target_host}"))?;
                let tls_stream = connector
                    .connect(server_name, tcp)
                    .await
                    .with_context(|| format!("TLS handshake with {target_host}"))?;
                BoxedIo(Box::new(tls_stream))
            } else {
                BoxedIo(Box::new(tcp))
            };

            let default_port = if is_secure { 443 } else { 80 };
            let host_header = if target_port == default_port {
                target_host.clone()
            } else {
                format!("{target_host}:{target_port}")
            };

            let ws_request = tokio_tungstenite::tungstenite::http::Request::builder()
                .uri(ws_url)
                .header("Host", host_header)
                .header("Connection", "Upgrade")
                .header("Upgrade", "websocket")
                .header(
                    "Sec-WebSocket-Key",
                    tokio_tungstenite::tungstenite::handshake::client::generate_key(),
                )
                .header("Sec-WebSocket-Version", "13")
                .body(())
                .with_context(|| "Failed to build WebSocket upgrade request")?;

            let (ws_stream, response) =
                tokio_tungstenite::client_async(ws_request, stream)
                    .await
                    .with_context(|| format!("WebSocket handshake failed for {ws_url}"))?;

            Ok((ws_stream, response))
        }
        Some(proxy) => ws_connect_via_proxy(ws_url, &proxy).await,
    }
}

/// Establish a WebSocket connection tunnelled through the given proxy URL.
async fn ws_connect_via_proxy(
    ws_url: &str,
    proxy_url: &str,
) -> anyhow::Result<(
    ProxiedWsStream,
    tokio_tungstenite::tungstenite::http::Response<Option<Vec<u8>>>,
)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt as _};
    use tokio::net::TcpStream;

    let target =
        reqwest::Url::parse(ws_url).with_context(|| format!("Invalid WebSocket URL: {ws_url}"))?;
    let target_host = target
        .host_str()
        .ok_or_else(|| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"ws_url": ws_url})),
                "WebSocket URL has no host"
            );
            anyhow::Error::msg(format!("WebSocket URL has no host: {ws_url}"))
        })?
        .to_string();
    let target_port = target
        .port_or_known_default()
        .unwrap_or(if target.scheme() == "wss" { 443 } else { 80 });

    let proxy = reqwest::Url::parse(proxy_url)
        .with_context(|| format!("Invalid proxy URL: {proxy_url}"))?;

    let stream: BoxedIo = match proxy.scheme() {
        "socks5" | "socks5h" | "socks" => {
            let proxy_addr = format!(
                "{}:{}",
                proxy.host_str().unwrap_or("127.0.0.1"),
                proxy.port_or_known_default().unwrap_or(1080)
            );
            let target_addr = format!("{target_host}:{target_port}");
            let socks_stream = if proxy.username().is_empty() {
                tokio_socks::tcp::Socks5Stream::connect(proxy_addr.as_str(), target_addr.as_str())
                    .await
                    .with_context(|| format!("SOCKS5 connect to {target_addr} via {proxy_addr}"))?
            } else {
                let password = proxy.password().unwrap_or("");
                tokio_socks::tcp::Socks5Stream::connect_with_password(
                    proxy_addr.as_str(),
                    target_addr.as_str(),
                    proxy.username(),
                    password,
                )
                .await
                .with_context(|| format!("SOCKS5 auth connect to {target_addr} via {proxy_addr}"))?
            };
            let tcp: TcpStream = socks_stream.into_inner();
            BoxedIo(Box::new(tcp))
        }
        "http" | "https" => {
            let proxy_host = proxy.host_str().unwrap_or("127.0.0.1");
            let proxy_port = proxy.port_or_known_default().unwrap_or(8080);
            let proxy_addr = format!("{proxy_host}:{proxy_port}");

            let mut tcp = TcpStream::connect(&proxy_addr)
                .await
                .with_context(|| format!("TCP connect to HTTP proxy {proxy_addr}"))?;

            // Send HTTP CONNECT request.
            let connect_req = format!(
                "CONNECT {target_host}:{target_port} HTTP/1.1\r\nHost: {target_host}:{target_port}\r\n\r\n"
            );
            tcp.write_all(connect_req.as_bytes()).await?;

            // Read the response (we only need the status line).
            let mut buf = vec![0u8; 4096];
            let mut total = 0usize;
            loop {
                let n = tcp.read(&mut buf[total..]).await?;
                if n == 0 {
                    anyhow::bail!("HTTP CONNECT proxy closed connection before response");
                }
                total += n;
                // Look for end of HTTP headers.
                if let Some(pos) = find_header_end(&buf[..total]) {
                    let status_line = std::str::from_utf8(&buf[..pos])
                        .unwrap_or("")
                        .lines()
                        .next()
                        .unwrap_or("");
                    if !status_line.contains("200") {
                        anyhow::bail!(
                            "HTTP CONNECT proxy returned non-200 response: {status_line}"
                        );
                    }
                    break;
                }
                if total >= buf.len() {
                    anyhow::bail!("HTTP CONNECT proxy response too large");
                }
            }

            BoxedIo(Box::new(tcp))
        }
        scheme => {
            anyhow::bail!("Unsupported proxy scheme '{scheme}' for WebSocket connections");
        }
    };

    // If the target is wss://, wrap in TLS.
    let is_secure = target.scheme() == "wss";
    let stream: BoxedIo = if is_secure {
        let mut root_store = rustls::RootCertStore::empty();
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls_config = std::sync::Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth(),
        );
        let connector = tokio_rustls::TlsConnector::from(tls_config);
        let server_name = rustls_pki_types::ServerName::try_from(target_host.clone())
            .with_context(|| format!("Invalid TLS server name: {target_host}"))?;

        // `stream` is `BoxedIo` — we need a concrete `AsyncRead + AsyncWrite`
        // for `TlsConnector::connect`. Since `BoxedIo` already satisfies
        // those bounds we can pass it directly.
        let tls_stream = connector
            .connect(server_name, stream)
            .await
            .with_context(|| format!("TLS handshake with {target_host}"))?;
        BoxedIo(Box::new(tls_stream))
    } else {
        stream
    };

    // Perform the WebSocket client handshake over the tunnelled stream.
    let ws_request = tokio_tungstenite::tungstenite::http::Request::builder()
        .uri(ws_url)
        .header("Host", format!("{target_host}:{target_port}"))
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header(
            "Sec-WebSocket-Key",
            tokio_tungstenite::tungstenite::handshake::client::generate_key(),
        )
        .header("Sec-WebSocket-Version", "13")
        .body(())
        .with_context(|| "Failed to build WebSocket upgrade request")?;

    let (ws_stream, response) = tokio_tungstenite::client_async(ws_request, stream)
        .await
        .with_context(|| format!("WebSocket handshake failed for {ws_url}"))?;

    Ok((ws_stream, response))
}

/// Find the `\r\n\r\n` boundary marking the end of HTTP headers.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

// ── Memory ───────────────────────────────────────────────────

/// Persistent storage configuration (`[storage]` section).
///
/// Storage is a two-tier alias-keyed map: `[storage.<backend>.<alias>]`,
/// parallel to `[providers.models.<type>.<alias>]`. Each backend has its own typed
/// config struct. `MemoryConfig.backend` carries a dotted reference (`"sqlite.default"`,
/// `"postgres.work"`) that resolves to one of these entries via
/// [`Config::resolve_active_storage`].
#[derive(Debug, Clone, Serialize, Deserialize, Default, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "storage"]
pub struct StorageConfig {
    /// SQLite storage instances (`[storage.sqlite.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub sqlite: HashMap<String, SqliteStorageConfig>,
    /// PostgreSQL storage instances (`[storage.postgres.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub postgres: HashMap<String, PostgresStorageConfig>,
    /// Qdrant storage instances (`[storage.qdrant.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub qdrant: HashMap<String, QdrantStorageConfig>,
    /// Markdown storage instances (`[storage.markdown.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub markdown: HashMap<String, MarkdownStorageConfig>,
    /// Lucid CLI sync instances (`[storage.lucid.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub lucid: HashMap<String, LucidStorageConfig>,
}

/// SQLite storage backend (`[storage.sqlite.<alias>]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "storage_sqlite"]
#[serde(default)]
pub struct SqliteStorageConfig {
    /// Optional override for the SQLite database path.
    /// When unset, defaults to `<workspace_dir>/brain.db`.
    pub path: Option<String>,
    /// Maximum seconds to wait when opening the DB if it's locked.
    /// `None` waits indefinitely. Recommended max: 300.
    pub open_timeout_secs: Option<u64>,
}

/// PostgreSQL storage backend (`[storage.postgres.<alias>]`).
///
/// Holds connection parameters AND pgvector settings on one alias-keyed
/// entry; previously these lived in two separate sections.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "storage_postgres"]
#[serde(default)]
pub struct PostgresStorageConfig {
    /// Connection URL (e.g. `"postgres://user:pass@host/db"`).
    /// Accepts legacy aliases: dbURL, database_url, databaseUrl.
    #[serde(alias = "dbURL", alias = "database_url", alias = "databaseUrl")]
    #[secret]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub db_url: Option<String>,
    /// Database schema for the memory table.
    pub schema: String,
    /// Table name for memory entries.
    pub table: String,
    /// Optional connection timeout in seconds.
    pub connect_timeout_secs: Option<u64>,
    /// Enable pgvector extension for hybrid vector+keyword recall.
    pub vector_enabled: bool,
    /// Vector dimensions for pgvector embeddings.
    pub vector_dimensions: usize,
}

impl Default for PostgresStorageConfig {
    fn default() -> Self {
        Self {
            db_url: None,
            schema: default_storage_schema(),
            table: default_storage_table(),
            connect_timeout_secs: None,
            vector_enabled: false,
            vector_dimensions: default_pgvector_dimensions(),
        }
    }
}

/// Qdrant vector database backend (`[storage.qdrant.<alias>]`).
///
/// URL, collection, and API key are typed config values. Use schema-mirror env
/// overrides such as `ZEROCLAW_storage__qdrant__<alias>__url=...` for runtime
/// env injection.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "storage_qdrant"]
#[serde(default)]
pub struct QdrantStorageConfig {
    /// Qdrant server URL (e.g. `"http://localhost:6333"`).
    /// Use `ZEROCLAW_storage__qdrant__<alias>__url=...` for env injection.
    pub url: Option<String>,
    /// Collection name for storing memories.
    /// Use `ZEROCLAW_storage__qdrant__<alias>__collection=...` for env injection.
    pub collection: String,
    /// API key for Qdrant Cloud or secured instances.
    /// Use `ZEROCLAW_storage__qdrant__<alias>__api_key=...` for env injection.
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_key: Option<String>,
}

impl Default for QdrantStorageConfig {
    fn default() -> Self {
        Self {
            url: None,
            collection: default_qdrant_collection(),
            api_key: None,
        }
    }
}

/// Markdown directory storage (`[storage.markdown.<alias>]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "storage_markdown"]
#[serde(default)]
pub struct MarkdownStorageConfig {
    /// Optional override for the markdown root directory.
    /// When unset, defaults to `<workspace_dir>/memory/`.
    pub directory: Option<String>,
}

/// Lucid CLI sync backend (`[storage.lucid.<alias>]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "storage_lucid"]
#[serde(default)]
pub struct LucidStorageConfig {
    /// Optional path to the lucid-memory binary. When unset, the bare `lucid`
    /// executable is resolved on `PATH`. A blank or whitespace-only value is
    /// rejected at config validation and remains invalid if startup continues,
    /// so an invalid explicit selector never falls through to `PATH`.
    pub binary_path: Option<String>,
    /// Recall (context) timeout override, in milliseconds. Lucid CLI cold
    /// starts (loading the local embedding model) can exceed 1.5s on ARM
    /// hosts; raise this if recalls still time out on your hardware.
    /// Unset falls back to the built-in default (3000ms). `0` is rejected
    /// at config validation; because startup warns and continues after
    /// validation errors, a `0` that reaches the runtime is treated as
    /// unset and falls back to the default.
    pub recall_timeout_ms: Option<u64>,
    /// Store timeout override, in milliseconds. Same cold-start
    /// consideration as `recall_timeout_ms`. Unset falls back to the
    /// built-in default (3000ms); `0` is rejected at validation and, if
    /// startup continues after the warning, is likewise treated as unset.
    pub store_timeout_ms: Option<u64>,
}

fn default_storage_schema() -> String {
    "public".into()
}

fn default_storage_table() -> String {
    "memories".into()
}

fn default_qdrant_collection() -> String {
    "zeroclaw_memories".into()
}

/// Search strategy for memory recall.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, zeroclaw_macros::ConfigEnum)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// Pure keyword search (FTS5 BM25)
    Bm25,
    /// Pure vector/semantic search
    Embedding,
    /// Weighted combination of keyword + vector (default)
    #[default]
    Hybrid,
}

/// Memory backend configuration (`[memory]` section).
///
/// Controls conversation memory storage, embeddings, hybrid search, response
/// caching, and memory snapshot/hydration. Backend-specific connection settings
/// live under `[storage.<backend>.<alias>]`; this section selects which storage
/// instance to use via the `backend` dotted reference.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "memory"]
#[allow(clippy::struct_excessive_bools)]
pub struct MemoryConfig {
    /// Dotted reference to the active storage instance: `<backend>.<alias>`
    /// (e.g. `"sqlite.default"`, `"postgres.work"`). Resolves through
    /// `Config.storage.<backend>.<alias>` at runtime. Bare backend names
    /// (`"sqlite"`) are treated as `"<backend>.default"`. Set to `"none"` to
    /// disable persistence entirely.
    #[serde(default = "default_memory_backend")]
    pub backend: String,
    /// Auto-save what *you* tell ZeroClaw into memory as conversation history — the agent's own replies are not saved. Turn off if you want memory to only hold things you explicitly record via the memory tool.
    #[serde(default = "default_auto_save")]
    pub auto_save: bool,
    /// Run the periodic hygiene pass that archives stale daily/session files and enforces retention windows. Leave on unless you want to manage cleanup yourself.
    #[serde(default = "default_hygiene_enabled")]
    pub hygiene_enabled: bool,
    /// Also extract atomic durable facts from each consolidated turn and store
    /// them as individual Core memories. Default off; the flip is sequenced in
    /// a later phase. SQLite-only: enabling this requires the sqlite memory
    /// backend globally and on every agent (validated at config load).
    #[serde(default)]
    pub consolidation_extract_facts: bool,
    /// Move daily/session files to the archive directory after this many days. Keeps the hot working set small without deleting history.
    #[serde(default = "default_archive_after_days")]
    pub archive_after_days: u32,
    /// Delete archived files permanently after this many days. Set high if you need long-term history; set low for privacy / disk-space reasons.
    #[serde(default = "default_purge_after_days")]
    pub purge_after_days: u32,
    /// Delete conversation rows older than this many days from the DB (sqlite backend only). Age is measured by `updated_at` (last write time). 0 = keep forever.
    #[serde(default = "default_conversation_retention_days")]
    pub conversation_retention_days: u32,
    /// Delete daily memory rows older than this many days from the DB. Age is measured by `updated_at` (last write time). 0 = keep forever.
    #[serde(default = "default_zero_retention")]
    pub daily_retention_days: u32,
    /// Delete core memory rows older than this many days from the DB. Age is measured by `created_at` (first-write time). Neither recall nor ordinary rewrites refresh `created_at` under the current SQLite upsert, so core retention is an absolute age limit from first write. Set this to a generously large window for durable core memories, or keep 0 = keep forever.
    #[serde(default = "default_zero_retention")]
    pub core_retention_days: u32,
    /// Source of embedding vectors for semantic search. `none` = keyword-only retrieval (no API calls, no vector cost); `openai` = OpenAI's embedding API; `custom:URL` = any OpenAI-compatible embedding endpoint (LiteLLM, local gateway, etc.).
    #[serde(default = "default_embedding_provider")]
    pub embedding_provider: String,
    /// Embedding model identifier — must match a model your chosen embedding model_provider serves (e.g. `text-embedding-3-small` for OpenAI). Changing this invalidates existing embeddings: the change is detected at startup and stale vectors are cleared automatically; run `zeroclaw memory reindex` to re-embed (or set `auto_reindex_on_identity_change`).
    #[serde(default = "default_embedding_model")]
    pub embedding_model: String,
    /// Vector width produced by the embedding model — must match the model's native dimension or vectors won't store correctly. Look up the number on the model_provider's model page.
    #[serde(default = "default_embedding_dims")]
    pub embedding_dimensions: usize,
    /// Automatically re-embed all memories in the background when a change of embedding provider/model/dimensions is detected at startup (after the stale vectors have been cleared). Costs one embedding API call per memory, so it's off by default — leave it off for large stores and run `zeroclaw memory reindex` explicitly instead.
    #[serde(default)]
    pub auto_reindex_on_identity_change: bool,
    /// Optional API key for the embedding endpoint. When set, embedding calls use this key instead of inheriting one from the seed model provider — decoupling embeddings from the chat model. Use it when the chat model runs on a provider that carries no usable embedding credential (e.g. an OAuth-only provider) while embeddings keep hitting an `openai`/`custom:` endpoint with their own key. Leave unset to inherit the seed provider's key (backward-compatible default).
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_api_key: Option<String>,
    /// How heavily vector (semantic) similarity counts when `search_mode = hybrid`. Raise toward 1.0 to favor meaning-based matches; lower it to lean on keyword overlap instead.
    #[serde(default = "default_vector_weight")]
    pub vector_weight: f64,
    /// How heavily BM25 (keyword) overlap counts when `search_mode = hybrid`. Raise toward 1.0 for exact-term matching; lower it when paraphrases should still score well.
    #[serde(default = "default_keyword_weight")]
    pub keyword_weight: f64,
    /// How memories are retrieved: `bm25` = keyword-only (no embeddings, cheapest); `embedding` = vector similarity only (needs an embedding model_provider); `hybrid` = blended keyword + vector score using the weights above (most robust).
    #[serde(default)]
    pub search_mode: SearchMode,
    /// Minimum hybrid score (0.0–1.0) for a memory to be included in context.
    /// Memories scoring below this threshold are dropped to prevent irrelevant
    /// context from bleeding into conversations. Default: 0.4
    #[serde(default = "default_min_relevance_score")]
    pub min_relevance_score: f64,
    /// Max embedding cache entries before LRU eviction
    #[serde(default = "default_cache_size")]
    pub embedding_cache_size: usize,
    /// Max tokens per chunk for document splitting
    #[serde(default = "default_chunk_size")]
    pub chunk_max_tokens: usize,

    // ── Response Cache (saves tokens on repeated prompts) ──────
    /// Enable LLM response caching to avoid paying for duplicate prompts
    #[serde(default)]
    pub response_cache_enabled: bool,
    /// TTL in minutes for cached responses (default: 60)
    #[serde(default = "default_response_cache_ttl")]
    pub response_cache_ttl_minutes: u32,
    /// Max number of cached responses before LRU eviction (default: 5000)
    #[serde(default = "default_response_cache_max")]
    pub response_cache_max_entries: usize,
    /// Max in-memory hot cache entries for the two-tier response cache (default: 256)
    #[serde(default = "default_response_cache_hot_entries")]
    pub response_cache_hot_entries: usize,

    // ── Memory Snapshot (soul backup to Markdown) ─────────────
    /// Enable periodic export of core memories to MEMORY_SNAPSHOT.md
    #[serde(default)]
    pub snapshot_enabled: bool,
    /// Run snapshot during hygiene passes (heartbeat-driven)
    #[serde(default)]
    pub snapshot_on_hygiene: bool,
    /// Auto-hydrate from MEMORY_SNAPSHOT.md when brain.db is missing
    #[serde(default = "default_true")]
    pub auto_hydrate: bool,

    // ── Retrieval Pipeline ─────────────────────────────────────
    /// Retrieval stages for per-agent recall. Only `"cache"` is active: it
    /// enables an in-process, per-handle hot cache over recall results and is
    /// omitted from the default so recall stays coherent across handles.
    /// `"fts"` and `"vector"` are reserved for when the backend exposes
    /// distinct FTS and vector operations; recall is a single hybrid backend
    /// call today, so those names have no effect (kept for forward compat).
    #[serde(default = "default_retrieval_stages")]
    pub retrieval_stages: Vec<String>,
    /// Candidate pool multiplier over the final recall limit before blend/rerank trimming.
    /// Values must be in 1..=20; runtime also enforces a bounded candidate pool.
    #[serde(default = "default_candidate_multiplier")]
    pub candidate_multiplier: usize,
    /// Enable the recall rerank stage: blend retrieval score with importance
    /// and recency, collapse near-duplicate entries, then trim back to the
    /// recall limit. The advanced strategy below runs when the candidate
    /// count reaches `rerank_threshold`.
    #[serde(default)]
    pub rerank_enabled: bool,
    /// Minimum candidate count to trigger the advanced rerank strategy.
    #[serde(default = "default_rerank_threshold")]
    pub rerank_threshold: usize,
    /// Advanced rerank strategy. Valid: "none", "mmr".
    #[serde(default = "default_rerank_strategy")]
    pub rerank_strategy: String,
    /// MMR relevance-vs-diversity weight, where 1.0 means relevance-only.
    #[serde(default = "default_mmr_lambda")]
    pub mmr_lambda: f64,
    /// Importance weight used by the recall blend.
    #[serde(default = "default_importance_weight")]
    pub importance_weight: f64,
    /// Recency weight used by the recall blend.
    #[serde(default = "default_recency_weight")]
    pub recency_weight: f64,
    /// Reserved (0.0-1.0): the FTS score above which recall would skip the
    /// vector stage. Inert until the backend exposes distinct FTS and vector
    /// operations; recall is a single hybrid call today, so this has no effect.
    #[serde(default = "default_fts_early_return_score")]
    pub fts_early_return_score: f64,

    // ── Namespace Isolation ─────────────────────────────────────
    /// Default namespace for memory entries.
    #[serde(default = "default_namespace")]
    pub default_namespace: String,

    // ── Conflict Resolution ─────────────────────────────────────
    /// Cosine similarity threshold for conflict detection (0.0–1.0).
    #[serde(default = "default_conflict_threshold")]
    pub conflict_threshold: f64,
    /// Enable reversible supersede soft-hide machinery when wired.
    #[serde(default = "default_conflict_supersede_enabled")]
    pub conflict_supersede_enabled: bool,
    /// Enable write-time near-duplicate detection.
    #[serde(default)]
    pub dedup_on_write: bool,
    /// Jaccard threshold for text-only duplicate detection.
    #[serde(default = "default_dedup_jaccard_threshold")]
    pub dedup_jaccard_threshold: f64,
    /// Action to take when a duplicate is detected.
    #[serde(default)]
    pub dedup_action: MemoryDedupAction,

    // ── Memory Budget / Pinning ────────────────────────────────
    /// Maximum Core rows before budget compaction. 0 = unbounded.
    #[serde(default)]
    pub core_max_rows: u64,
    /// Maximum Core bytes before budget compaction. 0 = unbounded.
    #[serde(default)]
    pub core_max_bytes: u64,
    /// Maximum Daily rows before budget compaction. 0 = unbounded.
    #[serde(default)]
    pub daily_max_rows: u64,
    /// Eviction ordering for budget compaction.
    #[serde(default)]
    pub evict_order: MemoryEvictOrder,
    /// Namespaces protected from budget eviction.
    #[serde(default)]
    pub pin_namespaces: Vec<String>,
    /// Pin entries at or above this importance. >1.0 means disabled.
    #[serde(default = "default_pin_min_importance")]
    pub pin_min_importance: f64,

    // ── Audit Trail ─────────────────────────────────────────────
    /// Enable audit logging of memory operations.
    #[serde(default)]
    pub audit_enabled: bool,
    /// Retention period for audit entries in days (default: 30).
    #[serde(default = "default_audit_retention_days")]
    pub audit_retention_days: u32,

    // ── Policy Engine ───────────────────────────────────────────
    /// Memory policy configuration.
    #[serde(default)]
    #[nested]
    pub policy: MemoryPolicyConfig,

    /// Typed memory configuration (`[memory.types]` section).
    #[serde(default)]
    #[nested]
    pub types: MemoryTypesConfig,
    // Backend-specific config fields (sqlite_open_timeout_secs, qdrant.*,
    // postgres.*) live on `[storage.<backend>.<alias>]`. The `backend` field
    // carries a dotted alias reference and the runtime looks up the typed
    // config via `Config::resolve_active_storage`.
}

/// Typed memory configuration (`[memory.types]` section).
///
/// Behaviour-neutral by default: `enabled` gates MemoryKind assignment on new
/// consolidation writes and defaults off; the flip is sequenced in a later
/// phase. SQLite-only: enabling requires the sqlite memory backend globally
/// and on every agent (validated at config load).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "memory.types"]
pub struct MemoryTypesConfig {
    /// Assign a first-class MemoryKind to new consolidation writes.
    #[serde(default)]
    pub enabled: bool,
}

/// Memory policy configuration (`[memory.policy]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "memory.policy"]
pub struct MemoryPolicyConfig {
    /// Maximum entries per namespace (0 = unlimited).
    #[serde(default)]
    pub max_entries_per_namespace: usize,
    /// Maximum entries per category (0 = unlimited).
    #[serde(default)]
    pub max_entries_per_category: usize,
    /// Retention days by category (overrides global). Keys: "core", "daily", "conversation".
    #[serde(default)]
    pub retention_days_by_category: std::collections::HashMap<String, u32>,
    /// Namespaces that are read-only (writes are rejected).
    #[serde(default)]
    pub read_only_namespaces: Vec<String>,
    /// Content scan mode for durable memory writes: "off", "on", or "strict".
    #[serde(default = "default_memory_threat_scan")]
    pub threat_scan: String,
    /// Re-scan stored entries at recall/read time and withhold flagged entries.
    #[serde(default = "default_true")]
    pub threat_scan_load_time: bool,
    /// Behavior when a write-time content scan matches: "reject" or
    /// "block-on-read".
    #[serde(default = "default_memory_threat_scan_on_hit")]
    pub threat_scan_on_hit: String,
    /// Redact configured secret/PII categories before persistence.
    #[serde(default)]
    pub redact_on_write: bool,
    /// Redaction categories applied when `redact_on_write` is true.
    #[serde(default = "default_memory_redact_categories")]
    pub redact_categories: Vec<String>,
}

impl Default for MemoryPolicyConfig {
    fn default() -> Self {
        Self {
            max_entries_per_namespace: 0,
            max_entries_per_category: 0,
            retention_days_by_category: std::collections::HashMap::new(),
            read_only_namespaces: Vec::new(),
            threat_scan: default_memory_threat_scan(),
            threat_scan_load_time: true,
            threat_scan_on_hit: default_memory_threat_scan_on_hit(),
            redact_on_write: false,
            redact_categories: default_memory_redact_categories(),
        }
    }
}

fn default_memory_threat_scan() -> String {
    "on".into()
}

fn default_memory_threat_scan_on_hit() -> String {
    "reject".into()
}

fn default_memory_redact_categories() -> Vec<String> {
    ["secret", "api_key", "private_key", "email", "phone"]
        .into_iter()
        .map(String::from)
        .collect()
}

fn default_retrieval_stages() -> Vec<String> {
    // No "cache": the hot cache is opt-in so activating the retrieval decorator
    // does not change default per-agent recall. "fts"/"vector" are reserved and
    // inert (recall is a single hybrid backend call today).
    vec!["fts".into(), "vector".into()]
}
fn default_rerank_threshold() -> usize {
    5
}

/// Largest allowed multiplier for the bounded query-time rerank candidate pool.
pub const MAX_MEMORY_RERANK_CANDIDATE_MULTIPLIER: usize = 20;

fn default_candidate_multiplier() -> usize {
    4
}
fn default_rerank_strategy() -> String {
    "none".into()
}
fn default_mmr_lambda() -> f64 {
    0.7
}
fn default_importance_weight() -> f64 {
    0.2
}
fn default_recency_weight() -> f64 {
    0.1
}
fn default_fts_early_return_score() -> f64 {
    0.85
}

fn validate_memory_rerank_config(memory: &MemoryConfig) -> Result<()> {
    if !(1..=MAX_MEMORY_RERANK_CANDIDATE_MULTIPLIER).contains(&memory.candidate_multiplier) {
        anyhow::bail!(
            "memory.candidate_multiplier must be in 1..={MAX_MEMORY_RERANK_CANDIDATE_MULTIPLIER} (got {})",
            memory.candidate_multiplier
        );
    }

    for (path, value) in [
        ("memory.min_relevance_score", memory.min_relevance_score),
        ("memory.mmr_lambda", memory.mmr_lambda),
        ("memory.importance_weight", memory.importance_weight),
        ("memory.recency_weight", memory.recency_weight),
    ] {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            anyhow::bail!("{path} must be a finite number in 0.0..=1.0 (got {value})");
        }
    }

    Ok(())
}
fn default_namespace() -> String {
    "default".into()
}
fn default_conflict_threshold() -> f64 {
    0.85
}
fn default_conflict_supersede_enabled() -> bool {
    true
}
fn default_dedup_jaccard_threshold() -> f64 {
    0.80
}
fn default_pin_min_importance() -> f64 {
    1.01
}

/// Surface `[memory]` knobs that the schema accepts but that have no runtime
/// consumer yet, so an operator who sets them learns they currently have no
/// effect instead of debugging missing behavior (same class of check as the
/// `wire_api_not_supported_for_family` warning).
///
/// A PR that wires a consumer for one of these knobs must drop its check
/// here in the same change; this list mirrors what is inert on the current
/// tree, not what is planned.
///
/// Called from `Config::collect_warnings`, so each warning reaches both the
/// CLI (via `validate()`'s tracing emission) and the gateway dashboard.
pub fn validate_memory_semantics(
    memory: &MemoryConfig,
) -> Vec<crate::validation_warnings::ValidationWarning> {
    let mut inert: Vec<(&'static str, &'static str)> = Vec::new();

    // The staged retrieval pipeline (`RetrievalPipeline`) exists but is not
    // wired into the production recall path, so its tuning knobs are inert.
    if memory.retrieval_stages != default_retrieval_stages() {
        inert.push((
            "memory.retrieval_stages",
            "the staged retrieval pipeline is not wired into the recall path yet",
        ));
    }
    if (memory.fts_early_return_score - default_fts_early_return_score()).abs() > f64::EPSILON {
        inert.push((
            "memory.fts_early_return_score",
            "the staged retrieval pipeline is not wired into the recall path yet",
        ));
    }

    // The proposed rerank stage was never landed, so these knobs remain inert.
    // `DefaultMemoryStrategy::new` logs the same fact at agent start; this
    // config-time copy reaches `config validate` and dashboard callers too.
    if memory.rerank_enabled {
        inert.push((
            "memory.rerank_enabled",
            "the rerank stage is not yet implemented",
        ));
    }
    if memory.rerank_threshold != default_rerank_threshold() {
        inert.push((
            "memory.rerank_threshold",
            "the rerank stage is not yet implemented",
        ));
    }

    inert
        .into_iter()
        .map(|(path, reason)| {
            crate::validation_warnings::ValidationWarning::new(
                "memory_config_knob_inert",
                format!("{path} is set but {reason}; this setting currently has no effect"),
                path,
            )
        })
        .collect()
}

/// Write-time duplicate handling policy for memory entries.
#[derive(
    Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MemoryDedupAction {
    /// Reject an incoming near-duplicate.
    #[default]
    Reject,
    /// Merge an incoming near-duplicate into a survivor.
    Merge,
}

/// Memory budget eviction order.
#[derive(
    Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MemoryEvictOrder {
    /// Evict the lowest-value rows first.
    #[default]
    Value,
    /// Evict the oldest rows first.
    Oldest,
}

fn default_audit_retention_days() -> u32 {
    30
}

fn default_pgvector_dimensions() -> usize {
    1536
}

fn default_embedding_provider() -> String {
    "none".into()
}
fn default_auto_save() -> bool {
    true
}
fn default_hygiene_enabled() -> bool {
    true
}
fn default_archive_after_days() -> u32 {
    7
}
fn default_purge_after_days() -> u32 {
    30
}
fn default_conversation_retention_days() -> u32 {
    30
}
fn default_zero_retention() -> u32 {
    0
}
fn default_embedding_model() -> String {
    "text-embedding-3-small".into()
}
fn default_embedding_dims() -> usize {
    1536
}
fn default_vector_weight() -> f64 {
    0.7
}
fn default_keyword_weight() -> f64 {
    0.3
}
fn default_min_relevance_score() -> f64 {
    0.4
}
fn default_cache_size() -> usize {
    10_000
}
fn default_chunk_size() -> usize {
    512
}
fn default_response_cache_ttl() -> u32 {
    60
}
fn default_response_cache_max() -> usize {
    5_000
}

fn default_response_cache_hot_entries() -> usize {
    256
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            backend: "sqlite".into(),
            auto_save: true,
            hygiene_enabled: default_hygiene_enabled(),
            consolidation_extract_facts: false,
            archive_after_days: default_archive_after_days(),
            purge_after_days: default_purge_after_days(),
            conversation_retention_days: default_conversation_retention_days(),
            daily_retention_days: default_zero_retention(),
            core_retention_days: default_zero_retention(),
            embedding_provider: default_embedding_provider(),
            embedding_model: default_embedding_model(),
            embedding_dimensions: default_embedding_dims(),
            auto_reindex_on_identity_change: false,
            embedding_api_key: None,
            vector_weight: default_vector_weight(),
            keyword_weight: default_keyword_weight(),
            search_mode: SearchMode::default(),
            min_relevance_score: default_min_relevance_score(),
            embedding_cache_size: default_cache_size(),
            chunk_max_tokens: default_chunk_size(),
            response_cache_enabled: false,
            response_cache_ttl_minutes: default_response_cache_ttl(),
            response_cache_max_entries: default_response_cache_max(),
            response_cache_hot_entries: default_response_cache_hot_entries(),
            snapshot_enabled: false,
            snapshot_on_hygiene: false,
            auto_hydrate: true,
            retrieval_stages: default_retrieval_stages(),
            candidate_multiplier: default_candidate_multiplier(),
            rerank_enabled: false,
            rerank_threshold: default_rerank_threshold(),
            rerank_strategy: default_rerank_strategy(),
            mmr_lambda: default_mmr_lambda(),
            importance_weight: default_importance_weight(),
            recency_weight: default_recency_weight(),
            fts_early_return_score: default_fts_early_return_score(),
            default_namespace: default_namespace(),
            conflict_threshold: default_conflict_threshold(),
            conflict_supersede_enabled: default_conflict_supersede_enabled(),
            dedup_on_write: false,
            dedup_jaccard_threshold: default_dedup_jaccard_threshold(),
            dedup_action: MemoryDedupAction::default(),
            core_max_rows: 0,
            core_max_bytes: 0,
            daily_max_rows: 0,
            evict_order: MemoryEvictOrder::default(),
            pin_namespaces: Vec::new(),
            pin_min_importance: default_pin_min_importance(),
            audit_enabled: false,
            audit_retention_days: default_audit_retention_days(),
            policy: MemoryPolicyConfig::default(),
            types: MemoryTypesConfig::default(),
        }
    }
}

// ── Observability ─────────────────────────────────────────────────

/// Observability sink backend.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ObservabilityBackend {
    #[default]
    #[serde(alias = "noop")]
    None,
    Log,
    Verbose,
    Prometheus,
    #[serde(alias = "opentelemetry", alias = "otlp")]
    Otel,
}

impl ObservabilityBackend {
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Log => "log",
            Self::Verbose => "verbose",
            Self::Prometheus => "prometheus",
            Self::Otel => "otel",
        }
    }
}

/// JSONL log persistence mode.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LogPersistence {
    None,
    #[default]
    Rolling,
    Full,
    /// Persist all events, but with ZeroClaw-managed archive rotation: the
    /// active file is rotated to a timestamped archive on a size and/or daily
    /// boundary, and old archives are pruned by count and/or age. Unlike
    /// `rolling` (entry-count trim of the active file), rotated events are
    /// preserved in archive files for later diagnostics. Rotation is tuned via
    /// the `log_persistence_max_bytes`, `log_persistence_rotate_daily`,
    /// `log_persistence_retention_max_files`, and
    /// `log_persistence_retention_max_age_days` settings.
    Rotating,
}

impl LogPersistence {
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Rolling => "rolling",
            Self::Full => "full",
            Self::Rotating => "rotating",
        }
    }
}

/// Tool I/O capture policy.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LogToolIo {
    Off,
    #[default]
    Redacted,
    Full,
}

impl LogToolIo {
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Redacted => "redacted",
            Self::Full => "full",
        }
    }
}

/// LLM request payload capture policy. Mirrors [`LogToolIo`] but gates the
/// prompt/messages sent on each `llm_request`. Defaults to `Off` because the
/// full payload is the entire system prompt plus conversation every turn.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LogLlmRequestPayload {
    #[default]
    Off,
    Redacted,
    Full,
}

impl LogLlmRequestPayload {
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Redacted => "redacted",
            Self::Full => "full",
        }
    }
}

/// OTel content capture policy. Mirrors [`LogToolIo`] but gates OTel span
/// attribute emission, not log persistence. Defaults to `Off` for privacy.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OtelContentPolicy {
    #[default]
    Off,
    Redacted,
    Full,
}

impl OtelContentPolicy {
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Redacted => "redacted",
            Self::Full => "full",
        }
    }
}

/// Observability backend configuration (`[observability]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "observability"]
pub struct ObservabilityConfig {
    /// Observability sink: none | log | verbose | prometheus | otel.
    #[serde(default, deserialize_with = "deserialize_enum_lenient")]
    pub backend: ObservabilityBackend,

    /// OTLP endpoint (e.g. `"http://localhost:4318"`). Only used when backend = `"otel"`.
    #[serde(default)]
    pub otel_endpoint: Option<String>,

    /// Service name reported to the OTel collector. Defaults to "zeroclaw".
    #[serde(default)]
    pub otel_service_name: Option<String>,

    /// Optional HTTP headers sent with every OTLP export request (e.g. authorization).
    /// Specified as key-value pairs in TOML:
    /// ```toml
    /// [observability.otel_headers]
    /// Authorization = "Bearer sk-..."
    /// ```
    #[serde(default)]
    #[secret]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub otel_headers: Option<std::collections::HashMap<String, String>>,

    /// Log persistence mode: "none" | "rolling" | "full".
    /// Controls whether every event passing through `zeroclaw_log::record!`
    /// is appended to the on-disk JSONL log.
    #[serde(
        default = "default_log_persistence",
        alias = "runtime_trace_mode",
        deserialize_with = "deserialize_enum_lenient"
    )]
    pub log_persistence: LogPersistence,

    /// Log persistence file path. Relative paths resolve under workspace_dir.
    #[serde(default = "default_log_persistence_path", alias = "runtime_trace_path")]
    pub log_persistence_path: String,

    /// Maximum entries retained when `log_persistence = "rolling"`.
    #[serde(
        default = "default_log_persistence_max_entries",
        alias = "runtime_trace_max_entries"
    )]
    pub log_persistence_max_entries: usize,

    /// Size threshold in bytes that triggers an archive rotation when
    /// `log_persistence = "rotating"`. The active file is rotated once an
    /// append leaves it at or above this size. `0` disables size-based
    /// rotation. Ignored unless `log_persistence = "rotating"`.
    #[serde(default = "default_log_persistence_max_bytes")]
    pub log_persistence_max_bytes: u64,

    /// Rotate the active file to an archive on a UTC day boundary when
    /// `log_persistence = "rotating"`: before the first event of a new day is
    /// appended, a file whose last write fell on an earlier day is archived.
    /// Ignored unless `log_persistence = "rotating"`.
    #[serde(default = "default_log_persistence_rotate_daily")]
    pub log_persistence_rotate_daily: bool,

    /// Retention cap on the number of rotated archive files kept alongside the
    /// active file when `log_persistence = "rotating"`. After a rotation the
    /// oldest archives beyond this count are deleted. `0` keeps all archives.
    /// Ignored unless `log_persistence = "rotating"`.
    #[serde(default = "default_log_persistence_retention_max_files")]
    pub log_persistence_retention_max_files: usize,

    /// Retention cap on the age (in days) of rotated archive files when
    /// `log_persistence = "rotating"`. After a rotation, archives older than
    /// this many days are deleted. `0` disables age-based cleanup. Ignored
    /// unless `log_persistence = "rotating"`.
    #[serde(default = "default_log_persistence_retention_max_age_days")]
    pub log_persistence_retention_max_age_days: u64,

    /// Tool I/O capture policy: "off" | "redacted" | "full".
    /// - `off`: only tool name + outcome + duration land in the log.
    /// - `redacted` (default): tool input + output are leak-scanned and
    ///   truncated at `log_tool_io_truncate_bytes` before persisting.
    /// - `full`: full input + output, still leak-scanned. For operators
    ///   who need replay fidelity and accept the disk cost.
    #[serde(
        default = "default_log_tool_io",
        deserialize_with = "deserialize_enum_lenient"
    )]
    pub log_tool_io: LogToolIo,

    /// Truncate the captured tool input and output at this many bytes when
    /// `log_tool_io = "redacted"`. Truncated events carry an explicit
    /// `tool_output_truncated: true` flag plus `tool_output_original_bytes`.
    #[serde(default = "default_log_tool_io_truncate_bytes")]
    pub log_tool_io_truncate_bytes: usize,

    /// Tool names whose I/O is never logged beyond name + outcome + duration
    /// regardless of `log_tool_io`. Use for tools whose I/O is intrinsically
    /// sensitive (e.g. memory recall against personal namespaces, agent
    /// secret reads). Empty by default.
    #[serde(default)]
    pub log_tool_io_denylist: Vec<String>,

    /// LLM request payload capture: "off" | "redacted" | "full".
    /// - `off` (default): only `messages_count` lands on the `llm_request`
    ///   event. No prompt or conversation content is persisted.
    /// - `redacted`: the full message history (role + content) is leak-scanned
    ///   and truncated at `log_tool_io_truncate_bytes` before persisting.
    /// - `full`: full message history, still leak-scanned, untruncated. For
    ///   operators who need replay fidelity and accept the disk cost.
    ///
    /// Opt-in (default off) because `full`/`redacted` capture the entire system
    /// prompt and conversation on every turn.
    #[serde(
        default = "default_log_llm_request_payload",
        deserialize_with = "deserialize_enum_lenient"
    )]
    pub log_llm_request_payload: LogLlmRequestPayload,

    /// OTel GenAI content capture: "off" | "redacted" | "full".
    /// Controls whether `gen_ai.system_instructions`, `gen_ai.input.messages`,
    /// and `gen_ai.output.messages` are emitted on OTel spans.
    /// - `off` (default): no content attributes, only metadata.
    /// - `redacted`: content is leak-scanned and truncated at `otel_genai_content_max_chars`.
    /// - `full`: content is leak-scanned but not truncated.
    #[serde(default, deserialize_with = "deserialize_enum_lenient")]
    pub otel_genai_content: OtelContentPolicy,

    /// Per-field character truncation limit for OTel GenAI content when
    /// `otel_genai_content = "redacted"`. Each string field is truncated
    /// independently. `0` is treated as `off`.
    #[serde(default = "default_otel_genai_content_max_chars")]
    pub otel_genai_content_max_chars: usize,

    /// OTel tool I/O capture: "off" | "redacted" | "full".
    /// Controls whether `gen_ai.tool.arguments`, `input.value`,
    /// `gen_ai.tool.result`, and `output.value` are emitted on OTel spans.
    /// - `off` (default): no content attributes, only tool name + outcome.
    /// - `redacted`: content is leak-scanned and truncated at `otel_tool_io_max_chars`.
    /// - `full`: content is leak-scanned but not truncated.
    #[serde(default, deserialize_with = "deserialize_enum_lenient")]
    pub otel_tool_io: OtelContentPolicy,

    /// Per-field character truncation limit for OTel tool I/O when
    /// `otel_tool_io = "redacted"`. Each string field is truncated
    /// independently. `0` is treated as `off`.
    #[serde(default = "default_otel_tool_io_max_chars")]
    pub otel_tool_io_max_chars: usize,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            backend: ObservabilityBackend::None,
            otel_endpoint: None,
            otel_service_name: None,
            otel_headers: None,
            log_persistence: default_log_persistence(),
            log_persistence_path: default_log_persistence_path(),
            log_persistence_max_entries: default_log_persistence_max_entries(),
            log_persistence_max_bytes: default_log_persistence_max_bytes(),
            log_persistence_rotate_daily: default_log_persistence_rotate_daily(),
            log_persistence_retention_max_files: default_log_persistence_retention_max_files(),
            log_persistence_retention_max_age_days: default_log_persistence_retention_max_age_days(
            ),
            log_tool_io: default_log_tool_io(),
            log_tool_io_truncate_bytes: default_log_tool_io_truncate_bytes(),
            log_tool_io_denylist: Vec::new(),
            log_llm_request_payload: default_log_llm_request_payload(),
            otel_genai_content: OtelContentPolicy::Off,
            otel_genai_content_max_chars: default_otel_genai_content_max_chars(),
            otel_tool_io: OtelContentPolicy::Off,
            otel_tool_io_max_chars: default_otel_tool_io_max_chars(),
        }
    }
}

fn default_log_persistence() -> LogPersistence {
    LogPersistence::Rolling
}

fn default_log_persistence_path() -> String {
    "state/runtime-trace.jsonl".to_string()
}

fn default_log_persistence_max_entries() -> usize {
    200
}

/// Size rotation off by default; operators opt in with an explicit byte budget.
fn default_log_persistence_max_bytes() -> u64 {
    0
}

/// Daily rotation is on by default so `rotating` mode produces day-scoped
/// archives out of the box even without a configured size budget.
fn default_log_persistence_rotate_daily() -> bool {
    true
}

/// Keep a week of rotated archives by default.
fn default_log_persistence_retention_max_files() -> usize {
    7
}

/// Age-based cleanup off by default; count-based retention governs unless set.
fn default_log_persistence_retention_max_age_days() -> u64 {
    0
}

fn default_log_tool_io() -> LogToolIo {
    LogToolIo::Redacted
}

fn default_log_llm_request_payload() -> LogLlmRequestPayload {
    LogLlmRequestPayload::Off
}

fn default_log_tool_io_truncate_bytes() -> usize {
    40960
}

fn default_otel_genai_content_max_chars() -> usize {
    1000
}

fn default_otel_tool_io_max_chars() -> usize {
    1000
}

// ── Hooks ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "hooks"]
pub struct HooksConfig {
    /// Enable lifecycle hook execution.
    ///
    /// Hooks run in-process with the same privileges as the main runtime.
    /// Keep enabled hook handlers narrowly scoped and auditable.
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    #[nested]
    pub builtin: BuiltinHooksConfig,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            builtin: BuiltinHooksConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "hooks.builtin"]
pub struct BuiltinHooksConfig {
    /// Enable the command-logger hook (logs tool calls for auditing).
    #[serde(default)]
    pub command_logger: bool,
    /// Configuration for the webhook-audit hook.
    ///
    /// When enabled, POSTs a JSON payload to `url` for every tool invocation
    /// that matches one of `tool_patterns`.
    #[serde(default)]
    #[nested]
    pub webhook_audit: WebhookAuditConfig,
}

/// Configuration for the webhook-audit builtin hook.
///
/// Sends an HTTP POST with a JSON body to an external endpoint each time
/// a tool call matches one of the configured patterns. Useful for
/// centralised audit logging, SIEM ingestion, or compliance pipelines.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "hooks.builtin.webhook_audit"]
pub struct WebhookAuditConfig {
    /// Enable the webhook-audit hook. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Target URL that will receive the audit POST requests.
    #[serde(default)]
    pub url: String,
    /// Glob patterns for tool names to audit (e.g. `["Bash", "Write"]`).
    /// An empty list means **no** tools are audited.
    #[serde(default)]
    pub tool_patterns: Vec<String>,
    /// Include tool call arguments in the audit payload. Default: `false`.
    ///
    /// Be mindful of sensitive data — arguments may contain secrets or PII.
    #[serde(default)]
    pub include_args: bool,
    /// Maximum size (in bytes) of serialised arguments included in a single
    /// audit payload. Arguments exceeding this limit are truncated.
    /// Default: `4096`.
    #[serde(default = "default_max_args_bytes")]
    pub max_args_bytes: u64,
}

fn default_max_args_bytes() -> u64 {
    4096
}

impl Default for WebhookAuditConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            tool_patterns: Vec::new(),
            include_args: false,
            max_args_bytes: default_max_args_bytes(),
        }
    }
}

// ── Autonomy / Security ──────────────────────────────────────────
//
// All policy fields live on per-agent `[risk_profiles.<alias>]` entries
// (see `RiskProfileConfig` below). `Config::active_risk_profile(agent_alias)`
// resolves the active profile for any callsite (agent-driven or non-agent
// contexts). Configs from older schema versions are folded into
// `risk_profiles.default` by the migration in `schema/v2.rs`.

pub fn default_auto_approve() -> Vec<String> {
    vec![
        "file_read".into(),
        "memory_recall".into(),
        "web_search_tool".into(),
        "web_fetch".into(),
        "calculator".into(),
        "glob_search".into(),
        "content_search".into(),
        "image_info".into(),
        "weather".into(),
        "tool_search".into(),
        "browser".into(),
        "browser_open".into(),
    ]
}

fn default_always_ask() -> Vec<String> {
    vec![]
}

impl RiskProfileConfig {
    /// Merge the built-in default `auto_approve` entries into the current
    /// list, preserving any user-supplied additions.
    pub fn ensure_default_auto_approve(&mut self) {
        let defaults = default_auto_approve();
        for entry in defaults {
            if !self.auto_approve.iter().any(|existing| existing == &entry) {
                self.auto_approve.push(entry);
            }
        }
    }

    /// Synthesize a [`SandboxConfig`] from this profile's flattened sandbox
    /// fields. Sandbox config is stored flat on the profile; callsites that
    /// still want a `SandboxConfig` instance (sandbox detection in
    /// `zeroclaw-runtime::security::detect`) can call this helper.
    #[must_use]
    pub fn sandbox_config(&self) -> SandboxConfig {
        let backend = self
            .sandbox_backend
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(parse_sandbox_backend)
            .unwrap_or_default();
        SandboxConfig {
            enabled: self.sandbox_enabled,
            backend,
            firejail_args: self.firejail_args.clone(),
        }
    }
}

fn parse_sandbox_backend(name: &str) -> SandboxBackend {
    match name.to_ascii_lowercase().as_str() {
        "auto" => SandboxBackend::Auto,
        "landlock" => SandboxBackend::Landlock,
        "firejail" => SandboxBackend::Firejail,
        "bubblewrap" => SandboxBackend::Bubblewrap,
        "docker" => SandboxBackend::Docker,
        "sandbox-exec" | "sandboxexec" | "seatbelt" => SandboxBackend::SandboxExec,
        "none" => SandboxBackend::None,
        _ => SandboxBackend::default(),
    }
}

fn is_valid_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

// ── Profiles & Bundles ───────────────────────────────────────────

/// Named risk/autonomy profile (`[risk_profiles.<alias>]`).
///
/// Unified policy surface. Agents reference a profile by alias and the
/// runtime resolves through it for shell command allowlists, approval gates,
/// sandbox/resource limits, and delegation guardrails. The conventional
/// `risk_profiles["default"]` is the resolution target for non-agent
/// contexts (orchestrator init, cron worker startup); the `Default` impl
/// below mirrors the legacy safety-first defaults so a fresh install
/// behaves the same as a config from before the per-profile split.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "risk_profile"]
#[serde(default)]
pub struct RiskProfileConfig {
    /// Autonomy level applied to this profile. Default: `supervised`.
    pub level: AutonomyLevel,
    /// Restrict filesystem access to workspace-relative paths. Default: `false`.
    pub workspace_only: bool,
    /// Allowlist of executable names for shell execution.
    pub allowed_commands: Vec<String>,
    /// Explicit path denylist.
    pub forbidden_paths: Vec<String>,
    /// Require approval for medium-risk operations.
    pub require_approval_for_medium_risk: bool,
    /// Block high-risk commands even when allowlisted.
    pub block_high_risk_commands: bool,
    /// Environment variable names passed through to shell subprocesses.
    #[credential_class = "legacy_env_path"]
    pub shell_env_passthrough: Vec<String>,
    /// Tools that never require approval in this profile.
    pub auto_approve: Vec<String>,
    /// Tools that always require approval in this profile.
    pub always_ask: Vec<String>,
    /// Extra directory roots the agent may access.
    #[serde(alias = "allowed_path", alias = "allowed_paths")]
    pub allowed_roots: Vec<String>,
    /// Route this profile's tool approvals to a DISTINCT approver channel instead of the
    /// channel that triggered the run (closes the cross-channel-HITL gap). Absent ⇒ the
    /// originating channel approves (today's behavior). See [`crate::autonomy::ApprovalRoute`].
    ///
    /// Honored on both the interactive channel-driven path and the non-interactive turn path
    /// (gateway chat/webhook dispatch and agent-to-agent peer messages). On the non-interactive
    /// path the approver is resolved from the live daemon channel registry; if no registry is
    /// available or the approver is not live, the gate keeps the non-interactive default
    /// (fail-closed deny under the default `on_no_approver`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_route: Option<crate::autonomy::ApprovalRoute>,
    /// Tools the agent may call in agentic mode. `None` = inherit / no
    /// authorization constraint; `Some(vec![])` = deny every tool;
    /// `Some(list)` = allow only the listed names. Authorization decision:
    /// which tools is the agent permitted to invoke at all. See
    /// `excluded_tools` for the inverse denylist scoped to non-CLI channels.
    ///
    /// MCP exception: when the list is non-empty, runtime-discovered MCP
    /// tools (any name containing `__`, which is the `<server>__<tool>`
    /// convention used by the MCP wrapper) are auto-admitted into the
    /// effective allow-list without needing to be listed here individually.
    /// This keeps the post-change eager-MCP default usable for agents with an
    /// explicit allow-list. Block individual MCP tools via `excluded_tools`.
    ///
    /// Scope of the exception: the `__` auto-admit applies only to this
    /// risk-profile allow-list, **not** to caller-supplied per-run
    /// `allowed_tools` (cron job `allowed_tools`, narrowed delegate
    /// invocations, etc.). Per-run lists are still strict explicit-list
    /// intersections, so a job that narrows `allowed_tools = ["cron_add"]`
    /// will not see runtime-discovered MCP tools unless it names them: a
    /// narrowed list is an explicit statement of intent, and silently
    /// widening it would defeat the narrowing.
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// Tools excluded from non-CLI channels under this profile.
    ///
    /// Also subtracts from the agentic-delegate allow-list resolved at
    /// runtime, which is the only way to block individual
    /// `<server>__<tool>` MCP names that would otherwise be auto-admitted
    /// by the `allowed_tools` MCP exception described above.
    pub excluded_tools: Vec<String>,
    /// Whether a non-empty `allowed_tools` auto-admits runtime-discovered
    /// MCP tools. Defaults to
    /// [`McpDiscoveredToolPolicy::ExplicitOnly`][crate::autonomy::McpDiscoveredToolPolicy::ExplicitOnly]:
    /// an MCP tool must be named like any other. Set `auto_admit` to restore
    /// the permissive behavior described on `allowed_tools` above.
    #[serde(default)]
    pub mcp_discovered_tool_policy: crate::autonomy::McpDiscoveredToolPolicy,
    // ── Sandbox (from security.sandbox) ─────────────────────────────
    /// Whether the sandbox is enabled for this profile. `None` inherits global.
    pub sandbox_enabled: Option<bool>,
    /// Sandbox backend identifier (e.g. `"firejail"`, `"landlock"`). `None` inherits.
    pub sandbox_backend: Option<String>,
    /// Extra arguments forwarded to firejail when sandbox_backend = "firejail".
    pub firejail_args: Vec<String>,
}

impl Default for RiskProfileConfig {
    fn default() -> Self {
        Self {
            level: AutonomyLevel::Supervised,
            workspace_only: true,
            allowed_commands: crate::policy::default_allowed_commands(),
            forbidden_paths: crate::policy::default_forbidden_paths(),
            require_approval_for_medium_risk: true,
            block_high_risk_commands: true,
            shell_env_passthrough: vec![],
            auto_approve: default_auto_approve(),
            always_ask: default_always_ask(),
            allowed_roots: Vec::new(),
            approval_route: None,
            allowed_tools: None,
            excluded_tools: Vec::new(),
            mcp_discovered_tool_policy: crate::autonomy::McpDiscoveredToolPolicy::default(),
            sandbox_enabled: None,
            sandbox_backend: None,
            firejail_args: Vec::new(),
        }
    }
}

/// Named runtime/LLM execution profile (`[runtime_profiles.<alias>]`).
///
/// Reusable operational tuning: agentic mode, iteration caps, context
/// budget, parallel dispatch, resource ceilings, and the budget knobs
/// that `SecurityPolicy` enforces with subagent parent-subset
/// discipline. (The spawn-depth knob retired with the spawn wall:
/// local recursion is the fixed D1 law, and the coordinator child host
/// that had its own admission cap was deleted with the control-plane
/// migration wall.) Anything authorization-shaped (allowed
/// commands/tools/paths, approval gates, sandbox) lives on
/// `[risk_profiles.<alias>]`. Anything model-provider shaped (model,
/// temperature, max_tokens, timeout_secs) lives on
/// `[providers.models.<type>.<alias>]`.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "runtime_profile"]
#[serde(default)]
pub struct RuntimeProfileConfig {
    /// Enable agentic (multi-turn tool-call loop) mode.
    pub agentic: bool,
    /// Maximum tool-call iterations in agentic mode. `0` inherits the global default.
    pub max_tool_iterations: usize,
    // ── Budget caps (enforced with subagent parent-subset discipline) ──
    /// Maximum actions allowed per hour. `0` is a hard zero budget — the
    /// per-sender rate tracker treats a max of 0 as always exhausted
    /// (`PerSenderTracker::is_exhausted`), blocking every action. For an
    /// effectively-unlimited budget use a high value (e.g. `u32::MAX`), not 0.
    /// `SecurityPolicy::ensure_no_escalation_beyond` rejects subagents
    /// that try to raise this above the parent's value.
    pub max_actions_per_hour: u32,
    /// Maximum cost per day in cents. `0` inherits the global limit.
    /// Parent-subset enforced for subagents.
    pub max_cost_per_day_cents: u32,
    /// Shell subprocess timeout in seconds. `0` inherits the global timeout.
    /// Parent-subset enforced for subagents.
    pub shell_timeout_secs: u64,
    // ── Per-agent runtime tunables (also live on AliasedAgentConfig) ─
    /// Maximum conversation history messages retained per session. `None` inherits.
    pub max_history_messages: Option<usize>,
    /// Maximum estimated tokens for context before compaction. `None` inherits.
    pub max_context_tokens: Option<usize>,
    /// Use compact bootstrap (6000 chars / 2 RAG chunks). `None` inherits.
    pub compact_context: Option<bool>,
    /// Enable parallel tool execution per iteration. `None` inherits.
    pub parallel_tools: Option<bool>,
    /// Tool dispatch strategy (e.g. `"auto"`). `None` inherits.
    pub tool_dispatcher: Option<String>,
    /// Tools exempt from within-turn dedup check.
    pub tool_call_dedup_exempt: Vec<String>,
    /// Maximum characters for the assembled system prompt. `None` inherits.
    pub max_system_prompt_chars: Option<usize>,
    /// Maximum characters for a single tool result. `None` inherits.
    pub max_tool_result_chars: Option<usize>,
    /// Number of recent turns whose full tool context is preserved. `None` inherits.
    pub keep_tool_context_turns: Option<usize>,
    /// Maximum memory entries injected per turn. `None` inherits global default (5).
    /// Set to `0` for unlimited.
    pub memory_recall_limit: Option<usize>,
    /// How skills are injected into the system prompt for agents on this
    /// profile. `None` inherits the global `[skills] prompt_injection_mode`;
    /// `compact` inlines only compact skill metadata and registers the
    /// `read_skill` tool, `full` inlines full skill instructions. Resolved
    /// through [`Config::effective_skills_prompt_mode`], the single point both
    /// the system-prompt builder and the `read_skill` tool-registration gate
    /// consult so they always agree on the effective mode.
    pub prompt_injection_mode: Option<SkillsPromptInjectionMode>,
    pub strict_tool_parsing: bool,
    #[nested]
    pub thinking: crate::scattered_types::ThinkingConfig,
    #[nested]
    pub history_pruning: crate::scattered_types::HistoryPrunerConfig,
    #[nested]
    pub eval: crate::scattered_types::EvalConfig,
    #[nested]
    pub auto_classify: Option<crate::scattered_types::AutoClassifyConfig>,
    #[nested]
    pub context_compression: crate::scattered_types::ContextCompressionConfig,
    #[nested]
    pub tool_receipts: ToolReceiptsConfig,
    pub tool_filter_groups: Vec<ToolFilterGroup>,
}

impl Default for RuntimeProfileConfig {
    fn default() -> Self {
        Self {
            agentic: false,
            max_tool_iterations: 0,
            max_actions_per_hour: 20,
            max_cost_per_day_cents: 500,
            shell_timeout_secs: 60,
            max_history_messages: None,
            max_context_tokens: None,
            compact_context: None,
            parallel_tools: None,
            tool_dispatcher: None,
            tool_call_dedup_exempt: Vec::new(),
            max_system_prompt_chars: None,
            max_tool_result_chars: None,
            keep_tool_context_turns: None,
            memory_recall_limit: None,
            prompt_injection_mode: None,
            strict_tool_parsing: false,
            thinking: crate::scattered_types::ThinkingConfig::default(),
            history_pruning: crate::scattered_types::HistoryPrunerConfig::default(),
            eval: crate::scattered_types::EvalConfig::default(),
            auto_classify: None,
            context_compression: crate::scattered_types::ContextCompressionConfig::default(),
            tool_receipts: ToolReceiptsConfig::default(),
            tool_filter_groups: Vec::new(),
        }
    }
}

/// Named skill bundle (`[skill_bundles.<alias>]`).
///
/// A reusable group of skills that can be attached to an agent or channel
/// by alias, controlling which skills are loaded and from where.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "skill_bundle"]
#[serde(default)]
pub struct SkillBundleConfig {
    /// Directory path (relative to workspace root) to load skills from.
    pub directory: Option<String>,
    /// Skill names to include. Empty means include all skills in `directory`.
    pub include: Vec<String>,
    /// Skill names to exclude from this bundle.
    pub exclude: Vec<String>,
}

impl SkillBundleConfig {
    /// Whether a skill name survives this bundle's include/exclude filter.
    /// Empty `include` admits every name; `exclude` always wins.
    pub fn admits_skill(&self, name: &str) -> bool {
        if !self.include.is_empty() && !self.include.iter().any(|s| s == name) {
            return false;
        }
        !self.exclude.iter().any(|s| s == name)
    }
}

/// Named knowledge bundle (`[knowledge_bundles.<alias>]`).
///
/// A reusable set of knowledge sources (documents, URLs, or RAG corpus paths)
/// that can be attached to an agent by alias.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "knowledge_bundle"]
#[serde(default)]
pub struct KnowledgeBundleConfig {
    /// Paths or URLs to include in this knowledge bundle.
    pub sources: Vec<String>,
    /// Tags for filtering or categorising sources within the bundle.
    pub tags: Vec<String>,
}

/// Named MCP server bundle (`[mcp_bundles.<alias>]`).
///
/// A reusable group of MCP servers granted to an agent that references the
/// bundle by alias in `agents.<alias>.mcp_bundles`. Server IDs are matched
/// against `[mcp.servers]` by `name`. Resolution is secure by default (see
/// `Config::mcp_servers_for_bundles`): an ID with no matching server grants
/// nothing, and `exclude` wins over `servers` across every bundle an agent
/// references.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "mcp_bundle"]
#[serde(default)]
pub struct McpBundleConfig {
    /// MCP server IDs (`[mcp.servers].name`) granted by this bundle.
    pub servers: Vec<String>,
    /// MCP server IDs removed from the grant. Deny wins: a name listed here is
    /// excluded even if another referenced bundle includes it.
    pub exclude: Vec<String>,
}

// ── Runtime ──────────────────────────────────────────────────────

/// Runtime adapter kind.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    #[default]
    Native,
    Docker,
    Cloudflare,
}

impl RuntimeKind {
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Docker => "docker",
            Self::Cloudflare => "cloudflare",
        }
    }
}

/// Runtime adapter configuration (`[runtime]` section).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "runtime"]
pub struct RuntimeConfig {
    /// Runtime kind: native | docker | cloudflare.
    #[serde(default, deserialize_with = "deserialize_enum_lenient")]
    pub kind: RuntimeKind,

    /// Docker runtime settings (used when `kind = "docker"`).
    #[serde(default)]
    #[nested]
    pub docker: DockerRuntimeConfig,

    /// Shell binary the native runtime uses for command execution.
    ///
    /// Applies only to `runtime.kind = "native"`; other runtimes ignore it.
    /// When unset or `null`, the system default `sh` is used. The shell is
    /// invoked as `<shell> -c "<command>"`, so it must be a POSIX-compatible
    /// shell binary.
    ///
    /// Accepted forms (Unix):
    /// - a bare command name resolved via `PATH` (e.g. `"bash"`), or
    /// - an absolute path (e.g. `"/bin/bash"`, `"/usr/bin/zsh"`).
    ///
    /// The value is validated when the native runtime is constructed, so a bad
    /// value is reported up front rather than failing on the first shell
    /// command. Rejected: empty/whitespace; a relative path with separators
    /// (e.g. `"./sh"`, `"bin/sh"` — use a bare `PATH` name or an absolute path
    /// instead); a bare name not found on `PATH`; and a path that does not
    /// exist or is not executable.
    ///
    /// **Ignored on Windows and Android** (and not validated there): Windows
    /// always uses `cmd.exe`, and Android always uses `/system/bin/sh`
    /// (its shell is not on `PATH` for spawned processes).
    ///
    /// **Examples:**
    /// ```toml
    /// [runtime]
    /// shell = "bash" # resolves via PATH
    /// shell = "/bin/zsh" # absolute path
    /// ```
    #[serde(default)]
    pub shell: Option<String>,

    /// Global reasoning override for model_providers that expose explicit controls.
    /// - `None`: model_provider default behavior
    /// - `Some(true)`: request reasoning/thinking when supported
    /// - `Some(false)`: disable reasoning/thinking when supported
    #[serde(default)]
    pub reasoning_enabled: Option<bool>,
    /// Optional reasoning effort for model_providers that expose a level control.
    #[serde(default, deserialize_with = "deserialize_reasoning_effort_opt")]
    pub reasoning_effort: Option<String>,
}

/// Docker runtime configuration (`[runtime.docker]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "runtime.docker"]
pub struct DockerRuntimeConfig {
    /// Runtime image used to execute shell commands.
    #[serde(default = "default_docker_image")]
    pub image: String,

    /// Docker network mode (`none`, `bridge`, etc.).
    #[serde(default = "default_docker_network")]
    pub network: String,

    /// Optional memory limit in MB (`None` = no explicit limit).
    #[serde(default = "default_docker_memory_limit_mb")]
    pub memory_limit_mb: Option<u64>,

    /// Optional CPU limit (`None` = no explicit limit).
    #[serde(default = "default_docker_cpu_limit")]
    pub cpu_limit: Option<f64>,

    /// Mount root filesystem as read-only.
    #[serde(default = "default_true")]
    pub read_only_rootfs: bool,

    /// Mount configured workspace into `/workspace`.
    ///
    /// When enabled, the workspace must exist and canonicalize before Docker
    /// command construction.
    #[serde(default = "default_true")]
    pub mount_workspace: bool,

    /// Optional workspace root allowlist for fail-closed Docker mount validation: when `mount_workspace` is enabled, the workspace must exist and canonicalize even when this list is empty; every configured root must also exist and canonicalize; one invalid entry rejects the command before Docker starts; an empty list permits any canonical workspace.
    #[serde(default)]
    pub allowed_workspace_roots: Vec<String>,
}

fn default_docker_image() -> String {
    "alpine:3.20".into()
}

fn default_docker_network() -> String {
    "none".into()
}

fn default_docker_memory_limit_mb() -> Option<u64> {
    Some(512)
}

fn default_docker_cpu_limit() -> Option<f64> {
    Some(1.0)
}

impl Default for DockerRuntimeConfig {
    fn default() -> Self {
        Self {
            image: default_docker_image(),
            network: default_docker_network(),
            memory_limit_mb: default_docker_memory_limit_mb(),
            cpu_limit: default_docker_cpu_limit(),
            read_only_rootfs: true,
            mount_workspace: true,
            allowed_workspace_roots: Vec::new(),
        }
    }
}

// ── Reliability / supervision ────────────────────────────────────

/// Reliability and supervision configuration (`[reliability]` section).
///
/// Controls model_provider retries, API key rotation, and channel restart backoff.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "reliability"]
pub struct ReliabilityConfig {
    /// Retries per model_provider before bailing.
    #[serde(default = "default_provider_retries")]
    pub provider_retries: u32,
    /// Base backoff (ms) for model_provider retry delay.
    #[serde(default = "default_provider_backoff_ms")]
    pub provider_backoff_ms: u64,
    /// Additional API keys for round-robin rotation on rate-limit (429) errors.
    /// The primary `api_key` is always tried first; these are extras.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_keys: Vec<String>,
    /// Initial backoff for channel/daemon restarts.
    #[serde(default = "default_channel_backoff_secs")]
    pub channel_initial_backoff_secs: u64,
    /// Max backoff for channel/daemon restarts.
    #[serde(default = "default_channel_backoff_max_secs")]
    pub channel_max_backoff_secs: u64,
    /// Scheduler polling cadence in seconds.
    #[serde(default = "default_scheduler_poll_secs")]
    pub scheduler_poll_secs: u64,
    /// Max retries for cron job execution attempts.
    #[serde(default = "default_scheduler_retries")]
    pub scheduler_retries: u32,
}

fn default_provider_retries() -> u32 {
    2
}

fn default_provider_backoff_ms() -> u64 {
    500
}

fn default_channel_backoff_secs() -> u64 {
    2
}

fn default_channel_backoff_max_secs() -> u64 {
    60
}

fn default_scheduler_poll_secs() -> u64 {
    15
}

fn default_scheduler_retries() -> u32 {
    2
}

impl Default for ReliabilityConfig {
    fn default() -> Self {
        Self {
            provider_retries: default_provider_retries(),
            provider_backoff_ms: default_provider_backoff_ms(),
            api_keys: Vec::new(),
            channel_initial_backoff_secs: default_channel_backoff_secs(),
            channel_max_backoff_secs: default_channel_backoff_max_secs(),
            scheduler_poll_secs: default_scheduler_poll_secs(),
            scheduler_retries: default_scheduler_retries(),
        }
    }
}

// ── Scheduler ────────────────────────────────────────────────────

/// Scheduler configuration for periodic task execution (`[scheduler]` section).
///
/// Owns the cron-runtime knobs: per-job declarations live on
/// `Config.cron: HashMap<String, CronJobDecl>` (alias-keyed), while the
/// scheduler loop's runtime behavior (`enabled`, polling cap, catch-up) lives here.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "scheduler"]
pub struct SchedulerConfig {
    /// Enable the built-in scheduler loop. When false, no cron jobs run.
    #[serde(default = "default_scheduler_enabled")]
    pub enabled: bool,
    /// Maximum number of persisted scheduled tasks per polling cycle.
    #[serde(default = "default_scheduler_max_tasks")]
    pub max_tasks: usize,
    /// Maximum tasks executed in parallel within a single polling cycle.
    #[serde(default = "default_scheduler_max_concurrent")]
    pub max_concurrent: usize,
    /// Run all overdue jobs at scheduler startup. Default: `true`.
    ///
    /// When the daemon restarts late, jobs whose `next_run` is in the past
    /// fire once before normal polling resumes. Disable to wait for the
    /// next scheduled occurrence instead.
    #[serde(default = "default_true")]
    pub catch_up_on_startup: bool,
    /// Maximum number of historical cron run records to retain. Default: `50`.
    #[serde(default = "default_max_run_history")]
    pub max_run_history: u32,
}

fn default_scheduler_enabled() -> bool {
    true
}

fn default_scheduler_max_tasks() -> usize {
    64
}

fn default_scheduler_max_concurrent() -> usize {
    4
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: default_scheduler_enabled(),
            max_tasks: default_scheduler_max_tasks(),
            max_concurrent: default_scheduler_max_concurrent(),
            catch_up_on_startup: true,
            max_run_history: default_max_run_history(),
        }
    }
}

// ── Model routing ────────────────────────────────────────────────

/// Route a task hint to a specific model_provider + model.
///
/// ```toml
/// [[model_routes]]
/// hint = "reasoning"
/// model_provider = "openrouter.default"
/// model = "anthropic/claude-opus-4-20250514"
///
/// [[model_routes]]
/// hint = "fast"
/// model_provider = "groq.low-latency"
/// model = "llama-3.3-70b-versatile"
/// ```
///
/// Usage: pass `hint:reasoning` as the model parameter to route the request.
#[derive(Debug, Clone, Serialize, Deserialize, Default, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "model_routes"]
pub struct ModelRouteConfig {
    /// Task hint name (e.g. "reasoning", "fast", "code", "summarize")
    /// `#[serde(default)]` lets the `create_map_key` macro default-construct
    /// from `{}` before the hint gets injected via `set_prop`. Empty strings
    /// are rejected by `Config::validate()`.
    #[serde(default)]
    pub hint: String,
    /// Dotted provider profile ref to route to (must resolve to `providers.models.<type>.<alias>`)
    /// `#[serde(default)]` is required for `Default` + `create_map_key` construction.
    /// Empty strings are rejected by `Config::validate()`.
    #[serde(default)]
    pub model_provider: String,
    /// Provider-local model identifier to use with that provider profile
    /// `#[serde(default)]` is required for `Default` + `create_map_key` construction.
    /// Empty strings are rejected by `Config::validate()`.
    #[serde(default)]
    pub model: String,
    /// Optional API key override for this route's model provider
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_key: Option<String>,
}

// ── Embedding routing ───────────────────────────────────────────

/// Route an embedding hint to a specific model_provider + model.
///
/// ```toml
/// [[embedding_routes]]
/// hint = "semantic"
/// model_provider = "openai.embeddings"
/// model = "text-embedding-3-small"
/// dimensions = 1536
///
/// [memory]
/// embedding_model = "hint:semantic"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "embedding_routes"]
pub struct EmbeddingRouteConfig {
    /// Route hint name (e.g. "semantic", "archive", "faq")
    /// `#[serde(default)]` lets the `create_map_key` macro default-construct
    /// from `{}` before the hint gets injected via `set_prop`. Empty strings
    /// are rejected by `Config::validate()`.
    #[serde(default)]
    pub hint: String,
    /// Dotted embedding-capable provider profile ref
    /// `#[serde(default)]` is required for `Default` + `create_map_key` construction.
    /// Empty strings are rejected by `Config::validate()`.
    #[serde(default)]
    pub model_provider: String,
    /// Provider-local embedding model identifier to use with that provider profile
    /// `#[serde(default)]` is required for `Default` + `create_map_key` construction.
    /// Empty strings are rejected by `Config::validate()`.
    #[serde(default)]
    pub model: String,
    /// Optional embedding dimension override for this route
    #[serde(default)]
    pub dimensions: Option<usize>,
    /// Optional API key override for this route's model_provider
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_key: Option<String>,
}

// ── Query Classification ─────────────────────────────────────────

/// Automatic query classification — classifies user messages by keyword/pattern
/// and routes to the appropriate model hint. Disabled by default.
#[derive(Debug, Clone, Serialize, Deserialize, Default, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "query_classification"]
pub struct QueryClassificationConfig {
    /// Enable automatic query classification. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Classification rules evaluated in priority order.
    #[serde(default)]
    pub rules: Vec<ClassificationRule>,
}

/// A single classification rule mapping message patterns to a model hint.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct ClassificationRule {
    /// Must match a `[[model_routes]]` hint value.
    pub hint: String,
    /// Case-insensitive substring matches.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Case-sensitive literal matches (for "```", "fn ", etc.).
    #[serde(default)]
    pub patterns: Vec<String>,
    /// Only match if message length >= N chars.
    #[serde(default)]
    pub min_length: Option<usize>,
    /// Only match if message length <= N chars.
    #[serde(default)]
    pub max_length: Option<usize>,
    /// Higher priority rules are checked first.
    #[serde(default)]
    pub priority: i32,
}

// ── Heartbeat ────────────────────────────────────────────────────

/// Heartbeat configuration for periodic health pings (`[heartbeat]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "heartbeat"]
#[allow(clippy::struct_excessive_bools)]
pub struct HeartbeatConfig {
    /// Enable periodic heartbeat pings. Default: `false`. When enabled,
    /// `agent` must name a configured agent — there is no default agent
    /// for heartbeat to fall through to.
    #[serde(default)]
    pub enabled: bool,
    /// Configured agent alias the heartbeat worker runs as. Required
    /// when `enabled = true`; refers to a `[agents.<alias>]` entry.
    #[serde(default)]
    pub agent: String,
    /// Interval in minutes between heartbeat pings. Minimum: `1`. Default: `30`.
    #[serde(default = "default_heartbeat_interval")]
    pub interval_minutes: u32,
    /// Enable two-phase heartbeat: Phase 1 asks LLM whether to run, Phase 2
    /// executes only when the LLM decides there is work to do. Saves API cost
    /// during quiet periods. Default: `true`.
    #[serde(default = "default_two_phase")]
    pub two_phase: bool,
    /// Optional fallback task text when `HEARTBEAT.md` has no task entries.
    #[serde(default)]
    pub message: Option<String>,
    /// Optional delivery channel for heartbeat output (for example: `telegram`).
    /// When omitted, auto-selects the first configured channel.
    #[serde(default, alias = "channel")]
    pub target: Option<String>,
    /// Optional delivery recipient/chat identifier (required when `target` is
    /// explicitly set).
    #[serde(default, alias = "recipient")]
    pub to: Option<String>,
    /// Enable adaptive intervals that back off on failures and speed up for
    /// high-priority tasks. Default: `false`.
    #[serde(default)]
    pub adaptive: bool,
    /// Minimum interval in minutes when adaptive mode is enabled. Default: `5`.
    #[serde(default = "default_heartbeat_min_interval")]
    pub min_interval_minutes: u32,
    /// Maximum interval in minutes when adaptive mode backs off. Default: `120`.
    #[serde(default = "default_heartbeat_max_interval")]
    pub max_interval_minutes: u32,
    /// Dead-man's switch timeout in minutes. If the heartbeat has not ticked
    /// within this window, an alert is sent. `0` disables. Default: `0`.
    #[serde(default)]
    pub deadman_timeout_minutes: u32,
    /// Channel for dead-man's switch alerts (e.g. `telegram`). Falls back to
    /// the heartbeat delivery channel.
    #[serde(default)]
    pub deadman_channel: Option<String>,
    /// Recipient for dead-man's switch alerts. Falls back to `to`.
    #[serde(default)]
    pub deadman_to: Option<String>,
    /// Maximum number of heartbeat run history records to retain. Default: `100`.
    #[serde(default = "default_heartbeat_max_run_history")]
    pub max_run_history: u32,
    /// Load the channel session history before each heartbeat task execution so
    /// the LLM has conversational context. Default: `false`.
    ///
    /// When `true`, the session file for the configured `target`/`to` is passed
    /// to the agent as `session_state_file`, giving it access to the recent
    /// conversation history — just as if the user had sent a message.
    #[serde(default)]
    pub load_session_context: bool,
    /// Maximum wall-clock seconds allowed for a single agent invocation
    /// (Phase 1 decision or Phase 2 task execution). `0` disables.
    /// Default: `600` (10 minutes).
    #[serde(default = "default_heartbeat_task_timeout")]
    pub task_timeout_secs: u64,
}

fn default_heartbeat_interval() -> u32 {
    30
}

fn default_two_phase() -> bool {
    true
}

fn default_heartbeat_min_interval() -> u32 {
    5
}

fn default_heartbeat_max_interval() -> u32 {
    120
}

fn default_heartbeat_max_run_history() -> u32 {
    100
}

fn default_heartbeat_task_timeout() -> u64 {
    600
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            agent: String::new(),
            interval_minutes: default_heartbeat_interval(),
            two_phase: true,
            message: None,
            target: None,
            to: None,
            adaptive: false,
            min_interval_minutes: default_heartbeat_min_interval(),
            max_interval_minutes: default_heartbeat_max_interval(),
            deadman_timeout_minutes: 0,
            deadman_channel: None,
            deadman_to: None,
            max_run_history: default_heartbeat_max_run_history(),
            load_session_context: false,
            task_timeout_secs: default_heartbeat_task_timeout(),
        }
    }
}

// ── TodoTracker ──────────────────────────────────────────────────

/// Location of the ZeroCode TodoWrite tracker panel.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum TodoTrackerLocation {
    /// Horizontal strip between the transcript and the input bar (Claude Code style).
    Bottom,
    /// Vertical side panel on the left.
    Left,
    /// Vertical side panel on the right (OpenCode style). Default.
    #[default]
    Right,
}

/// ZeroCode live task tracker configuration (`[todotracker]` section).
///
/// Read-only visual tracker driven by the model's `TodoWrite` tool.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "todotracker"]
pub struct TodoTrackerConfig {
    /// Master switch. When `false` the tracker never renders and never
    /// auto-pops. Default: `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Whether the panel is visible at launch (when `enabled`). When
    /// `false` it stays hidden until toggled or auto-popped by the first
    /// plan. Default: `false`.
    #[serde(default)]
    pub enabled_at_start: bool,
    /// Panel location: `bottom` (between transcript and input), `left`,
    /// or `right` (default).
    #[serde(default)]
    pub location: TodoTrackerLocation,
    /// Side-panel target column width (left/right). Runtime-clamped to at
    /// most half the terminal width. Ignored for `bottom`. Default: `32`.
    #[serde(default = "default_todotracker_width")]
    pub width: u16,
    /// Bottom-strip maximum height in rows (grows up to this). Ignored for
    /// left/right. Default: `5`.
    #[serde(default = "default_todotracker_max_height")]
    pub max_height: u16,
}

fn default_todotracker_width() -> u16 {
    32
}

fn default_todotracker_max_height() -> u16 {
    5
}

impl Default for TodoTrackerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            enabled_at_start: false,
            location: TodoTrackerLocation::Right,
            width: default_todotracker_width(),
            max_height: default_todotracker_max_height(),
        }
    }
}

// ── Cron ────────────────────────────────────────────────────────

/// A declarative cron job definition (`[cron.<alias>]`).
///
/// Stored alias-keyed on `Config.cron`. The map key serves as the stable job id.
/// Synced into the database at scheduler startup with `source = "declarative"`,
/// distinguishing them from jobs created imperatively via CLI or API.
/// Declarative config takes precedence on each sync: if the config changes,
/// the DB is updated to match. Imperative jobs are never deleted by sync.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "cron"]
pub struct CronJobDecl {
    /// Human-readable name.
    #[serde(default)]
    pub name: Option<String>,
    /// Job type: `"shell"` (default) or `"agent"`.
    #[serde(default = "default_job_type_decl")]
    pub job_type: String,
    /// Schedule for the job.
    #[serde(default)]
    pub schedule: CronScheduleDecl,
    /// Shell command to run (required when `job_type = "shell"`).
    #[serde(default)]
    pub command: Option<String>,
    /// Agent prompt (required when `job_type = "agent"`).
    #[serde(default)]
    pub prompt: Option<String>,
    /// Whether the job is enabled. Default: `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Model override for agent jobs.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional allowlist of tool names for agent jobs. When omitted, scheduler
    /// defaults may still exclude scheduler mutation tools for cron agent jobs.
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// Whether to recall and inject memory context before this agent job runs.
    /// Defaults to `true`; set to `false` for stateless digest jobs.
    #[serde(default = "default_true")]
    pub uses_memory: bool,
    /// Session target: `"isolated"` (default) or `"main"`.
    #[serde(default)]
    pub session_target: Option<String>,
    /// Delivery configuration.
    #[serde(default)]
    #[nested]
    pub delivery: Option<DeliveryConfigDecl>,
}

impl Default for CronJobDecl {
    fn default() -> Self {
        Self {
            name: None,
            job_type: default_job_type_decl(),
            schedule: CronScheduleDecl::default(),
            command: None,
            prompt: None,
            enabled: true,
            model: None,
            allowed_tools: None,
            uses_memory: true,
            session_target: None,
            delivery: None,
        }
    }
}

/// Schedule variant for declarative cron jobs.
#[derive(Debug, Clone, Serialize, Deserialize, zeroclaw_macros::ConfigEnum)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum CronScheduleDecl {
    /// Classic cron expression.
    Cron {
        expr: String,
        #[serde(default)]
        tz: Option<String>,
    },
    /// Interval in milliseconds.
    Every { every_ms: u64 },
    /// One-shot at an RFC 3339 timestamp.
    At { at: String },
}

impl Default for CronScheduleDecl {
    fn default() -> Self {
        // Empty cron expression — `validate_decl` rejects it. Used only as
        // a placeholder when a fresh map entry is auto-created via the
        // schema's `create_map_key` path; the user fills it in immediately.
        Self::Cron {
            expr: String::new(),
            tz: None,
        }
    }
}

/// Delivery configuration for declarative cron jobs.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "cron_delivery"]
pub struct DeliveryConfigDecl {
    /// Delivery mode: `"none"` or `"announce"`.
    #[serde(default = "default_delivery_mode")]
    pub mode: String,
    /// Channel name (e.g. `"telegram"`, `"discord"`).
    #[serde(default)]
    pub channel: Option<String>,
    /// Target/recipient identifier.
    #[serde(default)]
    pub to: Option<String>,
    /// Optional thread/conversation identifier carried into the outbound send.
    /// Required by channels that route on a separate `thread_id` field (e.g.
    /// webhook callbacks bridging into agent-chat platforms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Best-effort delivery. Default: `true`.
    #[serde(default = "default_true")]
    pub best_effort: bool,
}

impl Default for DeliveryConfigDecl {
    fn default() -> Self {
        Self {
            mode: default_delivery_mode(),
            channel: None,
            to: None,
            thread_id: None,
            best_effort: true,
        }
    }
}

fn default_job_type_decl() -> String {
    "shell".to_string()
}

fn default_delivery_mode() -> String {
    "none".to_string()
}

fn default_max_run_history() -> u32 {
    50
}

// ── ACP ──────────────────────────────────────────────────────────

/// ACP (Agent Client Protocol) server configuration (`[acp]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "acp"]
pub struct AcpConfig {
    /// Agent alias to use when `session/new` omits `agentAlias` and more than
    /// one agent is configured. When exactly one agent exists it is
    /// auto-selected regardless of this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_agent: Option<String>,
    /// Maximum number of concurrent ACP sessions. Default: `10`.
    #[serde(default = "default_acp_max_sessions")]
    pub max_sessions: usize,
    /// Idle session timeout in seconds. Sessions with no activity for this
    /// duration are eligible for eviction. Default: `3600` (1 hour).
    #[serde(default = "default_acp_session_timeout_secs")]
    pub session_timeout_secs: u64,
}

fn default_acp_max_sessions() -> usize {
    10
}

fn default_acp_session_timeout_secs() -> u64 {
    3600
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            default_agent: None,
            max_sessions: default_acp_max_sessions(),
            session_timeout_secs: default_acp_session_timeout_secs(),
        }
    }
}

// ── Tunnel ──────────────────────────────────────────────────────

/// Tunnel configuration for exposing the gateway publicly (`[tunnel]` section).
///
/// Supported model_providers: `"none"` (default), `"cloudflare"`, `"tailscale"`, `"ngrok"`, `"openvpn"`, `"pinggy"`, `"custom"`.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "tunnel"]
pub struct TunnelConfig {
    /// How the gateway gets exposed to the public internet so webhooks (Telegram, Slack, etc.) can reach it. `none` = keep it local, no tunnel; `cloudflare` = Cloudflare Tunnel via cloudflared (needs a Zero Trust account and token); `tailscale` = Tailscale Funnel/Serve (tailnet-only or public, no account beyond tailscale); `ngrok` = ngrok agent with auth token; `openvpn` = bring-your-own OpenVPN egress; `pinggy` = Pinggy SSH tunnels (quick one-shot URLs); `custom` = run an arbitrary command you define under `[tunnel.custom]`.
    #[serde(default = "default_tunnel_provider")]
    pub tunnel_provider: String,

    /// Cloudflare Tunnel configuration (used when `tunnel_provider = "cloudflare"`).
    #[serde(default)]
    #[nested]
    pub cloudflare: Option<CloudflareTunnelConfig>,

    /// Tailscale Funnel/Serve configuration (used when `tunnel_provider = "tailscale"`).
    #[serde(default)]
    #[nested]
    pub tailscale: Option<TailscaleTunnelConfig>,

    /// ngrok tunnel configuration (used when `tunnel_provider = "ngrok"`).
    #[serde(default)]
    #[nested]
    pub ngrok: Option<NgrokTunnelConfig>,

    /// OpenVPN tunnel configuration (used when `tunnel_provider = "openvpn"`).
    #[serde(default)]
    #[nested]
    pub openvpn: Option<OpenVpnTunnelConfig>,

    /// Custom tunnel command configuration (used when `tunnel_provider = "custom"`).
    #[serde(default)]
    #[nested]
    pub custom: Option<CustomTunnelConfig>,

    /// Pinggy tunnel configuration (used when `tunnel_provider = "pinggy"`).
    #[serde(default)]
    #[nested]
    pub pinggy: Option<PinggyTunnelConfig>,
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            tunnel_provider: "none".into(),
            cloudflare: None,
            tailscale: None,
            ngrok: None,
            openvpn: None,
            custom: None,
            pinggy: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "tunnel.cloudflare"]
pub struct CloudflareTunnelConfig {
    /// Cloudflare Tunnel token (from Zero Trust dashboard)
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub token: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "tunnel.tailscale"]
pub struct TailscaleTunnelConfig {
    /// Use Tailscale Funnel (public internet) vs Serve (tailnet only)
    #[serde(default)]
    pub funnel: bool,
    /// Optional hostname override
    #[serde(default)]
    pub hostname: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "tunnel.ngrok"]
pub struct NgrokTunnelConfig {
    /// ngrok auth token
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub auth_token: String,
    /// Optional custom domain
    #[serde(default)]
    pub domain: Option<String>,
}

/// OpenVPN tunnel configuration (`[tunnel.openvpn]`).
///
/// Required when `tunnel.tunnel_provider = "openvpn"`. Omitting this section entirely
/// preserves previous behavior. Setting `tunnel.tunnel_provider = "none"` (or removing
/// the `[tunnel.openvpn]` block) cleanly reverts to no-tunnel mode.
///
/// Defaults: `connect_timeout_secs = 30`.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "tunnel.openvpn"]
pub struct OpenVpnTunnelConfig {
    /// Path to `.ovpn` configuration file (must not be empty).
    pub config_file: String,
    /// Optional path to auth credentials file (`--auth-user-pass`).
    #[serde(default)]
    #[credential_class = "path_only_reference"]
    pub auth_file: Option<String>,
    /// Advertised address once VPN is connected (e.g., `"10.8.0.2:42617"`).
    /// When omitted the tunnel falls back to `http://{local_host}:{local_port}`.
    #[serde(default)]
    pub advertise_address: Option<String>,
    /// Connection timeout in seconds (default: 30, must be > 0).
    #[serde(default = "default_openvpn_timeout")]
    pub connect_timeout_secs: u64,
    /// Extra openvpn CLI arguments forwarded verbatim.
    #[serde(default)]
    pub extra_args: Vec<String>,
}

fn default_openvpn_timeout() -> u64 {
    30
}

impl Default for OpenVpnTunnelConfig {
    fn default() -> Self {
        Self {
            config_file: String::new(),
            auth_file: None,
            advertise_address: None,
            connect_timeout_secs: default_openvpn_timeout(),
            extra_args: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "tunnel.pinggy"]
pub struct PinggyTunnelConfig {
    /// Pinggy access token (optional — free tier works without one).
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub token: Option<String>,
    /// Server region: `"us"` (USA), `"eu"` (Europe), `"ap"` (Asia), `"br"` (South America), `"au"` (Australia), or omit for auto.
    #[serde(default)]
    pub region: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "tunnel.custom"]
pub struct CustomTunnelConfig {
    /// Command template to start the tunnel. Use {port} and {host} placeholders.
    /// Example: "bore local {port} --to bore.pub"
    #[serde(default)]
    pub start_command: String,
    /// Optional URL to check tunnel health
    #[serde(default)]
    pub health_url: Option<String>,
    /// Optional regex to extract public URL from command stdout
    #[serde(default)]
    pub url_pattern: Option<String>,
}

// ── Channels ─────────────────────────────────────────────────────

/// Top-level channel configurations (`[channels]` section).
///
/// each channel type is a keyed table of named instances (aliases).
/// `[channels.telegram.default]` is the conventional single-instance key.
/// Access via `config.channels.telegram.get("default")`.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels"]
pub struct ChannelsConfig {
    /// Enable the CLI interactive channel. Default: `true`.
    #[serde(default = "default_true")]
    pub cli: bool,
    /// Telegram bot channel instances (`[channels.telegram.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub telegram: HashMap<String, TelegramConfig>,
    /// Discord bot channel instances (`[channels.discord.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub discord: HashMap<String, DiscordConfig>,
    /// Slack bot channel instances (`[channels.slack.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub slack: HashMap<String, SlackConfig>,
    /// Mattermost bot channel instances (`[channels.mattermost.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub mattermost: HashMap<String, MattermostConfig>,
    /// Webhook channel instances (`[channels.webhook.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub webhook: HashMap<String, WebhookConfig>,
    /// iMessage channel instances (`[channels.imessage.<alias>]`, macOS only).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub imessage: HashMap<String, IMessageConfig>,
    /// Matrix channel instances (`[channels.matrix.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub matrix: HashMap<String, MatrixConfig>,
    /// Signal channel instances (`[channels.signal.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub signal: HashMap<String, SignalConfig>,
    /// WhatsApp channel instances (`[channels.whatsapp.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub whatsapp: HashMap<String, WhatsAppConfig>,
    /// Linq Partner API channel instances (`[channels.linq.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub linq: HashMap<String, LinqConfig>,
    /// WATI WhatsApp Business API channel instances (`[channels.wati.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub wati: HashMap<String, WatiConfig>,
    /// Nextcloud Talk bot channel instances (`[channels.nextcloud_talk.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub nextcloud_talk: HashMap<String, NextcloudTalkConfig>,
    /// Email channel instances (`[channels.email.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub email: HashMap<String, crate::scattered_types::EmailConfig>,
    /// Gmail Pub/Sub push notification channel instances (`[channels.gmail_push.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub gmail_push: HashMap<String, crate::scattered_types::GmailPushConfig>,
    /// IRC channel instances (`[channels.irc.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub irc: HashMap<String, IrcConfig>,
    /// Twitch chat channel instances (`[channels.twitch.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub twitch: HashMap<String, TwitchConfig>,
    /// Lark channel instances (`[channels.lark.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub lark: HashMap<String, LarkConfig>,
    /// LINE Messaging API channel instances (`[channels.line.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub line: HashMap<String, LineConfig>,
    /// DingTalk channel instances (`[channels.dingtalk.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub dingtalk: HashMap<String, DingTalkConfig>,
    /// WeCom (WeChat Enterprise) Bot Webhook channel instances (`[channels.wecom.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub wecom: HashMap<String, WeComConfig>,
    /// WeCom AI Bot WebSocket channel instances (`[channels.wecom_ws.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub wecom_ws: HashMap<String, WeComWsConfig>,
    /// WeChat personal iLink Bot channel instances (`[channels.wechat.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub wechat: HashMap<String, WeChatConfig>,
    /// QQ Official Bot channel instances (`[channels.qq.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub qq: HashMap<String, QQConfig>,
    /// X/Twitter channel instances (`[channels.twitter.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub twitter: HashMap<String, TwitterConfig>,
    /// Mochat customer service channel instances (`[channels.mochat.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub mochat: HashMap<String, MochatConfig>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub nostr: HashMap<String, NostrConfig>,
    /// ClawdTalk voice channel instances (`[channels.clawdtalk.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub clawdtalk: HashMap<String, crate::scattered_types::ClawdTalkConfig>,
    /// Reddit channel instances (`[channels.reddit.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub reddit: HashMap<String, RedditConfig>,
    /// Bluesky channel instances (`[channels.bluesky.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub bluesky: HashMap<String, BlueskyConfig>,
    /// Git-forge channel instances (`[channels.git.<alias>]`). GitHub is
    /// the first provider; the `provider` field selects the forge.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub git: HashMap<String, GitConfig>,
    /// Voice call channel instances (`[channels.voice_call.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub voice_call: HashMap<String, crate::scattered_types::VoiceCallConfig>,
    /// Voice wake word detection channel instances (`[channels.voice_wake.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub voice_wake: HashMap<String, VoiceWakeConfig>,
    /// Voice duplex instances (`[channels.voice_duplex.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub voice_duplex: HashMap<String, VoiceDuplexConfig>,
    /// MQTT SOP listener instances — RETIRED. The section now
    /// fails config parse with the migration message instead of silently
    /// no-op'ing. Field removed with the run-side config sweep.
    #[serde(
        default,
        skip_serializing_if = "HashMap::is_empty",
        deserialize_with = "reject_retired_sop_channel"
    )]
    #[cfg_attr(feature = "schema-export", schemars(skip))]
    pub mqtt: HashMap<String, MqttConfig>,
    /// AMQP channel instances (`[channels.amqp.<alias>]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[nested]
    pub amqp: HashMap<String, AmqpConfig>,
    /// Filesystem SOP listener instances — RETIRED. The section
    /// now fails config parse with the migration message instead of silently
    /// no-op'ing. Field removed with the run-side config sweep.
    #[serde(
        default,
        skip_serializing_if = "HashMap::is_empty",
        deserialize_with = "reject_retired_sop_channel"
    )]
    #[cfg_attr(feature = "schema-export", schemars(skip))]
    pub filesystem: HashMap<String, FilesystemConfig>,
    /// Base timeout in seconds for processing a single channel message (LLM + tools).
    /// Runtime uses this as a per-turn budget that scales with tool-loop depth
    /// (up to 4x, capped) so one slow/retried model call does not consume the
    /// entire conversation budget.
    /// Default: 300s for on-device LLMs (Ollama) which are slower than cloud APIs.
    #[serde(default = "default_channel_message_timeout_secs")]
    pub message_timeout_secs: u64,
    /// Per-channel multiplier for the global channel message in-flight budget.
    /// Runtime multiplies this value by the configured channel count, then
    /// applies its global minimum and maximum bounds to one shared dispatcher
    /// semaphore. Default: `4`.
    #[serde(default = "default_channel_max_concurrent_per_channel")]
    pub max_concurrent_per_channel: usize,
    /// Whether to add acknowledgement reactions (👀 on receipt, ✅/⚠️ on
    /// completion) to incoming channel messages. Default: `true`.
    #[serde(default = "default_true")]
    pub ack_reactions: bool,
    /// Whether to send tool-call notification messages (e.g. `🔧 web_search_tool: …`)
    /// to channel users. When `false`, tool calls are still logged server-side but
    /// not forwarded as individual channel messages. Default: `false`.
    #[serde(default = "default_false")]
    pub show_tool_calls: bool,
    /// Persist channel conversation history to JSONL files so sessions survive
    /// daemon restarts. Files are stored in `{workspace}/sessions/`. Default: `true`.
    #[serde(default = "default_true")]
    pub session_persistence: bool,
    /// Session persistence backend: `"jsonl"` (legacy) or `"sqlite"` (new default).
    /// SQLite provides FTS5 search, metadata tracking, and TTL cleanup.
    #[serde(default = "default_session_backend")]
    pub session_backend: String,
    /// Auto-archive stale sessions older than this many hours. `0` disables. Default: `0`.
    #[serde(default)]
    pub session_ttl_hours: u32,
    /// Inbound message debounce window in milliseconds. When a sender fires
    /// multiple messages within this window, they are accumulated and dispatched
    /// as a single concatenated message. `0` disables debouncing. Default: `0`.
    #[serde(default)]
    pub debounce_ms: u64,
}

// ── Retired SOP run-side config keys ────────────────────────────────────────
// These deserializers keep the retired keys PARSEABLE only long enough to
// reject them with an actionable message: silently ignoring them would
// reinterpret a run-side config (a SOP dispatch mode, a filesystem/mqtt SOP
// trigger channel, a git sop route) as "do nothing", which is not a safe
// reading of the operator's intent. Run truth is Tachi-side since the V4
// procedure_v1 seam.

fn reject_retired_sop_key<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    match value {
        Some(v) => Err(serde::de::Error::custom(format!(
            "retired SOP run-side config key (value `{v}`): SOP runs are Tachi-side \
             ProcedureRuns since #243 (#197 wall 5) — remove this key; see              docs/book/src/sop/ for the definition-only surface"
        ))),
        None => Ok(None),
    }
}

fn reject_retired_sop_channel<'de, D, V>(deserializer: D) -> Result<HashMap<String, V>, D::Error>
where
    D: serde::Deserializer<'de>,
    V: Default,
{
    // Any presence of the section — even an empty header — is rejected: the
    // section is retired, presence itself is the misconfiguration.
    let _map = HashMap::<String, serde::de::IgnoredAny>::deserialize(deserializer)?;
    Err(serde::de::Error::custom(
        "retired SOP run-side channel: MQTT/filesystem listeners were SOP-trigger-only \
         fan-in; SOP runs are Tachi-side ProcedureRuns since #243 (#197 wall 5) — remove \
         this [channels.*] section",
    ))
}

impl ChannelsConfig {
    /// Returns metadata and configuration status for every known channel type.
    ///
    /// Always returns the full set of channel types regardless of compile-time
    /// feature flags — the `configured` flag reflects whether the operator has
    /// populated that channel's config section. For a list restricted to only
    /// the channels compiled into this binary use
    /// `zeroclaw_channels::listing::compiled_channels` instead.
    pub fn channels(&self) -> Vec<super::traits::ChannelInfo> {
        use super::traits::ChannelInfo;
        vec![
            ChannelInfo {
                kind: "telegram",
                name: "Telegram",
                desc: "connect your bot",
                configured: !self.telegram.is_empty(),
            },
            ChannelInfo {
                kind: "discord",
                name: "Discord",
                desc: "connect your bot",
                configured: !self.discord.is_empty(),
            },
            ChannelInfo {
                kind: "slack",
                name: "Slack",
                desc: "connect your bot",
                configured: !self.slack.is_empty(),
            },
            ChannelInfo {
                kind: "mattermost",
                name: "Mattermost",
                desc: "connect to your bot",
                configured: !self.mattermost.is_empty(),
            },
            ChannelInfo {
                kind: "imessage",
                name: "iMessage",
                desc: "macOS only",
                configured: !self.imessage.is_empty(),
            },
            ChannelInfo {
                kind: "matrix",
                name: "Matrix",
                desc: "self-hosted chat",
                configured: !self.matrix.is_empty(),
            },
            ChannelInfo {
                kind: "signal",
                name: "Signal",
                desc: "An open-source, encrypted messaging service",
                configured: !self.signal.is_empty(),
            },
            ChannelInfo {
                kind: "whatsapp",
                name: "WhatsApp",
                desc: "Business Cloud API",
                configured: !self.whatsapp.is_empty(),
            },
            ChannelInfo {
                kind: "whatsapp-web",
                name: "WhatsApp Web",
                desc: "native WhatsApp Web (wa-rs)",
                configured: self.whatsapp.values().any(|c| c.is_web_config()),
            },
            ChannelInfo {
                kind: "linq",
                name: "Linq",
                desc: "iMessage/RCS/SMS via Linq API",
                configured: !self.linq.is_empty(),
            },
            ChannelInfo {
                kind: "wati",
                name: "WATI",
                desc: "WhatsApp via WATI Business API",
                configured: !self.wati.is_empty(),
            },
            ChannelInfo {
                kind: "nextcloud",
                name: "NextCloud Talk",
                desc: "NextCloud Talk platform",
                configured: !self.nextcloud_talk.is_empty(),
            },
            ChannelInfo {
                kind: "email",
                name: "Email",
                desc: "Email over IMAP/SMTP",
                configured: !self.email.is_empty(),
            },
            ChannelInfo {
                kind: "gmail-push",
                name: "Gmail Push",
                desc: "Gmail Pub/Sub push notifications",
                configured: !self.gmail_push.is_empty(),
            },
            ChannelInfo {
                kind: "twitch",
                name: "Twitch",
                desc: "Twitch chat (IRC)",
                configured: !self.twitch.is_empty(),
            },
            ChannelInfo {
                kind: "irc",
                name: "IRC",
                desc: "IRC over TLS",
                configured: !self.irc.is_empty(),
            },
            ChannelInfo {
                kind: "lark",
                name: "Lark",
                desc: "Lark Bot",
                configured: !self.lark.is_empty(),
            },
            ChannelInfo {
                kind: "dingtalk",
                name: "DingTalk",
                desc: "DingTalk Stream Mode",
                configured: !self.dingtalk.is_empty(),
            },
            ChannelInfo {
                kind: "wecom",
                name: "WeCom",
                desc: "WeCom Bot Webhook",
                configured: !self.wecom.is_empty(),
            },
            ChannelInfo {
                kind: "wecom-ws",
                name: "WeCom WebSocket",
                desc: "WeCom AI Bot long connection",
                configured: !self.wecom_ws.is_empty(),
            },
            ChannelInfo {
                kind: "wechat",
                name: "WeChat",
                desc: "WeChat iLink Bot",
                configured: !self.wechat.is_empty(),
            },
            ChannelInfo {
                kind: "qq",
                name: "QQ Official",
                desc: "Tencent QQ Bot",
                configured: !self.qq.is_empty(),
            },
            ChannelInfo {
                kind: "nostr",
                name: "Nostr",
                desc: "Nostr DMs",
                configured: !self.nostr.is_empty(),
            },
            ChannelInfo {
                kind: "clawdtalk",
                name: "ClawdTalk",
                desc: "ClawdTalk Channel",
                configured: !self.clawdtalk.is_empty(),
            },
            ChannelInfo {
                kind: "reddit",
                name: "Reddit",
                desc: "Reddit bot (OAuth2)",
                configured: !self.reddit.is_empty(),
            },
            ChannelInfo {
                kind: "bluesky",
                name: "Bluesky",
                desc: "AT Protocol",
                configured: !self.bluesky.is_empty(),
            },
            ChannelInfo {
                kind: "git",
                name: "Git",
                desc: "Git forge (GitHub, Gitea, Forgejo): issues, PRs & events",
                configured: !self.git.is_empty(),
            },
            ChannelInfo {
                kind: "twitter",
                name: "X/Twitter",
                desc: "X/Twitter Bot via API v2",
                configured: !self.twitter.is_empty(),
            },
            ChannelInfo {
                kind: "mochat",
                name: "Mochat",
                desc: "Mochat Customer Service",
                configured: !self.mochat.is_empty(),
            },
            ChannelInfo {
                kind: "line",
                name: "LINE",
                desc: "connect your LINE bot",
                configured: !self.line.is_empty(),
            },
            ChannelInfo {
                kind: "voice-call",
                name: "Voice Call",
                desc: "outbound voice call channel",
                configured: !self.voice_call.is_empty(),
            },
            ChannelInfo {
                kind: "voice-wake",
                name: "VoiceWake",
                desc: "voice wake word detection",
                configured: !self.voice_wake.is_empty(),
            },
            ChannelInfo {
                kind: "amqp",
                name: "AMQP",
                desc: "AMQP topic consumer",
                configured: !self.amqp.is_empty(),
            },
            ChannelInfo {
                kind: "webhook",
                name: "Webhook",
                desc: "HTTP endpoint",
                configured: !self.webhook.is_empty(),
            },
        ]
    }

    /// Returns `true` when at least one channel entry across all channel types
    /// has `enabled = true`. Used by the daemon to decide whether the channels
    /// supervisor should be started — a config with only `enabled = false`
    /// entries (e.g. partially-configured or disabled bots) must not start the
    /// supervisor, otherwise it exits immediately and restarts in a tight loop.
    pub fn has_any_enabled(&self) -> bool {
        self.telegram.values().any(|c| c.enabled)
            || self.discord.values().any(|c| c.enabled)
            || self.slack.values().any(|c| c.enabled)
            || self.mattermost.values().any(|c| c.enabled)
            || self.webhook.values().any(|c| c.enabled)
            || self.imessage.values().any(|c| c.enabled)
            || self.matrix.values().any(|c| c.enabled)
            || self.signal.values().any(|c| c.enabled)
            || self.whatsapp.values().any(|c| c.enabled)
            || self.linq.values().any(|c| c.enabled)
            || self.wati.values().any(|c| c.enabled)
            || self.nextcloud_talk.values().any(|c| c.enabled)
            || self.email.values().any(|c| c.enabled)
            || self.gmail_push.values().any(|c| c.enabled)
            || self.irc.values().any(|c| c.enabled)
            || self.twitch.values().any(|c| c.enabled)
            || self.lark.values().any(|c| c.enabled)
            || self.line.values().any(|c| c.enabled)
            || self.dingtalk.values().any(|c| c.enabled)
            || self.wecom.values().any(|c| c.enabled)
            || self.wecom_ws.values().any(|c| c.enabled)
            || self.wechat.values().any(|c| c.enabled)
            || self.qq.values().any(|c| c.enabled)
            || self.twitter.values().any(|c| c.enabled)
            || self.mochat.values().any(|c| c.enabled)
            || self.nostr.values().any(|c| c.enabled)
            || self.clawdtalk.values().any(|c| c.enabled)
            || self.reddit.values().any(|c| c.enabled)
            || self.bluesky.values().any(|c| c.enabled)
            || self.voice_call.values().any(|c| c.enabled)
            || self.voice_wake.values().any(|c| c.enabled)
            || self.voice_duplex.values().any(|c| c.enabled)
            || self.amqp.values().any(|c| c.enabled)
            || self.git.values().any(|c| c.enabled)
    }

    /// One `(canonical_name, configured, deliverable)` row per channel in the
    /// registry. Single source for name-addressed channel lookups so no surface
    /// has to hardcode a subset of the channel list. `deliverable` is `false`
    /// for input-only transports whose `Channel::send` is a no-op (amqp is a
    /// fan-in consumer; voice_wake and voice_duplex are input-only), so a name-addressed
    /// outbound surface such as `heartbeat.target` can refuse them at validation
    /// instead of accepting a target the delivery layer silently drops.
    pub fn channel_presence(&self) -> [(&'static str, bool, bool); 34] {
        [
            ("telegram", !self.telegram.is_empty(), true),
            ("discord", !self.discord.is_empty(), true),
            ("slack", !self.slack.is_empty(), true),
            ("mattermost", !self.mattermost.is_empty(), true),
            ("webhook", !self.webhook.is_empty(), true),
            ("imessage", !self.imessage.is_empty(), true),
            ("matrix", !self.matrix.is_empty(), true),
            ("signal", !self.signal.is_empty(), true),
            ("whatsapp", !self.whatsapp.is_empty(), true),
            ("linq", !self.linq.is_empty(), true),
            ("wati", !self.wati.is_empty(), true),
            ("nextcloud_talk", !self.nextcloud_talk.is_empty(), true),
            ("email", !self.email.is_empty(), true),
            ("gmail_push", !self.gmail_push.is_empty(), true),
            ("irc", !self.irc.is_empty(), true),
            ("twitch", !self.twitch.is_empty(), true),
            ("lark", !self.lark.is_empty(), true),
            ("line", !self.line.is_empty(), true),
            ("dingtalk", !self.dingtalk.is_empty(), true),
            ("wecom", !self.wecom.is_empty(), true),
            ("wecom_ws", !self.wecom_ws.is_empty(), true),
            ("wechat", !self.wechat.is_empty(), true),
            ("qq", !self.qq.is_empty(), true),
            ("twitter", !self.twitter.is_empty(), true),
            ("mochat", !self.mochat.is_empty(), true),
            ("nostr", !self.nostr.is_empty(), true),
            ("clawdtalk", !self.clawdtalk.is_empty(), true),
            ("reddit", !self.reddit.is_empty(), true),
            ("bluesky", !self.bluesky.is_empty(), true),
            ("git", !self.git.is_empty(), true),
            ("voice_call", !self.voice_call.is_empty(), true),
            ("voice_wake", !self.voice_wake.is_empty(), false),
            ("voice_duplex", !self.voice_duplex.is_empty(), false),
            ("amqp", !self.amqp.is_empty(), false),
        ]
    }

    /// Whether a channel named `name` (case-insensitive) is a known channel
    /// with at least one configured instance. Used by name-addressed surfaces
    /// (e.g. `heartbeat.target`) without hardcoding a subset of the registry.
    pub fn is_channel_configured(&self, name: &str) -> bool {
        let needle = name.to_ascii_lowercase();
        self.channel_presence()
            .iter()
            .any(|(canonical, configured, _)| *configured && *canonical == needle)
    }

    /// Whether `name` (case-insensitive) names a channel in the registry,
    /// regardless of whether it is configured.
    pub fn is_known_channel(&self, name: &str) -> bool {
        let needle = name.to_ascii_lowercase();
        self.channel_presence()
            .iter()
            .any(|(canonical, _, _)| *canonical == needle)
    }

    /// Whether `name` (case-insensitive) names a channel that can actually
    /// deliver an outbound message. Input-only transports (amqp, voice_wake,
    /// voice_duplex) are known and may be configured, but their `Channel::send`
    /// is a no-op or absent, so they are not valid outbound targets.
    pub fn is_channel_deliverable(&self, name: &str) -> bool {
        let needle = name.to_ascii_lowercase();
        self.channel_presence()
            .iter()
            .any(|(canonical, _, deliverable)| *deliverable && *canonical == needle)
    }
}

fn default_channel_message_timeout_secs() -> u64 {
    300
}

fn default_channel_max_concurrent_per_channel() -> usize {
    4
}

fn default_session_backend() -> String {
    "sqlite".into()
}

impl Default for ChannelsConfig {
    fn default() -> Self {
        Self {
            cli: true,
            telegram: HashMap::new(),
            discord: HashMap::new(),
            slack: HashMap::new(),
            mattermost: HashMap::new(),
            webhook: HashMap::new(),
            imessage: HashMap::new(),
            matrix: HashMap::new(),
            signal: HashMap::new(),
            whatsapp: HashMap::new(),
            linq: HashMap::new(),
            wati: HashMap::new(),
            nextcloud_talk: HashMap::new(),
            email: HashMap::new(),
            gmail_push: HashMap::new(),
            irc: HashMap::new(),
            twitch: HashMap::new(),
            lark: HashMap::new(),
            line: HashMap::new(),
            dingtalk: HashMap::new(),
            wecom: HashMap::new(),
            wecom_ws: HashMap::new(),
            wechat: HashMap::new(),
            qq: HashMap::new(),
            twitter: HashMap::new(),
            mochat: HashMap::new(),
            nostr: HashMap::new(),
            clawdtalk: HashMap::new(),
            reddit: HashMap::new(),
            bluesky: HashMap::new(),
            git: HashMap::new(),
            voice_call: HashMap::new(),
            voice_wake: HashMap::new(),
            voice_duplex: HashMap::new(),
            mqtt: HashMap::new(),
            amqp: HashMap::new(),
            filesystem: HashMap::new(),
            message_timeout_secs: default_channel_message_timeout_secs(),
            max_concurrent_per_channel: default_channel_max_concurrent_per_channel(),
            ack_reactions: true,
            show_tool_calls: false,
            session_persistence: true,
            session_backend: default_session_backend(),
            session_ttl_hours: 0,
            debounce_ms: 0,
        }
    }
}

/// Streaming mode for channels that support progressive message updates.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum StreamMode {
    /// No streaming -- send the complete response as a single message (default).
    #[default]
    Off,
    /// Update a draft message with every flush interval.
    Partial,
    /// Send the response as multiple separate messages at paragraph boundaries.
    #[serde(rename = "multi_message")]
    MultiMessage,
}

/// Where a channel registers its skill slash commands. `global` (default)
/// registers application-wide - the commands work everywhere the bot is, but
/// Discord takes up to ~1h to propagate changes. `guild` registers to each
/// guild in `guild_ids`, which propagates instantly (the right choice for fast
/// iteration and single-server bots). When `guild` is set with an empty
/// `guild_ids`, the channel logs a warning and falls back to global.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum SlashCommandScope {
    /// Application-wide commands (propagate in up to ~1h). Default.
    #[default]
    Global,
    /// Commands registered to each guild in `guild_ids` (instant propagation).
    Guild,
}

fn default_draft_update_interval_ms() -> u64 {
    1000
}

fn default_multi_message_delay_ms() -> u64 {
    800
}

fn default_telegram_approval_timeout_secs() -> u64 {
    120
}

pub const TELEGRAM_OFFICIAL_API_BASE_URL: &str = "https://api.telegram.org";

fn default_telegram_api_base_url() -> String {
    TELEGRAM_OFFICIAL_API_BASE_URL.to_string()
}

fn default_channel_approval_timeout_secs() -> u64 {
    300
}

fn default_matrix_draft_update_interval_ms() -> u64 {
    1500
}

/// Telegram bot channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.telegram"]
pub struct TelegramConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Telegram Bot API token (from @BotFather). `#[serde(default)]` so a
    /// config that omits or later has it pruned (e.g. a freshly created
    /// alias with an empty token, stripped by `prune_empty_leaves` before
    /// write) still deserializes as an empty string - instead of failing
    /// with `missing field 'bot_token'` and getting dropped by the resilient
    /// salvage pass. `validate_bot_token` below still requires a real token
    /// once `enabled = true`.
    #[secret]
    #[tab(Connection)]
    #[serde(default)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub bot_token: String,
    /// Telegram Bot API base URL. Defaults to the official Telegram endpoint;
    /// set to a local Bot API server URL when self-hosting Telegram's bot API.
    #[tab(Connection)]
    #[serde(default = "default_telegram_api_base_url")]
    pub api_base_url: String,
    /// Streaming mode for progressive response delivery via message edits.
    #[tab(Behavior)]
    #[serde(default)]
    pub stream_mode: StreamMode,
    /// Minimum interval (ms) between draft message edits to avoid rate limits.
    #[tab(Behavior)]
    #[serde(default = "default_draft_update_interval_ms")]
    pub draft_update_interval_ms: u64,
    /// Inbound message debounce window in milliseconds for this Telegram alias.
    /// When set, overrides the global `[channels].debounce_ms` for this channel
    /// only. `0` or unset falls back to the global value.
    #[tab(Behavior)]
    #[serde(default)]
    pub debounce_ms: Option<u64>,
    /// When true, a newer Telegram message from the same sender in the same chat
    /// cancels the in-flight request and starts a fresh response with preserved history.
    #[tab(Behavior)]
    #[serde(default)]
    pub interrupt_on_new_message: bool,
    /// When true, only respond to messages that @-mention the bot in groups.
    /// Direct messages are always processed.
    #[tab(Behavior)]
    #[serde(default)]
    pub mention_only: bool,
    /// Override for the top-level `ack_reactions` setting. When `None`, the
    /// channel falls back to `[channels].ack_reactions`. When set
    /// explicitly, it takes precedence.
    #[tab(Behavior)]
    #[serde(default)]
    pub ack_reactions: Option<bool>,
    /// Per-channel proxy URL (http, https, socks5, socks5h).
    /// Overrides the global `[proxy]` setting for this channel only.
    #[tab(Advanced)]
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// How long (seconds) to wait for the operator to tap an inline-keyboard
    /// button on a tool approval prompt before auto-denying. Default: 120.
    #[tab(Behavior)]
    #[serde(default = "default_telegram_approval_timeout_secs")]
    pub approval_timeout_secs: u64,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
    /// Per-(channel, recipient) outbound pacing floor in seconds.
    /// Range: `0..=REPLY_MIN_INTERVAL_MAX_SECS` (0 disables).
    #[serde(default)]
    pub reply_min_interval_secs: u64,
    /// Per-(channel, recipient) outbound pacing queue depth.
    /// Range: `0..=REPLY_QUEUE_DEPTH_CEILING`. When `reply_min_interval_secs > 0`
    /// and this value is `0`, the pacing wrapper substitutes
    /// `DEFAULT_REPLY_QUEUE_DEPTH` (16). When the queue is full, the
    /// newest send is dropped and a `WARN` is logged.
    #[serde(default)]
    pub reply_queue_depth_max: u16,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bot_token: String::new(),
            api_base_url: default_telegram_api_base_url(),
            stream_mode: StreamMode::default(),
            draft_update_interval_ms: default_draft_update_interval_ms(),
            interrupt_on_new_message: false,
            mention_only: false,
            ack_reactions: None,
            proxy_url: None,
            approval_timeout_secs: default_telegram_approval_timeout_secs(),
            excluded_tools: Vec::new(),
            reply_min_interval_secs: 0,
            reply_queue_depth_max: 0,
            debounce_ms: None,
        }
    }
}

impl TelegramConfig {
    /// Validate this alias's bot-token placeholder and enabled-state rules.
    pub fn validate_bot_token(&self, alias: &str) -> Result<()> {
        validate_required_bot_token(
            &format!("channels.telegram.{alias}.bot_token"),
            self.enabled,
            &self.bot_token,
        )
    }
}

impl ChannelConfig for TelegramConfig {
    fn name() -> &'static str {
        "Telegram"
    }
    fn desc() -> &'static str {
        "connect your bot"
    }
}

/// Scope of inbound Discord reaction events the bot records.
///
/// Anything other than `Off` adds the GUILD_MESSAGE_REACTIONS and
/// DIRECT_MESSAGE_REACTIONS gateway intents (both unprivileged; no
/// Developer Portal toggle needed) and archives matching reactions to the
/// channel's `discord.db` sidecar when `archive` is enabled.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum DiscordReactionScope {
    /// Ignore inbound reaction events entirely (no reaction intents requested).
    #[default]
    Off,
    /// Record reactions to the bot's own messages only.
    Own,
    /// Record reactions to any message that passes the channel's filters.
    All,
}

/// Discord bot channel configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.discord"]
#[allow(clippy::struct_excessive_bools)]
pub struct DiscordConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Discord bot token (from Discord Developer Portal). `#[serde(default)]`
    /// for the same reason as `TelegramConfig::bot_token`: a missing token
    /// must deserialize as an empty string instead of salvage-dropping the
    /// alias; `validate_bot_token` still requires a real token once
    /// `enabled = true`.
    #[secret]
    #[tab(Connection)]
    #[serde(default)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub bot_token: String,
    /// Guild (server) IDs to restrict the bot to. Empty = listen across all
    /// guilds the bot is invited to. Migrated from the legacy `guild_id`
    /// singular field.
    #[tab(Advanced)]
    #[serde(default)]
    pub guild_ids: Vec<String>,
    /// Channel IDs to watch. Empty = watch every channel the bot can see.
    /// Used by the archive sidecar (when `archive = true`) and by the
    /// in-channel filter when set.
    #[tab(Advanced)]
    #[serde(default)]
    pub channel_ids: Vec<String>,
    /// When true, the channel opens a sidecar `discord.db` SQLite memory
    /// backend, archives every non-bot message it sees, and registers the
    /// `discord_search` tool against it. Default: false. Folded in from
    /// the legacy `[channels.discord-history]` block.
    #[tab(Advanced)]
    #[serde(default)]
    pub archive: bool,
    /// When true, process messages from other bots (not just humans).
    /// The bot still ignores its own messages to prevent feedback loops.
    #[tab(Advanced)]
    #[serde(default)]
    pub listen_to_bots: bool,
    /// When true, a newer Discord message from the same sender in the same channel
    /// cancels the in-flight request and starts a fresh response with preserved history.
    #[tab(Behavior)]
    #[serde(default)]
    pub interrupt_on_new_message: bool,
    /// When true, only respond to messages that @-mention the bot.
    /// Other messages in the guild are silently ignored.
    #[tab(Behavior)]
    #[serde(default)]
    pub mention_only: bool,
    /// When true, register and serve Discord slash commands (e.g. `/ask`)
    /// over the Gateway WebSocket, in addition to message handling. Default
    /// false. (Prototype: currently registers a single `/ask <prompt>`.)
    #[tab(Behavior)]
    #[serde(default)]
    pub slash_commands: bool,
    /// Scope for registered slash commands: `global` (default, application-wide,
    /// ~1h propagation) or `guild` (registered to each `guild_ids` entry,
    /// instant). Only meaningful when `slash_commands = true`; `guild` with an
    /// empty `guild_ids` warns and falls back to global. Switching scope reaps
    /// owned commands from the now-inactive scope; note that *removing* a guild
    /// from `guild_ids` (without switching scope) does not reap that guild's
    /// commands - remove the bot from the guild, or switch scope, to clear them.
    #[tab(Behavior)]
    #[serde(default)]
    pub slash_command_scope: SlashCommandScope,
    /// Per-channel proxy URL (http, https, socks5, socks5h).
    /// Overrides the global `[proxy]` setting for this channel only.
    #[tab(Advanced)]
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// Streaming mode for progressive response delivery.
    /// `off` (default): single message. `partial`: editable draft updates.
    /// `multi_message`: split response into separate messages at paragraph boundaries.
    #[tab(Behavior)]
    #[serde(default)]
    pub stream_mode: StreamMode,
    /// Minimum interval (ms) between draft message edits to avoid rate limits.
    /// Only used when `stream_mode = "partial"`.
    #[tab(Behavior)]
    #[serde(default = "default_draft_update_interval_ms")]
    pub draft_update_interval_ms: u64,
    /// Delay (ms) between sending each message chunk in multi-message mode.
    /// Only used when `stream_mode = "multi_message"`.
    #[tab(Behavior)]
    #[serde(default = "default_multi_message_delay_ms")]
    pub multi_message_delay_ms: u64,
    /// Stall-watchdog timeout in seconds. When non-zero, the bot will abort
    /// and retry if no progress is made within this duration. 0 = disabled.
    #[tab(Advanced)]
    #[serde(default)]
    pub stall_timeout_secs: u64,
    /// Raw gateway intent mask override. When set, this exact value is sent
    /// in IDENTIFY instead of the derived mask: an operator escape hatch
    /// for downstream deployments that consume gateway events through
    /// custom builds or forks. The operator owns the consequences:
    /// privileged bits still need their Developer Portal toggles, and
    /// dropping baseline bits silences message handling (a warning is
    /// logged). Unset (default) = derive the mask from the channel's
    /// feature config.
    #[tab(Advanced)]
    #[serde(default)]
    pub intents_mask: Option<u64>,
    /// Which inbound reactions to record: `off` (default: reaction events
    /// are not even requested from the gateway), `own` (reactions to the
    /// bot's messages), or `all` (reactions to any message passing the
    /// channel's filters). Recorded reactions are archived to `discord.db`
    /// when `archive` is enabled, and removed again when a user removes
    /// their reaction (bulk clears, remove-all / remove-emoji, are not
    /// yet swept). A set `intents_mask` overrides the derived mask: if it
    /// omits the reaction intents, nothing is recorded.
    #[tab(Behavior)]
    #[serde(default)]
    pub reaction_notifications: DiscordReactionScope,
    /// Seconds to wait for operator approval on `always_ask` tools before auto-denying.
    #[tab(Behavior)]
    #[serde(default = "default_channel_approval_timeout_secs")]
    pub approval_timeout_secs: u64,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
    /// Per-(channel, recipient) outbound pacing floor in seconds.
    /// Range: `0..=REPLY_MIN_INTERVAL_MAX_SECS` (0 disables).
    #[serde(default)]
    pub reply_min_interval_secs: u64,
    /// Per-(channel, recipient) outbound pacing queue depth.
    /// Range: `0..=REPLY_QUEUE_DEPTH_CEILING`. When `reply_min_interval_secs > 0`
    /// and this value is `0`, the pacing wrapper substitutes
    /// `DEFAULT_REPLY_QUEUE_DEPTH` (16). When the queue is full, the
    /// newest send is dropped and a `WARN` is logged.
    #[serde(default)]
    pub reply_queue_depth_max: u16,
}

impl DiscordConfig {
    /// Validate this alias's bot-token placeholder and enabled-state rules.
    /// Mirrors `TelegramConfig::validate_bot_token`.
    pub fn validate_bot_token(&self, alias: &str) -> Result<()> {
        validate_required_bot_token(
            &format!("channels.discord.{alias}.bot_token"),
            self.enabled,
            &self.bot_token,
        )
    }
}

impl ChannelConfig for DiscordConfig {
    fn name() -> &'static str {
        "Discord"
    }
    fn desc() -> &'static str {
        "connect your bot"
    }
}

/// Slack bot channel configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.slack"]
#[allow(clippy::struct_excessive_bools)]
pub struct SlackConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Slack bot OAuth token (xoxb-...). Optional in config: when unset or
    /// empty it is resolved at channel construction from
    /// `ZEROCLAW_SLACK_BOT_TOKEN`, then `SLACK_BOT_TOKEN`. `#[serde(default)]`
    /// so a config that omits it still deserializes - the env fallback then
    /// supplies it - instead of failing with `missing field 'bot_token'`.
    ///
    #[secret]
    #[serde(default)]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub bot_token: Option<String>,
    /// Slack app-level token for Socket Mode (xapp-...). When unset or empty,
    /// resolved at channel construction from `ZEROCLAW_SLACK_APP_TOKEN`, then
    /// `SLACK_APP_TOKEN`. `#[serde(default)]` makes omission explicit (an
    /// `Option` field is already omittable, but this keeps it consistent with
    /// `bot_token`).
    #[secret]
    #[serde(default)]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub app_token: Option<String>,
    /// Explicit list of channel IDs to watch.
    /// Empty = listen across all accessible channels.
    /// Migrated from the legacy `channel_id` singular field.
    #[tab(Advanced)]
    #[serde(default)]
    pub channel_ids: Vec<String>,
    /// When true, a newer Slack message from the same sender in the same channel
    /// cancels the in-flight request and starts a fresh response with preserved history.
    #[tab(Behavior)]
    #[serde(default)]
    pub interrupt_on_new_message: bool,
    /// When true (default), replies stay in the originating Slack thread.
    /// When false, replies go to the channel root instead.
    #[tab(Advanced)]
    #[serde(default)]
    pub thread_replies: Option<bool>,
    /// When true, only respond to messages that @-mention the bot in groups.
    /// Direct messages remain allowed.
    #[tab(Behavior)]
    #[serde(default)]
    pub mention_only: bool,
    /// When true (and `mention_only` is also true), messages inside a Slack
    /// thread must also @-mention the bot to trigger a response. By default,
    /// thread replies are allowed through without a mention so the bot can
    /// keep a back-and-forth going without the user repeating @-mentions.
    /// Set this to true in channels shared with human discussion where the
    /// bot should stay silent unless explicitly addressed.
    #[tab(Advanced)]
    #[serde(default)]
    pub strict_mention_in_thread: bool,
    /// Use the newer Slack `markdown` block type (12 000 char limit, richer formatting).
    /// Defaults to false (uses universally supported `section` blocks with `mrkdwn`).
    /// Enable this only if your Slack workspace supports the `markdown` block type.
    #[tab(Advanced)]
    #[serde(default)]
    pub use_markdown_blocks: bool,
    /// Per-channel proxy URL (http, https, socks5, socks5h).
    /// Overrides the global `[proxy]` setting for this channel only.
    #[tab(Advanced)]
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// Enable progressive draft message streaming via `chat.update`.
    #[tab(Behavior)]
    #[serde(default)]
    pub stream_drafts: bool,
    /// Minimum interval (ms) between draft message edits to avoid Slack rate limits.
    #[tab(Behavior)]
    #[serde(default = "default_slack_draft_update_interval_ms")]
    pub draft_update_interval_ms: u64,
    /// Emoji reaction name (without colons) that cancels an in-flight request.
    /// For example, `"x"` means reacting with `:x:` cancels the task.
    /// Leave unset to disable reaction-based cancellation.
    #[tab(Advanced)]
    #[serde(default)]
    pub cancel_reaction: Option<String>,
    /// Seconds to wait for operator approval on `always_ask` tools before auto-denying.
    #[tab(Behavior)]
    #[serde(default = "default_channel_approval_timeout_secs")]
    pub approval_timeout_secs: u64,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
    /// Per-(channel, recipient) outbound pacing floor in seconds.
    /// Range: `0..=REPLY_MIN_INTERVAL_MAX_SECS` (0 disables).
    #[serde(default)]
    pub reply_min_interval_secs: u64,
    /// Per-(channel, recipient) outbound pacing queue depth.
    /// Range: `0..=REPLY_QUEUE_DEPTH_CEILING`. When `reply_min_interval_secs > 0`
    /// and this value is `0`, the pacing wrapper substitutes
    /// `DEFAULT_REPLY_QUEUE_DEPTH` (16). When the queue is full, the
    /// newest send is dropped and a `WARN` is logged.
    #[serde(default)]
    pub reply_queue_depth_max: u16,
}

fn default_slack_draft_update_interval_ms() -> u64 {
    1200
}

impl ChannelConfig for SlackConfig {
    fn name() -> &'static str {
        "Slack"
    }
    fn desc() -> &'static str {
        "connect your bot"
    }
}

impl SlackConfig {
    /// Resolve the effective Slack bot token: the configured `bot_token` when
    /// set and non-empty, else the `ZEROCLAW_SLACK_BOT_TOKEN` env var, else
    /// `SLACK_BOT_TOKEN`. Returns `None` when none is available. Resolving here
    /// (rather than writing the env value back into the config struct) keeps an
    /// env-supplied secret out of any config that might later be persisted to
    /// disk.
    pub fn resolved_bot_token(&self) -> Option<String> {
        resolve_slack_token(self.bot_token.as_deref(), "BOT")
    }

    /// Resolve the effective Slack app-level (Socket Mode) token: configured
    /// `app_token` when set and non-empty, else `ZEROCLAW_SLACK_APP_TOKEN`,
    /// else `SLACK_APP_TOKEN`. Returns `None` when none is available.
    pub fn resolved_app_token(&self) -> Option<String> {
        resolve_slack_token(self.app_token.as_deref(), "APP")
    }
}

/// Shared token resolution for [`SlackConfig`]: a non-empty configured value
/// wins; otherwise the `ZEROCLAW_SLACK_<KIND>_TOKEN` env var takes precedence
/// over the conventional `SLACK_<KIND>_TOKEN`. Blank values (config or env) are
/// treated as absent. `kind` is `"BOT"` or `"APP"`.
fn resolve_slack_token(configured: Option<&str>, kind: &str) -> Option<String> {
    if let Some(value) = configured {
        let value = value.trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    for var in [
        format!("ZEROCLAW_SLACK_{kind}_TOKEN"),
        format!("SLACK_{kind}_TOKEN"),
    ] {
        if let Ok(value) = std::env::var(&var) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// How the Mattermost channel receives inbound messages.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum MattermostListenMode {
    /// Poll REST API every 3 seconds. The default — all existing configs
    /// continue to work without changes.
    #[default]
    Polling,
    /// Connect to `/api/v4/websocket` for near-real-time event delivery.
    Websocket,
}

/// Mattermost bot channel configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.mattermost"]
pub struct MattermostConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Mattermost server URL (e.g. `"https://mattermost.example.com"`).
    #[tab(Connection)]
    pub url: String,
    /// Mattermost bot access token. When unset, the channel falls back to
    /// the login flow using `login_id` + `password`.
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    #[serde(default)]
    pub bot_token: Option<String>,
    /// Login ID (email or username) for the password login flow. Used only
    /// when `bot_token` is unset; both `login_id` and `password` must be
    /// set together.
    #[tab(Connection)]
    #[serde(default)]
    pub login_id: Option<String>,
    /// Account password for the login flow. Used only when `bot_token` is
    /// unset; both `login_id` and `password` must be set together.
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    #[serde(default)]
    pub password: Option<String>,
    /// Channel IDs to restrict the bot to. Empty or `["*"]` = auto-discover
    /// every channel the bot can read (public, private, DMs, group DMs) and
    /// poll them all. Explicit IDs disable discovery and pin the bot to the
    /// listed channels only. Migrated from the legacy `channel_id` singular
    /// field.
    #[tab(Advanced)]
    #[serde(default)]
    pub channel_ids: Vec<String>,
    /// Team IDs to restrict auto-discovery to. Empty = discover across every
    /// team the bot belongs to. Non-empty = only discover public/private
    /// channels whose `team_id` is in this list. DMs and group DMs (which
    /// have no team) are governed by `discover_dms` instead.
    #[tab(Advanced)]
    #[serde(default)]
    pub team_ids: Vec<String>,
    /// When true (default), auto-discovery includes DM (`type=D`) and group
    /// DM (`type=G`) channels. Set false to restrict the bot to public and
    /// private team channels only. Has no effect when `channel_ids` lists
    /// explicit IDs. Defaults to `true` at the call site via
    /// `discover_dms.unwrap_or(true)`.
    #[tab(Advanced)]
    #[serde(default)]
    pub discover_dms: Option<bool>,
    /// When true (default), replies thread on the original post.
    /// When false, replies go to the channel root.
    #[tab(Advanced)]
    #[serde(default)]
    pub thread_replies: Option<bool>,
    /// When true, only respond to messages that @-mention the bot. Other
    /// messages in the channel are silently ignored. DM and group DM
    /// channels always bypass this filter: a 1:1 (or small-group) direct
    /// conversation has no ambient noise to gate against, so every message
    /// is treated as addressed to the bot.
    #[tab(Behavior)]
    #[serde(default)]
    pub mention_only: Option<bool>,
    /// When true, a newer Mattermost message from the same sender in the same channel
    /// cancels the in-flight request and starts a fresh response with preserved history.
    #[tab(Behavior)]
    #[serde(default)]
    pub interrupt_on_new_message: bool,
    /// Per-channel proxy URL (http, https, socks5, socks5h).
    /// Overrides the global `[proxy]` setting for this channel only.
    #[tab(Advanced)]
    #[serde(default)]
    pub proxy_url: Option<String>,

    /// Listen mode: `"polling"` (REST API every 3s, default) or `"websocket"`
    /// (persistent WebSocket connection to `/api/v4/websocket` for near-real-time
    /// event delivery). WebSocket mode reduces server load and delivers events
    /// faster but requires a WebSocket-capable Mattermost server (v4.0+).
    #[serde(default)]
    pub listen_mode: MattermostListenMode,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
    /// Per-(channel, recipient) outbound pacing floor in seconds.
    /// Range: `0..=REPLY_MIN_INTERVAL_MAX_SECS` (0 disables).
    #[serde(default)]
    pub reply_min_interval_secs: u64,
    /// Per-(channel, recipient) outbound pacing queue depth.
    /// Range: `0..=REPLY_QUEUE_DEPTH_CEILING`. When `reply_min_interval_secs > 0`
    /// and this value is `0`, the pacing wrapper substitutes
    /// `DEFAULT_REPLY_QUEUE_DEPTH` (16). When the queue is full, the
    /// newest send is dropped and a `WARN` is logged.
    #[serde(default)]
    pub reply_queue_depth_max: u16,
}

impl ChannelConfig for MattermostConfig {
    fn name() -> &'static str {
        "Mattermost"
    }
    fn desc() -> &'static str {
        "connect to your bot"
    }
}

/// Webhook channel configuration.
///
/// Receives messages via HTTP POST and sends replies to a configurable outbound URL.
/// This is the "universal adapter" for any system that supports webhooks.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.webhook"]
pub struct WebhookConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Port to listen on for incoming webhooks.
    #[tab(Advanced)]
    #[serde(default = "default_webhook_channel_port")]
    pub port: u16,
    /// URL path to listen on (default: `/webhook`).
    #[tab(Advanced)]
    #[serde(default)]
    pub listen_path: Option<String>,
    /// URL to POST/PUT outbound messages to.
    #[tab(Advanced)]
    #[serde(default)]
    pub send_url: Option<String>,
    /// HTTP method for outbound messages (`POST` or `PUT`). Default: `POST`.
    #[tab(Advanced)]
    #[serde(default)]
    pub send_method: Option<String>,
    /// Optional `Authorization` header value for outbound requests.
    #[tab(Connection)]
    #[serde(default)]
    #[secret]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub auth_header: Option<String>,
    /// Shared secret for webhook signature verification (HMAC-SHA256).
    /// The channel will refuse to start without one. Set
    /// `[channels.webhook.<alias>].secret` in config.
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub secret: Option<String>,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
    /// Per-(channel, recipient) outbound pacing floor in seconds.
    /// Range: `0..=REPLY_MIN_INTERVAL_MAX_SECS` (0 disables).
    #[serde(default)]
    pub reply_min_interval_secs: u64,
    /// Per-(channel, recipient) outbound pacing queue depth.
    /// Range: `0..=REPLY_QUEUE_DEPTH_CEILING`. When `reply_min_interval_secs > 0`
    /// and this value is `0`, the pacing wrapper substitutes
    /// `DEFAULT_REPLY_QUEUE_DEPTH` (16). When the queue is full, the
    /// newest send is dropped and a `WARN` is logged.
    #[serde(default)]
    pub reply_queue_depth_max: u16,

    /// Maximum number of retry attempts for outbound sends on transient failures
    /// (network errors, 429, 5xx). Set to `0` to disable retries. Default: `3`.
    #[serde(default)]
    pub max_retries: Option<u32>,
    /// Base delay in milliseconds for exponential backoff between retries. Default: `500`.
    /// Values below `1` are clamped to `1ms` at runtime to avoid busy-retry loops.
    #[serde(default)]
    pub retry_base_delay_ms: Option<u64>,
    /// Maximum delay cap in milliseconds for any single retry wait. Default: `30000` (30s).
    /// Values below `1` are clamped to `1ms` at runtime to avoid busy-retry loops.
    #[serde(default)]
    pub retry_max_delay_ms: Option<u64>,
}

fn default_webhook_channel_port() -> u16 {
    8090
}

impl ChannelConfig for WebhookConfig {
    fn name() -> &'static str {
        "Webhook"
    }
    fn desc() -> &'static str {
        "HTTP endpoint"
    }
}

/// iMessage channel configuration (macOS only).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.imessage"]
pub struct IMessageConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
    /// Per-(channel, recipient) outbound pacing floor in seconds.
    /// Range: `0..=REPLY_MIN_INTERVAL_MAX_SECS` (0 disables).
    #[serde(default)]
    pub reply_min_interval_secs: u64,
    /// Per-(channel, recipient) outbound pacing queue depth.
    /// Range: `0..=REPLY_QUEUE_DEPTH_CEILING`. When `reply_min_interval_secs > 0`
    /// and this value is `0`, the pacing wrapper substitutes
    /// `DEFAULT_REPLY_QUEUE_DEPTH` (16). When the queue is full, the
    /// newest send is dropped and a `WARN` is logged.
    #[serde(default)]
    pub reply_queue_depth_max: u16,
}

impl ChannelConfig for IMessageConfig {
    fn name() -> &'static str {
        "iMessage"
    }
    fn desc() -> &'static str {
        "macOS only"
    }
}

/// Matrix channel configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.matrix"]
pub struct MatrixConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Matrix homeserver URL (e.g. `"https://matrix.org"`).
    #[tab(Connection)]
    pub homeserver: String,
    /// Matrix access token for the bot account. When unset, the channel
    /// falls back to password login using `user_id` + `password`.
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    #[serde(default)]
    pub access_token: Option<String>,
    /// Optional Matrix user ID (e.g. `"@bot:matrix.org"`).
    #[tab(Connection)]
    #[serde(default)]
    pub user_id: Option<String>,
    /// Optional Matrix device ID.
    #[tab(Connection)]
    #[serde(default)]
    pub device_id: Option<String>,
    /// Allowed Matrix room IDs. Empty = allow all rooms the bot has joined.
    /// Entries are matched literally against the canonical room ID
    /// (`!abc:server`) of each incoming message; `#room:server` aliases are
    /// not resolved for this allowlist (they are resolved only for outbound
    /// delivery targets such as cron `delivery.to`).
    #[tab(Behavior)]
    #[serde(default)]
    pub allowed_rooms: Vec<String>,
    /// Whether to interrupt an in-flight agent response when a new message arrives.
    #[tab(Behavior)]
    #[serde(default)]
    pub interrupt_on_new_message: bool,
    /// Streaming mode for progressive response delivery.
    /// `"off"` (default): single message. `"partial"`: edit-in-place draft.
    /// `"multi_message"`: paragraph-split delivery.
    #[tab(Behavior)]
    #[serde(default)]
    pub stream_mode: StreamMode,
    /// Minimum interval (ms) between draft message edits in Partial mode.
    #[tab(Behavior)]
    #[serde(default = "default_matrix_draft_update_interval_ms")]
    pub draft_update_interval_ms: u64,
    /// Delay (ms) between sending each paragraph in MultiMessage mode.
    #[tab(Behavior)]
    #[serde(default = "default_multi_message_delay_ms")]
    pub multi_message_delay_ms: u64,
    /// When true, only respond to messages that @-mention the bot in groups.
    /// Direct messages are always processed.
    #[tab(Behavior)]
    #[serde(default)]
    pub mention_only: bool,
    /// Optional Matrix recovery key for automatic E2EE key backup restore.
    /// When set, ZeroClaw recovers room keys and cross-signing secrets on startup.
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    #[serde(default)]
    pub recovery_key: Option<String>,
    /// Optional login password for Matrix account (used for initial login flow).
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    #[serde(default)]
    pub password: Option<String>,
    /// Seconds to wait for operator approval on `always_ask` tools before auto-denying.
    #[tab(Behavior)]
    #[serde(default = "default_channel_approval_timeout_secs")]
    pub approval_timeout_secs: u64,
    /// When true (default), replies are sent as thread replies. Starts a new thread from the
    /// incoming message when none exists. When false, only continues existing threads.
    #[tab(Behavior)]
    #[serde(default = "default_true")]
    pub reply_in_thread: bool,
    /// Override for the top-level `[channels].ack_reactions`. When
    /// `None`, falls back to the channels-wide default. When set
    /// explicitly (`true`/`false`), takes precedence for this Matrix
    /// instance only.
    #[tab(Behavior)]
    #[serde(default)]
    pub ack_reactions: Option<bool>,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
    /// Per-(channel, recipient) outbound pacing floor in seconds.
    /// Range: `0..=REPLY_MIN_INTERVAL_MAX_SECS` (0 disables).
    #[serde(default)]
    pub reply_min_interval_secs: u64,
    /// Per-(channel, recipient) outbound pacing queue depth.
    /// Range: `0..=REPLY_QUEUE_DEPTH_CEILING`. When `reply_min_interval_secs > 0`
    /// and this value is `0`, the pacing wrapper substitutes
    /// `DEFAULT_REPLY_QUEUE_DEPTH` (16). When the queue is full, the
    /// newest send is dropped and a `WARN` is logged.
    #[serde(default)]
    pub reply_queue_depth_max: u16,
}

impl ChannelConfig for MatrixConfig {
    fn name() -> &'static str {
        "Matrix"
    }
    fn desc() -> &'static str {
        "self-hosted chat"
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.signal"]
pub struct SignalConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Base URL for the signal-cli HTTP daemon (e.g. `"http://127.0.0.1:8686"`).
    #[tab(Connection)]
    pub http_url: String,
    /// E.164 phone number of the signal-cli account (e.g. "+1234567890").
    #[tab(Connection)]
    pub account: String,
    /// Group IDs to filter messages. Empty = accept all messages (DMs and
    /// groups). When non-empty, only messages from listed groups are
    /// accepted (DMs are still accepted unless `dm_only` flips the policy
    /// to DMs-only). Migrated from the legacy `group_id` singular field.
    #[tab(Advanced)]
    #[serde(default)]
    pub group_ids: Vec<String>,
    /// When true, only accept direct messages and ignore all group traffic.
    /// Mutually exclusive with `group_ids` (which is ignored when this is
    /// set). Migrated from the legacy `group_id = "dm"` sentinel.
    #[tab(Advanced)]
    #[serde(default)]
    pub dm_only: bool,
    /// Skip messages that are attachment-only (no text body).
    #[tab(Advanced)]
    #[serde(default)]
    pub ignore_attachments: bool,
    /// Skip incoming story messages.
    #[tab(Advanced)]
    #[serde(default)]
    pub ignore_stories: bool,
    /// Per-channel proxy URL (http, https, socks5, socks5h).
    /// Overrides the global `[proxy]` setting for this channel only.
    #[tab(Advanced)]
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// Seconds to wait for operator approval on `always_ask` tools before auto-denying.
    #[tab(Behavior)]
    #[serde(default = "default_channel_approval_timeout_secs")]
    pub approval_timeout_secs: u64,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
    /// Per-(channel, recipient) outbound pacing floor in seconds.
    /// Range: `0..=REPLY_MIN_INTERVAL_MAX_SECS` (0 disables).
    #[serde(default)]
    pub reply_min_interval_secs: u64,
    /// Per-(channel, recipient) outbound pacing queue depth.
    /// Range: `0..=REPLY_QUEUE_DEPTH_CEILING`. When `reply_min_interval_secs > 0`
    /// and this value is `0`, the pacing wrapper substitutes
    /// `DEFAULT_REPLY_QUEUE_DEPTH` (16). When the queue is full, the
    /// newest send is dropped and a `WARN` is logged.
    #[serde(default)]
    pub reply_queue_depth_max: u16,
}

impl ChannelConfig for SignalConfig {
    fn name() -> &'static str {
        "Signal"
    }
    fn desc() -> &'static str {
        "An open-source, encrypted messaging service"
    }
}

/// WhatsApp Web usage mode.
///
/// `Personal` treats the account as a personal phone — the bot only responds to
/// incoming messages that pass the DM/group/self-chat policy filters.
/// `Business` (default) responds to all incoming messages, subject only to the
/// `allowed_numbers` allowlist.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, zeroclaw_macros::ConfigEnum)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WhatsAppWebMode {
    /// Respond to all messages passing the allowlist (default).
    #[default]
    Business,
    /// Apply per-chat-type policies (dm_policy, group_policy, self_chat_mode).
    Personal,
}

/// Policy for a particular WhatsApp chat type (DMs or groups) when
/// `mode = "personal"`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, zeroclaw_macros::ConfigEnum)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WhatsAppChatPolicy {
    /// Only respond to senders on the `allowed_numbers` list (default).
    #[default]
    Allowlist,
    /// Ignore all messages in this chat type.
    Ignore,
    /// Respond to every message regardless of allowlist.
    All,
}

/// WhatsApp channel configuration (Cloud API or Web mode).
///
/// Set `phone_number_id` for Cloud API mode, or `session_path` for Web mode.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.whatsapp"]
pub struct WhatsAppConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Access token from Meta Business Suite (Cloud API mode)
    #[serde(default)]
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub access_token: Option<String>,
    /// Phone number ID from Meta Business API (Cloud API mode)
    #[tab(Connection)]
    #[serde(default)]
    pub phone_number_id: Option<String>,
    /// Webhook verify token (you define this, Meta sends it back for verification)
    /// Only used in Cloud API mode
    #[serde(default)]
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub verify_token: Option<String>,
    /// App secret from Meta Business Suite (for webhook signature verification)
    /// Can also be set via `ZEROCLAW_WHATSAPP_APP_SECRET` environment variable
    /// Only used in Cloud API mode
    #[serde(default)]
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub app_secret: Option<String>,
    /// Session database path for WhatsApp Web client (Web mode)
    /// When set, enables native WhatsApp Web mode with wa-rs
    #[tab(Connection)]
    #[serde(default)]
    pub session_path: Option<String>,
    /// Phone number for pair code linking (Web mode, optional)
    /// Format: country code + number (e.g., "15551234567")
    /// If not set, QR code pairing will be used
    #[tab(Connection)]
    #[serde(default)]
    pub pair_phone: Option<String>,
    /// Custom pair code for linking (Web mode, optional)
    /// Leave empty to let WhatsApp generate one
    #[tab(Connection)]
    #[serde(default)]
    pub pair_code: Option<String>,
    /// Override the WhatsApp Web WebSocket URL (Web mode, optional). Used
    /// by integration tests and proxy setups; leave unset to use the
    /// default endpoint that ships with `wa-rs`.
    #[tab(Connection)]
    #[serde(default)]
    pub ws_url: Option<String>,
    /// When true, only respond to messages that @-mention the bot in groups (Web mode only).
    /// Direct messages are always processed.
    /// Bot identity is resolved from the wa-rs device at runtime; `pair_phone` seeds it on first connect.
    #[tab(Behavior)]
    #[serde(default)]
    pub mention_only: bool,
    /// When true in WhatsApp Web group chats, unaddressed messages that pass
    /// sender/chat authorization are recorded as passive conversation context
    /// without starting an agent turn. Default: `false`.
    #[tab(Behavior)]
    #[serde(default)]
    pub passive_group_context: bool,
    /// Cancel an in-flight response from this channel sender when a newer
    /// WhatsApp message arrives. Default: `false`.
    #[serde(default)]
    pub interrupt_on_new_message: bool,
    /// Usage mode for WhatsApp Web: "business" (default) or "personal".
    /// In personal mode the bot applies dm_policy, group_policy, and
    /// self_chat_mode to decide which chats to respond in.
    #[tab(Advanced)]
    #[serde(default)]
    pub mode: WhatsAppWebMode,
    /// Policy for direct messages when mode = "personal".
    /// "allowlist" (default) | "ignore" | "all".
    #[tab(Advanced)]
    #[serde(default)]
    pub dm_policy: WhatsAppChatPolicy,
    /// Policy for group chats when mode = "personal".
    /// "allowlist" (default) | "ignore" | "all".
    #[tab(Advanced)]
    #[serde(default)]
    pub group_policy: WhatsAppChatPolicy,
    /// When true and mode = "personal", always respond to messages in the
    /// user's own self-chat (Notes to Self). Defaults to false.
    #[tab(Advanced)]
    #[serde(default)]
    pub self_chat_mode: bool,
    /// Regex patterns for DM mention gating (case-insensitive).
    /// When non-empty, only direct messages matching at least one pattern are
    /// processed; matched fragments are stripped from the forwarded content.
    /// Example: `["@?ZeroClaw", "\\+?15555550123"]`
    #[tab(Advanced)]
    #[serde(default)]
    pub dm_mention_patterns: Vec<String>,
    /// Regex patterns for group-chat mention gating (case-insensitive).
    /// When non-empty, only group messages matching at least one pattern are
    /// processed; matched fragments are stripped from the forwarded content.
    /// Example: `["@?ZeroClaw", "\\+?15555550123"]`
    #[tab(Advanced)]
    #[serde(default)]
    pub group_mention_patterns: Vec<String>,
    /// Allowed group chats by JID (Web mode). An empty list (the default)
    /// permits all groups; a non-empty list drops every group message whose
    /// chat JID matches no entry. Each entry matches either the full group
    /// JID (`123456789012345@g.us`) or the JID user part - the segment before
    /// `@` (`123456789012345`) - compared exactly, not as a string prefix.
    /// Direct messages bypass this filter regardless of list contents.
    /// Modeled on the Matrix channel's `allowed_rooms`; it gates group
    /// identity, which `dm_policy`/`group_policy` (chat type) and the
    /// sender allowlist (sender) do not.
    #[tab(Advanced)]
    #[serde(default)]
    pub allowed_groups: Vec<String>,
    /// Per-channel proxy URL (http, https, socks5, socks5h).
    /// Overrides the global `[proxy]` setting for this channel only.
    #[tab(Advanced)]
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// Seconds to wait for operator approval on `always_ask` tools before auto-denying.
    #[tab(Behavior)]
    #[serde(default = "default_channel_approval_timeout_secs")]
    pub approval_timeout_secs: u64,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
    /// Per-(channel, recipient) outbound pacing floor in seconds.
    /// Range: `0..=REPLY_MIN_INTERVAL_MAX_SECS` (0 disables).
    #[serde(default)]
    pub reply_min_interval_secs: u64,
    /// Per-(channel, recipient) outbound pacing queue depth.
    /// Range: `0..=REPLY_QUEUE_DEPTH_CEILING`. When `reply_min_interval_secs > 0`
    /// and this value is `0`, the pacing wrapper substitutes
    /// `DEFAULT_REPLY_QUEUE_DEPTH` (16). When the queue is full, the
    /// newest send is dropped and a `WARN` is logged.
    #[serde(default)]
    pub reply_queue_depth_max: u16,
}

impl ChannelConfig for WhatsAppConfig {
    fn name() -> &'static str {
        "WhatsApp"
    }
    fn desc() -> &'static str {
        "Business Cloud API"
    }
}

impl_reply_pacing!(
    TelegramConfig,
    DiscordConfig,
    SlackConfig,
    MattermostConfig,
    WebhookConfig,
    IMessageConfig,
    MatrixConfig,
    SignalConfig,
    WhatsAppConfig,
);

#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.linq"]
pub struct LinqConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Linq Partner API token (Bearer auth)
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_token: String,
    /// Phone number to send from (E.164 format)
    #[tab(Advanced)]
    pub from_phone: String,
    /// Webhook signing secret for signature verification
    #[serde(default)]
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub signing_secret: Option<String>,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
}

impl ChannelConfig for LinqConfig {
    fn name() -> &'static str {
        "Linq"
    }
    fn desc() -> &'static str {
        "iMessage/RCS/SMS via Linq API"
    }
}

/// WATI WhatsApp Business API channel configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.wati"]
pub struct WatiConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// WATI API token (Bearer auth).
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_token: String,
    /// WATI API base URL (default: <https://live-mt-server.wati.io>).
    #[tab(Advanced)]
    #[serde(default = "default_wati_api_url")]
    pub api_url: String,
    /// Tenant ID for multi-channel setups (optional).
    #[tab(Advanced)]
    #[serde(default)]
    pub tenant_id: Option<String>,
    /// Per-channel proxy URL (http, https, socks5, socks5h).
    /// Overrides the global `[proxy]` setting for this channel only.
    #[tab(Advanced)]
    #[serde(default)]
    pub proxy_url: Option<String>,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
}

fn default_wati_api_url() -> String {
    "https://live-mt-server.wati.io".to_string()
}

impl ChannelConfig for WatiConfig {
    fn name() -> &'static str {
        "WATI"
    }
    fn desc() -> &'static str {
        "WhatsApp via WATI Business API"
    }
}

/// Nextcloud Talk bot configuration (webhook receive + OCS send API).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.nextcloud_talk"]
pub struct NextcloudTalkConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Nextcloud base URL (e.g. `"https://cloud.example.com"`).
    #[tab(Connection)]
    pub base_url: String,
    /// Bot app token used for OCS API bearer auth.
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub app_token: String,
    /// Shared secret for webhook signature verification.
    ///
    /// Can also be set via `ZEROCLAW_NEXTCLOUD_TALK_WEBHOOK_SECRET`.
    #[serde(default)]
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub webhook_secret: Option<String>,
    /// Per-channel proxy URL (http, https, socks5, socks5h).
    /// Overrides the global `[proxy]` setting for this channel only.
    #[tab(Advanced)]
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// Display name of the bot in Nextcloud Talk (e.g. "zeroclaw").
    /// Used to filter out the bot's own messages and prevent feedback loops.
    /// If not set, defaults to an empty string (no self-message filtering by name).
    #[tab(Advanced)]
    #[serde(default)]
    pub bot_name: Option<String>,
    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
    /// Controls whether and how streaming draft updates are delivered.
    ///
    /// - `"off"` (default): responses are sent as a single final message.
    /// - `"partial"`: a placeholder is posted first and edited incrementally
    ///   as tokens arrive, making long responses visible in real time.
    #[tab(Behavior)]
    #[serde(default)]
    pub stream_mode: StreamMode,
    /// Minimum interval in milliseconds between consecutive OCS edit calls per
    /// room when `stream_mode = "partial"`. Default: 1000 ms.
    #[tab(Behavior)]
    #[serde(default = "default_draft_update_interval_ms")]
    pub draft_update_interval_ms: u64,
}

impl ChannelConfig for NextcloudTalkConfig {
    fn name() -> &'static str {
        "NextCloud Talk"
    }
    fn desc() -> &'static str {
        "NextCloud Talk platform"
    }
}

impl WhatsAppConfig {
    /// Detect which backend to use based on config fields.
    /// Cloud API when `phone_number_id` is set; otherwise Web when any
    /// Web-only selector (`session_path`, `pair_phone`, `pair_code`,
    /// `ws_url`, or `mode = personal`) is present. Falls back to Cloud
    /// for an otherwise-empty config (env-injected Cloud credentials).
    pub fn backend_type(&self) -> &'static str {
        if self.phone_number_id.is_some() {
            "cloud"
        } else if self.has_web_selector() {
            "web"
        } else {
            "cloud"
        }
    }

    /// Check if this is a valid Cloud API config
    pub fn is_cloud_config(&self) -> bool {
        self.phone_number_id.is_some() && self.access_token.is_some() && self.verify_token.is_some()
    }

    /// Any Web-only selector that signals WhatsApp Web intent. `mode`
    /// defaults to `Business` on every config, so only a non-default
    /// `Personal` mode counts; the rest are `Option` fields absent by
    /// default.
    pub fn has_web_selector(&self) -> bool {
        self.session_path.is_some()
            || self.pair_phone.is_some()
            || self.pair_code.is_some()
            || self.ws_url.is_some()
            || self.mode == WhatsAppWebMode::Personal
    }

    /// Check if this is a valid Web config. The Web client defaults a
    /// missing `session_path`, so any Web selector is sufficient.
    pub fn is_web_config(&self) -> bool {
        self.has_web_selector()
    }

    /// Returns true when both Cloud and Web selectors are present.
    ///
    /// Runtime currently prefers Cloud mode in this case for backward compatibility.
    pub fn is_ambiguous_config(&self) -> bool {
        self.phone_number_id.is_some() && self.has_web_selector()
    }
}

/// MQTT channel configuration.
///
/// Subscribes to MQTT topics and feeds incoming messages into the agent
/// loop. The former SOP-listener fan-in (which dispatched messages to the
/// SOP engine) was removed with the run side; its config section is a
/// parse-time tombstone that rejects legacy blocks with a migration
/// message.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.mqtt"]
pub struct MqttConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// MQTT broker URL (e.g., `mqtt://localhost:1883` or `mqtts://broker.example.com:8883`).
    /// Use `mqtt://` for plain connections or `mqtts://` for TLS.
    #[tab(Connection)]
    pub broker_url: String,
    /// MQTT client ID (must be unique per broker).
    #[tab(Advanced)]
    pub client_id: String,
    /// Topics to subscribe to (e.g., `sensors/#`, `alerts/+/critical`).
    /// At least one topic is required.
    #[tab(Advanced)]
    #[serde(default)]
    pub topics: Vec<String>,
    /// MQTT QoS level (0 = at-most-once, 1 = at-least-once, 2 = exactly-once). Default: 1.
    #[tab(Advanced)]
    #[serde(default = "default_mqtt_qos")]
    pub qos: u8,
    /// Username for authentication (optional).
    #[tab(Connection)]
    pub username: Option<String>,
    /// Password for authentication (optional).
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub password: Option<String>,
    /// Enable TLS encryption. Must match the broker_url scheme:
    /// - `mqtt://` → `use_tls: false`
    /// - `mqtts://` → `use_tls: true`
    #[tab(Advanced)]
    #[serde(default)]
    pub use_tls: bool,
    /// Keep-alive interval in seconds (default: 30). Prevents broker disconnect on idle.
    #[tab(Advanced)]
    #[serde(default = "default_mqtt_keep_alive_secs")]
    pub keep_alive_secs: u64,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
}

impl MqttConfig {
    /// Validate the MQTT configuration.
    ///
    /// Checks:
    /// - QoS is 0, 1, or 2
    /// - broker_url uses valid scheme (`mqtt://` or `mqtts://`)
    /// - `use_tls` flag matches broker_url scheme
    /// - At least one topic is configured
    /// - client_id is non-empty
    pub fn validate(&self) -> anyhow::Result<()> {
        // QoS validation
        if self.qos > 2 {
            anyhow::bail!("qos must be 0, 1, or 2, got {}", self.qos);
        }

        // Broker URL validation
        let is_tls_scheme = self.broker_url.starts_with("mqtts://");
        let is_mqtt_scheme = self.broker_url.starts_with("mqtt://");

        if !is_tls_scheme && !is_mqtt_scheme {
            anyhow::bail!(
                "broker_url must start with 'mqtt://' or 'mqtts://', got: {}",
                self.broker_url
            );
        }

        // TLS flag validation
        if is_mqtt_scheme && self.use_tls {
            anyhow::bail!("use_tls is true but broker_url uses 'mqtt://' (not 'mqtts://')");
        }

        if is_tls_scheme && !self.use_tls {
            anyhow::bail!(
                "use_tls is false but broker_url uses 'mqtts://' (requires use_tls: true)"
            );
        }

        // Topics validation
        if self.topics.is_empty() {
            anyhow::bail!("at least one topic must be configured");
        }

        // Client ID validation
        if self.client_id.is_empty() {
            validation_bail!(
                RequiredFieldEmpty,
                "client_id",
                "client_id must not be empty"
            );
        }

        Ok(())
    }
}

impl ChannelConfig for MqttConfig {
    fn name() -> &'static str {
        "MQTT"
    }
    fn desc() -> &'static str {
        "MQTT SOP Listener"
    }
}

fn default_mqtt_qos() -> u8 {
    1
}

fn default_mqtt_keep_alive_secs() -> u64 {
    30
}

/// Filesystem channel configuration.
///
/// Watches configured paths and feeds file create/modify/delete/rename
/// events into the agent loop. The former SOP-listener fan-in (which
/// dispatched these events to the SOP engine) was removed with the run
/// side; its config section is a parse-time tombstone that rejects legacy
/// blocks with a migration message.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.filesystem"]
pub struct FilesystemConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Root paths to watch. At least one is required.
    #[tab(Connection)]
    #[serde(default)]
    pub paths: Vec<String>,
    /// Watch nested paths beneath each root.
    #[tab(Behavior)]
    #[serde(default = "default_true")]
    pub recursive: bool,
    /// Glob patterns to include. Empty matches all paths under the roots.
    #[tab(Behavior)]
    #[serde(default)]
    pub include: Vec<String>,
    /// Glob patterns to exclude. Applied after `include`.
    #[tab(Behavior)]
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Event kinds to emit (`created`, `modified`, `deleted`, `renamed`).
    #[tab(Behavior)]
    #[serde(default = "default_filesystem_events")]
    pub events: Vec<String>,
    /// Collapse rapid repeated events per path/kind within this window.
    #[tab(Advanced)]
    #[serde(default = "default_filesystem_debounce_ms")]
    pub debounce_ms: u64,
    /// Wait this long after the last event before reading metadata or content,
    /// giving atomic writes and slow copies time to finish.
    #[tab(Advanced)]
    #[serde(default = "default_filesystem_settle_ms")]
    pub settle_ms: u64,
    /// Read file content into the event payload. Opt-in.
    #[tab(Behavior)]
    #[serde(default)]
    pub read_content: bool,
    /// Follow symlinks when reading event-path metadata, hash, and content.
    /// Off by default: a symlink under a watched root is rejected before any
    /// metadata/hash/content handling so its target cannot escape the root. When
    /// on, the canonical target must still resolve inside a configured root.
    #[tab(Advanced)]
    #[serde(default)]
    pub follow_symlinks: bool,
    /// Maximum file content bytes admitted into a payload when `read_content`,
    /// and the size ceiling for hashing. `Some(n)` caps reads and hashing at
    /// `n` bytes; oversize files are skipped rather than truncated. `None`
    /// removes the ceiling entirely (unbounded reads). Defaults to
    /// `Some(65536)` (64 KiB), so the cap applies by default whenever
    /// `read_content` or hashing runs.
    #[tab(Advanced)]
    #[serde(default = "default_filesystem_max_content_bytes")]
    pub max_content_bytes: Option<usize>,
    /// Watch broad system roots (`/`, `/home`, `/etc`, `/var`, `/proc`,
    /// `/sys`, `/dev`, `/tmp`) despite the deny-broad-roots default. The
    /// pseudo-filesystems `/proc` and `/sys` would surface kernel object
    /// paths in SOP payloads and flood the watcher with events. Off unless
    /// explicitly enabled.
    #[tab(Advanced)]
    #[serde(default)]
    pub allow_broad_roots: bool,
    /// Tools excluded from this channel's tool spec.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
}

impl Default for FilesystemConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            paths: Vec::new(),
            recursive: true,
            include: Vec::new(),
            exclude: Vec::new(),
            events: default_filesystem_events(),
            debounce_ms: default_filesystem_debounce_ms(),
            settle_ms: default_filesystem_settle_ms(),
            read_content: false,
            follow_symlinks: false,
            max_content_bytes: default_filesystem_max_content_bytes(),
            allow_broad_roots: false,
            excluded_tools: Vec::new(),
        }
    }
}

const FILESYSTEM_BROAD_ROOTS: [&str; 8] = [
    "/", "/home", "/etc", "/var", "/proc", "/sys", "/dev", "/tmp",
];

impl FilesystemConfig {
    /// Validate the filesystem listener configuration.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.paths.is_empty() {
            anyhow::bail!("at least one path must be configured");
        }
        for path in &self.paths {
            let trimmed = path.trim_end_matches('/');
            let normalized = if trimmed.is_empty() { "/" } else { trimmed };
            if !self.allow_broad_roots && FILESYSTEM_BROAD_ROOTS.contains(&normalized) {
                anyhow::bail!(
                    "path '{path}' is a broad system root; set allow_broad_roots = true to watch it"
                );
            }
        }
        for kind in &self.events {
            if !matches!(
                kind.as_str(),
                "created" | "modified" | "deleted" | "renamed"
            ) {
                anyhow::bail!(
                    "event '{kind}' is invalid; expected created, modified, deleted, or renamed"
                );
            }
        }
        Ok(())
    }
}

impl ChannelConfig for FilesystemConfig {
    fn name() -> &'static str {
        "Filesystem"
    }
    fn desc() -> &'static str {
        "Filesystem SOP Listener"
    }
}

fn default_filesystem_events() -> Vec<String> {
    vec![
        "created".to_string(),
        "modified".to_string(),
        "deleted".to_string(),
        "renamed".to_string(),
    ]
}

fn default_filesystem_debounce_ms() -> u64 {
    500
}

fn default_filesystem_settle_ms() -> u64 {
    250
}

fn default_filesystem_max_content_bytes() -> Option<usize> {
    Some(65536)
}

/// Generic AMQP 0-9-1 channel configuration (RabbitMQ, Fedora Messaging, etc.).
///
/// Subscribes to an exchange via routing keys and lifts each delivery into an
/// inbound `ChannelMessage`. The mapping from a JSON delivery body to message
/// fields is config-driven (`content_template`, `thread_id_field`) so a new
/// source — Anitya, an internal bus, anything publishing JSON — is onboarded by
/// configuration rather than code.
/// RETIRED dispatch routing for AMQP fan-in: this field formerly chose between
/// the agent loop, the SOP engine, or both. Removed with the run side; any
/// value now fails config parse with the migration message and deliveries
/// always drive the agent loop.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.amqp"]
pub struct AmqpConfig {
    /// RETIRED dispatch mode (`sop` / `sop_and_agent_loop` / `agent_loop`).
    /// Kept as a parse-time tombstone so a legacy value is rejected with the
    /// migration message instead of silently reinterpreted as agent-loop
    /// delivery. Removed with the run-side config sweep.
    #[serde(
        default,
        deserialize_with = "reject_retired_sop_key",
        skip_serializing_if = "Option::is_none"
    )]
    #[cfg_attr(feature = "schema-export", schemars(skip))]
    pub dispatch: Option<String>,
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// AMQP broker URL. Use `amqp://` for plain or `amqps://` for TLS
    /// (e.g. `amqps://fedora:@rabbitmq.fedoraproject.org/%2Fpublic_pubsub`).
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub amqp_url: String,
    /// Exchange to bind the consumer queue to (e.g. `amq.topic`).
    #[tab(Advanced)]
    pub exchange: String,
    /// Routing keys to bind. Scope these to the topics of interest; binding
    /// `#` consumes the entire exchange and is almost never what you want.
    #[tab(Advanced)]
    #[serde(default)]
    pub routing_keys: Vec<String>,
    /// Queue name. Leave unset for a server-generated, transient,
    /// auto-deleted, exclusive queue. Set a stable name (UUID recommended)
    /// only when durable delivery across reconnects is required.
    #[tab(Advanced)]
    pub queue: Option<String>,
    /// Path to the CA certificate bundle for `amqps://` connections.
    #[tab(Connection)]
    pub ca_cert: Option<PathBuf>,
    /// Path to the client certificate for broker mutual-TLS auth
    /// (Fedora Messaging requires a client cert).
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub client_cert: Option<PathBuf>,
    /// Path to the client private key matching `client_cert`.
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub client_key: Option<PathBuf>,
    /// Value placed in `ChannelMessage.sender` for every delivery from this
    /// source (e.g. `anitya`). Lets the orchestrator's self-loop guard and
    /// per-channel routing identify the origin.
    #[tab(Behavior)]
    #[serde(default = "default_amqp_sender_label")]
    pub sender_label: String,
    /// Template for the inbound message content. `{field}` placeholders are
    /// interpolated from the JSON delivery body's top-level keys. When empty,
    /// the raw delivery body is used verbatim.
    #[tab(Behavior)]
    #[serde(default)]
    pub content_template: String,
    /// Dotted path into the JSON delivery body whose value becomes the
    /// message `thread_ts`, correlating replies to the originating event
    /// (e.g. `message.project.name`). Empty disables threading.
    #[tab(Behavior)]
    #[serde(default)]
    pub thread_id_field: String,
    /// Acknowledgement mode. When `true` (default), deliveries are acked only
    /// after the message is durably handed to the agent loop, giving
    /// at-least-once semantics: a crash before hand-off redelivers the event.
    /// Set `false` for at-most-once (broker acks on dispatch), which silently
    /// drops in-flight events on crash and is only appropriate for
    /// non-side-effecting, drop-on-overload consumers.
    #[tab(Behavior)]
    #[serde(default = "default_amqp_durable_ack")]
    pub durable_ack: bool,
    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
}

impl AmqpConfig {
    /// Validate the AMQP configuration.
    ///
    /// Checks:
    /// - `amqp_url` uses a valid scheme (`amqp://` or `amqps://`)
    /// - `amqps://` connections carry a CA certificate
    /// - `client_cert` and `client_key` are supplied together (mutual TLS)
    /// - the exchange is non-empty
    /// - at least one routing key is bound
    pub fn validate(&self) -> anyhow::Result<()> {
        let is_tls = self.amqp_url.starts_with("amqps://");
        let is_plain = self.amqp_url.starts_with("amqp://");

        if !is_tls && !is_plain {
            anyhow::bail!(
                "amqp_url must start with 'amqp://' or 'amqps://', got: {}",
                self.amqp_url
            );
        }

        if is_tls && self.ca_cert.is_none() {
            anyhow::bail!("amqps:// requires ca_cert to verify the broker");
        }

        match (self.client_cert.is_some(), self.client_key.is_some()) {
            (true, false) => {
                anyhow::bail!(
                    "client_cert is set but client_key is missing (both are required for mutual TLS)"
                )
            }
            (false, true) => {
                anyhow::bail!(
                    "client_key is set but client_cert is missing (both are required for mutual TLS)"
                )
            }
            _ => {}
        }

        if self.exchange.is_empty() {
            validation_bail!(RequiredFieldEmpty, "exchange", "exchange must not be empty");
        }

        if self.routing_keys.is_empty() {
            anyhow::bail!("at least one routing key must be configured");
        }

        Ok(())
    }
}

impl ChannelConfig for AmqpConfig {
    fn name() -> &'static str {
        "AMQP"
    }
    fn desc() -> &'static str {
        "AMQP topic consumer"
    }
}

fn default_amqp_sender_label() -> String {
    "amqp".to_string()
}

fn default_amqp_durable_ack() -> bool {
    true
}

/// IRC channel configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.irc"]
pub struct IrcConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// IRC server hostname
    #[tab(Advanced)]
    pub server: String,
    /// IRC server port (default: 6697 for TLS)
    #[tab(Advanced)]
    #[serde(default = "default_irc_port")]
    pub port: u16,
    /// Bot nickname
    #[tab(Advanced)]
    pub nickname: String,
    /// Username (defaults to nickname if not set)
    #[tab(Connection)]
    pub username: Option<String>,
    /// Channels to join on connect
    #[tab(Advanced)]
    #[serde(default)]
    pub channels: Vec<String>,
    /// Server password (for bouncers like ZNC)
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub server_password: Option<String>,
    /// NickServ IDENTIFY password
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub nickserv_password: Option<String>,
    /// SASL PLAIN password (IRCv3)
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub sasl_password: Option<String>,
    /// Verify TLS certificate (default: true)
    #[tab(Advanced)]
    pub verify_tls: Option<bool>,
    /// When true, only respond to messages that mention the bot.
    /// Other messages in the channel are silently ignored.
    #[tab(Behavior)]
    #[serde(default)]
    pub mention_only: bool,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
}

impl ChannelConfig for IrcConfig {
    fn name() -> &'static str {
        "IRC"
    }
    fn desc() -> &'static str {
        "IRC over TLS"
    }
}

/// Twitch chat channel configuration. A thin adapter over IRC
/// (`irc.chat.twitch.tv:6697` over TLS); see the `twitch` channel module.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.twitch"]
pub struct TwitchConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false`.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Twitch login name of the bot account (case-insensitive — lowercased
    /// before send).
    #[tab(Connection)]
    pub bot_username: String,
    /// Twitch OAuth user-access token. The `oauth:` prefix is added
    /// automatically if missing, so both `"oauth:abcdef"` and `"abcdef"`
    /// work. Mint via <https://twitchapps.com/tmi/> or the Twitch CLI.
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub oauth_token: String,
    /// Twitch channels to join. Each entry receives a `#` prefix if missing
    /// and is lowercased before send (Twitch channel names are
    /// case-insensitive). E.g. `["mychannel", "#anotherchannel"]`.
    #[tab(Advanced)]
    #[serde(default)]
    pub channels: Vec<String>,
    /// When true, only respond to messages that mention the bot's login
    /// name. Default: `false`.
    #[tab(Behavior)]
    #[serde(default)]
    pub mention_only: bool,
}

impl ChannelConfig for TwitchConfig {
    fn name() -> &'static str {
        "Twitch"
    }
    fn desc() -> &'static str {
        "Twitch chat (IRC)"
    }
}

fn default_irc_port() -> u16 {
    6697
}

/// How ZeroClaw receives events from Feishu / Lark.
///
/// - `websocket` (default) — persistent WSS long-connection; no public URL required.
/// - `webhook` — HTTP callback server; requires a public HTTPS endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum LarkReceiveMode {
    #[default]
    Websocket,
    Webhook,
}

/// Lark/Feishu configuration for messaging integration.
/// Lark is the international version; Feishu is the Chinese version.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.lark"]
pub struct LarkConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// App ID from Lark/Feishu developer console
    #[tab(Connection)]
    pub app_id: String,
    /// App Secret from Lark/Feishu developer console
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub app_secret: String,
    /// Encrypt key for webhook message decryption (optional)
    #[serde(default)]
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub encrypt_key: Option<String>,
    /// Verification token for webhook validation (optional)
    #[serde(default)]
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub verification_token: Option<String>,
    /// When true, only respond to messages that @-mention the bot in groups.
    /// Direct messages are always processed.
    #[tab(Behavior)]
    #[serde(default)]
    pub mention_only: bool,
    /// Whether to use the Feishu (Chinese) endpoint instead of Lark (International)
    #[tab(Advanced)]
    #[serde(default)]
    pub use_feishu: bool,
    /// Event receive mode: "websocket" (default) or "webhook"
    #[tab(Advanced)]
    #[serde(default)]
    pub receive_mode: LarkReceiveMode,
    /// HTTP port for webhook mode only. Must be set when receive_mode = "webhook".
    /// Not required (and ignored) for websocket mode.
    #[tab(Advanced)]
    #[serde(default)]
    pub port: Option<u16>,
    /// Per-channel proxy URL (http, https, socks5, socks5h).
    /// Overrides the global `[proxy]` setting for this channel only.
    #[tab(Advanced)]
    #[serde(default)]
    pub proxy_url: Option<String>,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,

    /// Time in seconds an approval card waits for user response before
    /// the runtime auto-denies. Default: 300 (5 minutes).
    #[tab(Behavior)]
    #[serde(default = "default_channel_approval_timeout_secs")]
    pub approval_timeout_secs: u64,
    /// When `true`, group-chat sessions key on the sender's open_id, so
    /// distinct members of the same group chat don't share conversation
    /// context. When `false` (default), all members of a group share one
    /// session keyed on chat_id (matches the existing behavior). 1-on-1
    /// chats are unaffected (chat_id is already unique per user-bot pair).
    #[tab(Behavior)]
    #[serde(default)]
    pub per_user_session: bool,

    /// Override for the top-level `ack_reactions` setting. When `None`, the
    /// channel falls back to `[channels].ack_reactions`. When set
    /// explicitly, it takes precedence.
    #[tab(Behavior)]
    #[serde(default)]
    pub ack_reactions: Option<bool>,

    /// Streaming mode for the LLM response: `off` (default) routes every
    /// response through `send()`; `partial` opens a Feishu interactive
    /// card and edits it incrementally via `update_draft` /
    /// `finalize_draft`; `multi_message` is rejected for Lark (Feishu has
    /// no equivalent surface — falls back to `off` with a warning).
    #[tab(Behavior)]
    #[serde(default)]
    pub stream_mode: StreamMode,

    /// Minimum interval between consecutive `update_draft` PATCH calls in
    /// milliseconds. Default 1000 ms tunes to Feishu's 5 QPS-per-message
    /// edit cap; raise on enterprise plans with higher quotas.
    #[tab(Behavior)]
    #[serde(default = "default_draft_update_interval_ms")]
    pub draft_update_interval_ms: u64,
}

impl ChannelConfig for LarkConfig {
    fn name() -> &'static str {
        "Lark"
    }
    fn desc() -> &'static str {
        "Lark Bot"
    }
}

/// DM (1:1 chat) access policy for the LINE channel.
#[derive(
    Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum LineDmPolicy {
    /// Respond to every DM regardless of who sent it.
    Open,
    /// Require a one-time `/bind <code>` handshake before responding (default).
    /// ZeroClaw prints the bind code on startup; send it once to unlock access.
    #[default]
    Pairing,
    /// Respond only to LINE user IDs listed in `allowed_users`.
    Allowlist,
}

/// Group / multi-person chat policy for the LINE channel.
#[derive(
    Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum LineGroupPolicy {
    /// Respond to every message in group/room chats.
    Open,
    /// Respond only when the bot is @mentioned (default).
    #[default]
    Mention,
    /// Ignore all messages in group/room chats.
    Disabled,
}

/// LINE Messaging API channel configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.line"]
pub struct LineConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Long-lived channel access token (from LINE Developers Console).
    /// Used for both the Reply API and the Push API fallback.
    /// Falls back to the `LINE_CHANNEL_ACCESS_TOKEN` environment variable if empty.
    #[serde(default)]
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub channel_access_token: String,
    /// Channel secret (from LINE Developers Console).
    /// Used to verify the `X-Line-Signature` header on incoming webhooks.
    /// Falls back to the `LINE_CHANNEL_SECRET` environment variable if empty.
    #[serde(default)]
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub channel_secret: String,
    /// DM (1:1 chat) access policy. Default: `pairing`.
    ///
    /// - `open`: respond to everyone
    /// - `pairing`: require one-time `/bind <code>` handshake on first contact
    /// - `allowlist`: respond only to user IDs listed in `allowed_users`
    #[tab(Advanced)]
    #[serde(default)]
    pub dm_policy: LineDmPolicy,
    /// Group / multi-person chat policy. Default: `mention`.
    ///
    /// - `open`: respond to every message
    /// - `mention`: respond only when @mentioned
    /// - `disabled`: ignore all group messages
    #[tab(Advanced)]
    #[serde(default)]
    pub group_policy: LineGroupPolicy,
    /// TCP port the embedded webhook server listens on. Default: `8443`.
    #[tab(Advanced)]
    #[serde(default = "default_line_webhook_port")]
    pub webhook_port: u16,
    /// Per-channel proxy URL (http, https, socks5, socks5h).
    /// Overrides the global `[proxy]` setting for this channel only.
    #[tab(Advanced)]
    #[serde(default)]
    pub proxy_url: Option<String>,

    /// Display name shown as the sender in LINE chat bubbles.
    /// Defaults to `"AI"` when unset or empty.
    #[tab(Behavior)]
    #[serde(default)]
    pub sender_name: Option<String>,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
}

fn default_line_webhook_port() -> u16 {
    8443
}

impl ChannelConfig for LineConfig {
    fn name() -> &'static str {
        "LINE"
    }
    fn desc() -> &'static str {
        "connect your LINE bot"
    }
}

// ── Security Config ─────────────────────────────────────────────────

/// Security configuration for audit logging, OTP, e-stop, IAM/SSO, and WebAuthn.
///
/// Sandbox backend and resource limits live on per-agent risk profiles
/// (see `RiskProfileConfig::sandbox_*` and `RiskProfileConfig::max_*`); the
/// runtime resolves them via `Config::active_risk_profile(agent_alias)`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "security"]
pub struct SecurityConfig {
    /// Audit logging configuration
    #[serde(default)]
    #[nested]
    pub audit: AuditConfig,

    /// Outbound credential leak detection and redaction configuration. See
    /// `[security.leak_detection]` for the detector controls.
    #[serde(default)]
    #[nested]
    pub leak_detection: LeakDetectionConfig,

    /// OTP gating configuration for sensitive actions/domains.
    #[serde(default)]
    #[nested]
    pub otp: OtpConfig,

    /// Emergency-stop state machine configuration.
    #[serde(default)]
    #[nested]
    pub estop: EstopConfig,

    /// Nevis IAM integration for SSO/MFA authentication and role-based access.
    #[serde(default)]
    #[nested]
    pub nevis: NevisConfig,

    /// WebAuthn / FIDO2 hardware key authentication configuration.
    #[serde(default)]
    #[nested]
    pub webauthn: WebAuthnConfig,
}

/// Outbound credential leak detection configuration.
///
/// These settings control the final guardrail pass over outbound channel
/// responses before they are delivered. Deterministic credential patterns
/// include API keys, private keys, database URLs, bot tokens, and related
/// token syntax. The high-entropy pass is a separate heuristic for standalone
/// opaque tokens.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "security.leak_detection"]
pub struct LeakDetectionConfig {
    /// Enable outbound credential leak detection and redaction.
    #[serde(default = "default_leak_detection_enabled")]
    pub enabled: bool,

    /// Detection sensitivity from 0.0 to 1.0; higher is more aggressive.
    #[serde(default = "default_leak_detection_sensitivity")]
    pub sensitivity: f64,

    /// Enable high-entropy token redaction; deterministic patterns still run when false.
    #[serde(default = "default_leak_detection_high_entropy_tokens")]
    pub high_entropy_tokens: bool,
}

fn default_leak_detection_enabled() -> bool {
    true
}

fn default_leak_detection_sensitivity() -> f64 {
    0.7
}

fn default_leak_detection_high_entropy_tokens() -> bool {
    true
}

impl Default for LeakDetectionConfig {
    fn default() -> Self {
        Self {
            enabled: default_leak_detection_enabled(),
            sensitivity: default_leak_detection_sensitivity(),
            high_entropy_tokens: default_leak_detection_high_entropy_tokens(),
        }
    }
}

/// WebAuthn / FIDO2 hardware key authentication configuration (`[security.webauthn]`).
///
/// Enables registration and authentication via hardware security keys
/// (YubiKey, SoloKey, etc.) and platform authenticators (Touch ID, Windows Hello).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "security.webauthn"]
pub struct WebAuthnConfig {
    /// Enable WebAuthn authentication. Default: false.
    #[serde(default)]
    pub enabled: bool,
    /// Relying Party ID (domain name, e.g. "example.com"). Default: "localhost".
    #[serde(default = "default_webauthn_rp_id")]
    pub rp_id: String,
    /// Relying Party origin URL (e.g. `"https://example.com"`). Default: `"http://localhost:42617"`.
    #[serde(default = "default_webauthn_rp_origin")]
    pub rp_origin: String,
    /// Relying Party display name. Default: "ZeroClaw".
    #[serde(default = "default_webauthn_rp_name")]
    pub rp_name: String,
}

impl Default for WebAuthnConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            rp_id: default_webauthn_rp_id(),
            rp_origin: default_webauthn_rp_origin(),
            rp_name: default_webauthn_rp_name(),
        }
    }
}

fn default_webauthn_rp_id() -> String {
    "localhost".into()
}

fn default_webauthn_rp_origin() -> String {
    "http://localhost:42617".into()
}

fn default_webauthn_rp_name() -> String {
    "ZeroClaw".into()
}

/// OTP validation strategy.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum OtpMethod {
    /// Time-based one-time password (RFC 6238).
    #[default]
    Totp,
    /// Future method for paired-device confirmations.
    Pairing,
    /// Future method for local CLI challenge prompts.
    CliPrompt,
}

/// Security OTP configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "security.otp"]
#[serde(deny_unknown_fields)]
pub struct OtpConfig {
    /// Enable OTP gating. Defaults to disabled for backward compatibility.
    #[serde(default)]
    pub enabled: bool,

    /// OTP method.
    #[serde(default)]
    pub method: OtpMethod,

    /// TOTP time-step in seconds.
    #[serde(default = "default_otp_token_ttl_secs")]
    pub token_ttl_secs: u64,

    /// Reuse window for recently validated OTP codes.
    #[serde(default = "default_otp_cache_valid_secs")]
    pub cache_valid_secs: u64,

    /// Deprecated and unsupported: ZeroClaw has no OTP action-gating, so
    /// this list is parsed for backward compatibility but never enforced
    /// and provides no authorization boundary. Malformed entries still
    /// fail validation for compatibility; a non-default value emits an
    /// `otp_action_gating_unsupported` config-health warning. High-risk
    /// action authorization is owned by the Tachi approval/grant plane
    /// with Node-local enforcement.
    #[serde(default = "default_otp_gated_actions")]
    pub gated_actions: Vec<String>,

    /// Deprecated and unsupported: no runtime path matches domains against
    /// OTP policy, so this list is parsed for backward compatibility but
    /// never enforced. A non-empty value emits an
    /// `otp_action_gating_unsupported` config-health warning (see
    /// `gated_actions` for the authorization owner).
    #[serde(default)]
    pub gated_domains: Vec<String>,

    /// Deprecated and unsupported: like `gated_domains`, parsed for
    /// backward compatibility but never enforced. A non-empty value emits
    /// an `otp_action_gating_unsupported` config-health warning.
    #[serde(default)]
    pub gated_domain_categories: Vec<String>,

    /// Deprecated and unsupported: no OTP challenge-attempt limiter
    /// consumes this value, so it is parsed for backward compatibility but
    /// never enforced. `0` still fails validation for compatibility; any
    /// other non-default value emits an `otp_action_gating_unsupported`
    /// config-health warning.
    #[serde(default = "default_otp_challenge_max_attempts")]
    pub challenge_max_attempts: u32,
}

fn default_otp_token_ttl_secs() -> u64 {
    30
}

fn default_otp_cache_valid_secs() -> u64 {
    300
}

fn default_otp_challenge_max_attempts() -> u32 {
    3
}

fn default_otp_gated_actions() -> Vec<String> {
    vec![
        "shell".to_string(),
        "file_write".to_string(),
        "browser_open".to_string(),
        "browser".to_string(),
        "memory_forget".to_string(),
    ]
}

impl Default for OtpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            method: OtpMethod::Totp,
            token_ttl_secs: default_otp_token_ttl_secs(),
            cache_valid_secs: default_otp_cache_valid_secs(),
            gated_actions: default_otp_gated_actions(),
            gated_domains: Vec::new(),
            gated_domain_categories: Vec::new(),
            challenge_max_attempts: default_otp_challenge_max_attempts(),
        }
    }
}

/// Emergency stop configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "security.estop"]
#[serde(deny_unknown_fields)]
pub struct EstopConfig {
    /// Enable emergency stop controls.
    #[serde(default)]
    pub enabled: bool,

    /// File path used to persist estop state.
    #[serde(default = "default_estop_state_file")]
    pub state_file: String,

    /// Require a valid OTP before resume operations.
    #[serde(default = "default_true")]
    pub require_otp_to_resume: bool,
}

fn default_estop_state_file() -> String {
    default_path_under_config_dir("estop-state.json")
}

impl Default for EstopConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            state_file: default_estop_state_file(),
            require_otp_to_resume: true,
        }
    }
}

/// Nevis IAM integration configuration.
///
/// When `enabled` is true, ZeroClaw validates incoming requests against a Nevis
/// Security Suite instance and maps Nevis roles to tool/workspace permissions.
#[derive(Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "security.nevis"]
#[serde(deny_unknown_fields)]
pub struct NevisConfig {
    /// Enable Nevis IAM integration. Defaults to false for backward compatibility.
    #[serde(default)]
    pub enabled: bool,

    /// Base URL of the Nevis instance (e.g. `https://nevis.example.com`).
    #[serde(default)]
    pub instance_url: String,

    /// Nevis realm to authenticate against.
    #[serde(default = "default_nevis_realm")]
    pub realm: String,

    /// OAuth2 client ID registered in Nevis.
    #[serde(default)]
    pub client_id: String,

    /// OAuth2 client secret. Encrypted via SecretStore when stored on disk.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub client_secret: Option<String>,

    /// Token validation strategy: `"local"` (JWKS) or `"remote"` (introspection).
    #[serde(default = "default_nevis_token_validation")]
    pub token_validation: String,

    /// JWKS endpoint URL for local token validation.
    #[serde(default)]
    pub jwks_url: Option<String>,

    /// Nevis role to ZeroClaw permission mappings.
    #[serde(default)]
    pub role_mapping: Vec<NevisRoleMappingConfig>,

    /// Require MFA verification for all Nevis-authenticated requests.
    #[serde(default)]
    pub require_mfa: bool,

    /// Session timeout in seconds.
    #[serde(default = "default_nevis_session_timeout_secs")]
    pub session_timeout_secs: u64,
}

impl std::fmt::Debug for NevisConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NevisConfig")
            .field("enabled", &self.enabled)
            .field("instance_url", &self.instance_url)
            .field("realm", &self.realm)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .field("token_validation", &self.token_validation)
            .field("jwks_url", &self.jwks_url)
            .field("role_mapping", &self.role_mapping)
            .field("require_mfa", &self.require_mfa)
            .field("session_timeout_secs", &self.session_timeout_secs)
            .finish()
    }
}

impl NevisConfig {
    /// Validate that required fields are present when Nevis is enabled.
    ///
    /// Call at config load time to fail fast on invalid configuration rather
    /// than deferring errors to the first authentication request.
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }

        if self.instance_url.trim().is_empty() {
            return Err("nevis.instance_url is required when Nevis IAM is enabled".into());
        }

        if self.client_id.trim().is_empty() {
            return Err("nevis.client_id is required when Nevis IAM is enabled".into());
        }

        if self.realm.trim().is_empty() {
            return Err("nevis.realm is required when Nevis IAM is enabled".into());
        }

        match self.token_validation.as_str() {
            "local" | "remote" => {}
            other => {
                return Err(format!(
                    "nevis.token_validation has invalid value '{other}': \
                     expected 'local' or 'remote'"
                ));
            }
        }

        if self.token_validation == "local" && self.jwks_url.is_none() {
            return Err("nevis.jwks_url is required when token_validation is 'local'".into());
        }

        if self.session_timeout_secs == 0 {
            return Err("nevis.session_timeout_secs must be greater than 0".into());
        }

        Ok(())
    }
}

fn default_nevis_realm() -> String {
    "master".into()
}

fn default_nevis_token_validation() -> String {
    "local".into()
}

fn default_nevis_session_timeout_secs() -> u64 {
    3600
}

impl Default for NevisConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            instance_url: String::new(),
            realm: default_nevis_realm(),
            client_id: String::new(),
            client_secret: None,
            token_validation: default_nevis_token_validation(),
            jwks_url: None,
            role_mapping: Vec::new(),
            require_mfa: false,
            session_timeout_secs: default_nevis_session_timeout_secs(),
        }
    }
}

/// Maps a Nevis role to ZeroClaw tool permissions and workspace access.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct NevisRoleMappingConfig {
    /// Nevis role name (case-insensitive).
    pub nevis_role: String,

    /// Tool names this role can access. Use `"all"` for unrestricted tool access.
    #[serde(default)]
    pub zeroclaw_permissions: Vec<String>,

    /// Workspace names this role can access. Use `"all"` for unrestricted.
    #[serde(default)]
    pub workspace_access: Vec<String>,
}

/// Sandbox configuration for OS-level isolation
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "security.sandbox"]
pub struct SandboxConfig {
    /// Enable sandboxing (None = auto-detect, Some = explicit)
    #[serde(default)]
    pub enabled: Option<bool>,

    /// Sandbox backend to use
    #[serde(default)]
    pub backend: SandboxBackend,

    /// Custom Firejail arguments (when backend = firejail)
    #[serde(default)]
    pub firejail_args: Vec<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: None, // Auto-detect
            backend: SandboxBackend::Auto,
            firejail_args: Vec::new(),
        }
    }
}

/// Sandbox backend selection
#[derive(Debug, Clone, Serialize, Deserialize, Default, zeroclaw_macros::ConfigEnum)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum SandboxBackend {
    /// Auto-detect best available (default)
    #[default]
    Auto,
    /// Landlock (Linux kernel LSM, native)
    Landlock,
    /// Firejail (user-space sandbox)
    Firejail,
    /// Bubblewrap (user namespaces)
    Bubblewrap,
    /// Docker container isolation
    Docker,
    /// macOS sandbox-exec (Seatbelt)
    #[serde(alias = "sandbox-exec")]
    SandboxExec,
    /// No sandboxing (application-layer only)
    None,
}

/// Audit logging configuration
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "security.audit"]
pub struct AuditConfig {
    /// Enable audit logging
    #[serde(default = "default_audit_enabled")]
    pub enabled: bool,

    /// Path to audit log file (relative to zeroclaw dir)
    #[serde(default = "default_audit_log_path")]
    pub log_path: String,

    /// Maximum log size in MB before rotation
    #[serde(default = "default_audit_max_size_mb")]
    pub max_size_mb: u32,

    /// Sign events with HMAC for tamper evidence
    #[serde(default)]
    pub sign_events: bool,
}

fn default_audit_enabled() -> bool {
    true
}

fn default_audit_log_path() -> String {
    "audit.log".to_string()
}

fn default_audit_max_size_mb() -> u32 {
    100
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: default_audit_enabled(),
            log_path: default_audit_log_path(),
            max_size_mb: default_audit_max_size_mb(),
            sign_events: false,
        }
    }
}

/// DingTalk configuration for Stream Mode messaging
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.dingtalk"]
pub struct DingTalkConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Client ID (AppKey) from DingTalk developer console
    #[tab(Connection)]
    pub client_id: String,
    /// Client Secret (AppSecret) from DingTalk developer console
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub client_secret: String,
    /// Per-channel proxy URL (http, https, socks5, socks5h).
    /// Overrides the global `[proxy]` setting for this channel only.
    #[tab(Advanced)]
    #[serde(default)]
    pub proxy_url: Option<String>,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
}

impl ChannelConfig for DingTalkConfig {
    fn name() -> &'static str {
        "DingTalk"
    }
    fn desc() -> &'static str {
        "DingTalk Stream Mode"
    }
}

/// WeCom (WeChat Enterprise) Bot Webhook configuration
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.wecom"]
pub struct WeComConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Webhook key from WeCom Bot configuration
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub webhook_key: String,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
}

impl ChannelConfig for WeComConfig {
    fn name() -> &'static str {
        "WeCom"
    }
    fn desc() -> &'static str {
        "WeCom Bot Webhook"
    }
}

fn default_wecom_ws_file_retention_days() -> u32 {
    7
}

fn default_wecom_ws_max_file_size_mb() -> u64 {
    20
}

fn default_wecom_ws_stream_mode() -> StreamMode {
    StreamMode::Partial
}

/// WeCom AI Bot WebSocket configuration.
///
/// This is distinct from webhook-based [`WeComConfig`] and uses the WeCom AI
/// Bot long-connection API for inbound messages and active-session replies.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.wecom_ws"]
pub struct WeComWsConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[serde(default)]
    pub enabled: bool,
    /// Bot ID for WeCom WebSocket subscription.
    pub bot_id: String,
    /// Secret for WeCom WebSocket subscription authentication.
    #[secret]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub secret: String,
    /// Allowed WeCom user IDs. Empty = deny all, "*" = allow all users.
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// Allowed WeCom group chat IDs. Empty = deny all groups, "*" = allow all groups.
    #[serde(default)]
    pub allowed_groups: Vec<String>,
    /// Display name or mention alias of the WeCom AI bot, for example `danya`.
    ///
    /// WeCom group text often arrives as plain text such as `@danya say hi`;
    /// passing this name through lets the generic reply-intent precheck
    /// recognize that a group message was addressed to the bot.
    #[serde(default)]
    pub bot_name: Option<String>,
    /// File retention days for downloaded WeCom attachments under the workspace cache.
    #[serde(default = "default_wecom_ws_file_retention_days")]
    pub file_retention_days: u32,
    /// Maximum accepted file size in MiB for WeCom attachment download attempts.
    #[serde(default = "default_wecom_ws_max_file_size_mb")]
    pub max_file_size_mb: u64,
    /// Streaming mode for progressive draft delivery over the WeCom long connection.
    #[serde(default = "default_wecom_ws_stream_mode")]
    pub stream_mode: StreamMode,
    /// Optional per-channel proxy override. Falls back to the global proxy config when empty.
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[serde(default)]
    pub excluded_tools: Vec<String>,
}

impl Default for WeComWsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bot_id: String::new(),
            secret: String::new(),
            allowed_users: Vec::new(),
            allowed_groups: Vec::new(),
            bot_name: None,
            file_retention_days: default_wecom_ws_file_retention_days(),
            max_file_size_mb: default_wecom_ws_max_file_size_mb(),
            stream_mode: default_wecom_ws_stream_mode(),
            proxy_url: None,
            excluded_tools: Vec::new(),
        }
    }
}

impl ChannelConfig for WeComWsConfig {
    fn name() -> &'static str {
        "WeCom WebSocket"
    }
    fn desc() -> &'static str {
        "WeCom AI Bot long connection"
    }
}

/// WeChat personal iLink Bot channel configuration.
///
/// Uses the iLink Bot API (`ilinkai.weixin.qq.com`) with QR-code login.
/// The bot token is obtained by scanning a QR code and persisted to disk
/// so subsequent restarts do not require re-scanning.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.wechat"]
pub struct WeChatConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Override the iLink API base URL. Default: `https://ilinkai.weixin.qq.com`.
    #[tab(Advanced)]
    #[serde(default)]
    pub api_base_url: Option<String>,
    /// Override the CDN base URL. Default: `https://novac2c.cdn.weixin.qq.com/c2c`.
    #[tab(Advanced)]
    #[serde(default)]
    pub cdn_base_url: Option<String>,
    /// Directory to persist bot token and sync cursor.
    /// Default: `~/.zeroclaw/wechat/`.
    #[tab(Advanced)]
    #[serde(default)]
    pub state_dir: Option<String>,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
}

impl ChannelConfig for WeChatConfig {
    fn name() -> &'static str {
        "WeChat"
    }
    fn desc() -> &'static str {
        "WeChat iLink Bot"
    }
}

/// QQ Official Bot configuration (Tencent QQ Bot SDK)
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.qq"]
pub struct QQConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// App ID from QQ Bot developer console
    #[tab(Connection)]
    pub app_id: String,
    /// App Secret from QQ Bot developer console
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub app_secret: String,
    /// Per-channel proxy URL (http, https, socks5, socks5h).
    /// Overrides the global `[proxy]` setting for this channel only.
    #[tab(Advanced)]
    #[serde(default)]
    pub proxy_url: Option<String>,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
}

impl ChannelConfig for QQConfig {
    fn name() -> &'static str {
        "QQ Official"
    }
    fn desc() -> &'static str {
        "Tencent QQ Bot"
    }
}

/// X/Twitter channel configuration (Twitter API v2)
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.twitter"]
pub struct TwitterConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Twitter API v2 Bearer Token (OAuth 2.0)
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub bearer_token: String,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
}

impl ChannelConfig for TwitterConfig {
    fn name() -> &'static str {
        "X/Twitter"
    }
    fn desc() -> &'static str {
        "X/Twitter Bot via API v2"
    }
}

/// Mochat channel configuration (Mochat customer service API)
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.mochat"]
pub struct MochatConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Mochat API base URL
    #[tab(Advanced)]
    pub api_url: String,
    /// Mochat API token
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_token: String,
    /// Poll interval in seconds for new messages. Default: 5
    #[tab(Advanced)]
    #[serde(default = "default_mochat_poll_interval")]
    pub poll_interval_secs: u64,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
}

fn default_mochat_poll_interval() -> u64 {
    5
}

impl ChannelConfig for MochatConfig {
    fn name() -> &'static str {
        "Mochat"
    }
    fn desc() -> &'static str {
        "Mochat Customer Service"
    }
}

/// Reddit channel configuration (OAuth2 bot).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.reddit"]
pub struct RedditConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Reddit OAuth2 client ID.
    #[tab(Connection)]
    pub client_id: String,
    /// Reddit OAuth2 client secret.
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub client_secret: String,
    /// Reddit OAuth2 refresh token for persistent access.
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub refresh_token: String,
    /// Reddit bot username (without `u/` prefix).
    #[tab(Advanced)]
    pub username: String,
    /// Subreddits to filter messages (without `r/` prefix). Empty = accept
    /// from any subreddit the bot has access to. Migrated from the legacy
    /// `subreddit` singular field.
    #[tab(Advanced)]
    #[serde(default)]
    pub subreddits: Vec<String>,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
}

impl ChannelConfig for RedditConfig {
    fn name() -> &'static str {
        "Reddit"
    }
    fn desc() -> &'static str {
        "Reddit bot (OAuth2)"
    }
}

/// Bluesky channel configuration (AT Protocol).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.bluesky"]
pub struct BlueskyConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Bluesky handle (e.g. `"mybot.bsky.social"`).
    #[tab(Connection)]
    pub handle: String,
    /// App-specific password (from Bluesky settings).
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub app_password: String,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
}

impl ChannelConfig for BlueskyConfig {
    fn name() -> &'static str {
        "Bluesky"
    }
    fn desc() -> &'static str {
        "AT Protocol"
    }
}

/// Git-forge channel configuration (polling-based; no webhook required).
///
/// A `provider` selects the forge. GitHub authenticates as a GitHub App:
/// a short-lived RS256 JWT signed with the app's private key is exchanged
/// for per-installation access tokens. Gitea and Forgejo use a personal
/// access token against the instance's `/api/v1` endpoint, named by the
/// required `api_base_url`. Inbound issue/PR comments are polled from the
/// forge REST API, so the daemon needs no inbound network exposure.
#[derive(Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.git"]
pub struct GitConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Git forge provider. Supported: `"github"`, `"gitea"`, and
    /// `"forgejo"` (Forgejo uses the Gitea-compatible REST provider).
    /// Default: `"github"`.
    #[tab(Connection)]
    #[serde(default = "default_git_provider")]
    pub provider: String,
    /// GitHub App ID (shown on the app's settings page). GitHub provider only.
    #[tab(Connection)]
    #[serde(default)]
    pub app_id: u64,
    /// RS256 private key PEM, the contents of the `.pem` file GitHub
    /// generates on the app's settings page, inline and encrypted at rest.
    /// Include the BEGIN/END lines. GitHub provider only.
    #[secret]
    #[multiline]
    #[credential_class = "encrypted_secret"]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key: Option<String>,
    /// Filesystem path to the RS256 private key `.pem` file, read at startup
    /// when `private_key` (inline PEM) is unset. Backward-compatible fallback
    /// for configs that predate the inline-PEM field. GitHub provider only.
    #[tab(Connection)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key_path: Option<String>,
    /// Installation ID to act as. When unset, the app's installations are
    /// listed on first use and a sole installation is auto-selected;
    /// startup fails if the app has zero or multiple installations.
    #[tab(Connection)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installation_id: Option<u64>,
    /// Gitea/Forgejo API base URL, including `/api/v1`, for example
    /// `https://git.example.org/api/v1` (for the public Gitea service:
    /// `https://gitea.com/api/v1`). Required when `provider` is `"gitea"`
    /// or `"forgejo"` - there is no default host, because every API
    /// request carries `access_token`; the channel fails closed at
    /// startup rather than send the token to an endpoint the operator
    /// never named. Gitea/Forgejo provider only.
    #[tab(Connection)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base_url: Option<String>,
    /// Personal access token for Gitea/Forgejo API requests. The token needs
    /// repository read access plus issue/PR comment write access for replies
    /// and reactions. Gitea/Forgejo provider only.
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    #[serde(default)]
    pub access_token: String,
    /// Repositories to poll, as `owner/repo`. Empty = every repository
    /// visible to the installation.
    #[tab(Advanced)]
    #[serde(default)]
    pub repos: Vec<String>,
    /// Poll interval in seconds for new issues and comments. Values below
    /// 15 are clamped to 15. Default: `30`.
    #[tab(Advanced)]
    #[serde(default = "default_github_poll_interval_secs")]
    pub poll_interval_secs: u64,
    /// Only respond to comments that @-mention the app's bot login.
    /// Default: `true`.
    #[tab(Behavior)]
    #[serde(default = "default_true")]
    pub mention_only: bool,
    /// Process comments authored by other bot accounts. The app's own
    /// comments are always ignored. Default: `false`.
    #[tab(Behavior)]
    #[serde(default)]
    pub listen_to_bots: bool,
    /// Per-channel proxy override for GitHub API requests.
    #[tab(Advanced)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    /// Per-event routing table, keyed by normalized event type
    /// (`"issue_comment.created"`, `"pull_request.opened"`,
    /// `"workflow_run.failed"`, …). Event types absent from the table fall
    /// back to the conversational default: `issue_comment.created`,
    /// `issues.opened`, and `pull_request.opened` are delivered as
    /// messages (mention-gated); everything else is ignored. Which API
    /// endpoints are polled is derived from this table; routing an event
    /// type is also subscribing to it.
    #[tab(Behavior)]
    #[serde(default)]
    #[nested]
    pub events: HashMap<String, GitEventRoute>,
    /// Also poll the repository Events API (`/repos/{owner}/{repo}/events`)
    /// as a broad backbone transport: one conditional (ETag) request per
    /// repository per tick, so an idle repo costs almost nothing. Caveats:
    /// events arrive with a delay of up to ~5 minutes and the feed carries
    /// no Actions/check events (workflow runs always use their dedicated
    /// endpoint). Items also surfaced by a targeted endpoint are
    /// de-duplicated. Default: `false`.
    #[tab(Advanced)]
    #[serde(default)]
    pub events_backbone: bool,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
}

impl std::fmt::Debug for GitConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitConfig")
            .field("enabled", &self.enabled)
            .field("provider", &self.provider)
            .field("app_id", &self.app_id)
            .field("private_key", &self.private_key.as_ref().map(|_| "***"))
            .field("private_key_path", &self.private_key_path)
            .field("installation_id", &self.installation_id)
            .field("api_base_url", &self.api_base_url)
            .field(
                "access_token",
                &if self.access_token.is_empty() {
                    ""
                } else {
                    "***"
                },
            )
            .field("repos", &self.repos)
            .field("poll_interval_secs", &self.poll_interval_secs)
            .field("mention_only", &self.mention_only)
            .field("listen_to_bots", &self.listen_to_bots)
            .field("proxy_url", &self.proxy_url)
            .field("events", &self.events)
            .field("events_backbone", &self.events_backbone)
            .field("excluded_tools", &self.excluded_tools)
            .finish()
    }
}

fn default_github_poll_interval_secs() -> u64 {
    30
}

fn default_git_provider() -> String {
    "github".to_string()
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: default_git_provider(),
            app_id: 0,
            private_key: None,
            private_key_path: None,
            installation_id: None,
            api_base_url: None,
            access_token: String::new(),
            repos: Vec::new(),
            poll_interval_secs: default_github_poll_interval_secs(),
            mention_only: true,
            listen_to_bots: false,
            proxy_url: None,
            events: HashMap::new(),
            events_backbone: false,
            excluded_tools: Vec::new(),
        }
    }
}

impl ChannelConfig for GitConfig {
    fn name() -> &'static str {
        "Git"
    }
    fn desc() -> &'static str {
        "Git forge (GitHub, Gitea, Forgejo): issues, PRs & events"
    }
}

/// Routing for one normalized git-forge event type, keyed in
/// `[channels.git.<alias>.events]` by the event type string
/// (`"pull_request.opened" = { sop = "pr-triage" }`).
///
/// An entry with neither `message = true` nor a `sop` explicitly ignores the
/// event type (overriding the conversational default).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.git.events"]
pub struct GitEventRoute {
    /// Deliver the event into the agent loop as a normal channel message.
    #[serde(default)]
    pub message: bool,
    /// RETIRED SOP route target. Parse-time tombstone: a legacy `sop = "..."`
    /// value is rejected with the migration message instead of silently
    /// degrading the route to `Ignore`. Removed with the run-side config
    /// sweep.
    #[serde(
        default,
        deserialize_with = "reject_retired_sop_key",
        skip_serializing_if = "Option::is_none"
    )]
    #[cfg_attr(feature = "schema-export", schemars(skip))]
    pub sop: Option<String>,
}

/// Voice duplex configuration (`[channels.voice_duplex]`).
///
/// Enables full-duplex voice event handling over WebSocket.
/// When disabled (default), voice events are rejected as unknown types.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct VoiceDuplexConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[serde(default)]
    pub enabled: bool,
    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[serde(default)]
    pub excluded_tools: Vec<String>,
}

/// Voice wake word detection channel configuration.
///
/// Listens on the default microphone for a configurable wake word,
/// then captures the following utterance and transcribes it via the
/// existing transcription API.
#[derive(Debug, Clone, Serialize, Deserialize, zeroclaw_macros::Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "voice_wake"]
pub struct VoiceWakeConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[serde(default)]
    pub enabled: bool,
    /// Wake word phrase to listen for (case-insensitive substring match).
    /// Default: `"hey zeroclaw"`.
    #[serde(default = "default_voice_wake_word")]
    pub wake_word: String,
    /// Silence timeout in milliseconds: how long to wait after the last
    /// energy spike before finalizing a capture window. Default: `2000`.
    #[serde(default = "default_voice_wake_silence_timeout_ms")]
    pub silence_timeout_ms: u32,
    /// RMS energy threshold for voice activity detection. Samples below
    /// this level are treated as silence. Default: `0.01`.
    #[serde(default = "default_voice_wake_energy_threshold")]
    pub energy_threshold: f32,
    /// Maximum capture duration in seconds before forcing transcription.
    /// Default: `30`.
    #[serde(default = "default_voice_wake_max_capture_secs")]
    pub max_capture_secs: u32,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[serde(default)]
    pub excluded_tools: Vec<String>,
}

fn default_voice_wake_word() -> String {
    "hey zeroclaw".into()
}

fn default_voice_wake_silence_timeout_ms() -> u32 {
    2000
}

fn default_voice_wake_energy_threshold() -> f32 {
    0.01
}

fn default_voice_wake_max_capture_secs() -> u32 {
    30
}

impl Default for VoiceWakeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            wake_word: default_voice_wake_word(),
            silence_timeout_ms: default_voice_wake_silence_timeout_ms(),
            energy_threshold: default_voice_wake_energy_threshold(),
            max_capture_secs: default_voice_wake_max_capture_secs(),
            excluded_tools: Vec::new(),
        }
    }
}

impl ChannelConfig for VoiceWakeConfig {
    fn name() -> &'static str {
        "VoiceWake"
    }
    fn desc() -> &'static str {
        "voice wake word detection"
    }
}

/// Nostr channel configuration (NIP-04 + NIP-17 private messages)
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "channels.nostr"]
pub struct NostrConfig {
    /// Whether this channel is active. The runtime only loads channels whose
    /// `enabled = true`. Default: `false` so an operator who pastes a partial
    /// `[channels.<type>.<alias>]` block doesn't accidentally bring a channel
    /// live before the rest of its config is filled in.
    #[tab(Behavior)]
    #[serde(default)]
    pub enabled: bool,
    /// Private key in hex or nsec bech32 format
    #[secret]
    #[tab(Connection)]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub private_key: String,
    /// Relay URLs (wss://). Defaults to popular public relays if omitted.
    #[tab(Advanced)]
    #[serde(default = "default_nostr_relays")]
    pub relays: Vec<String>,

    /// Tools excluded from this channel's tool spec. When set, these tools
    /// are not exposed to the model when responding via this channel.
    #[tab(Behavior)]
    #[serde(default)]
    pub excluded_tools: Vec<String>,
}

impl ChannelConfig for NostrConfig {
    fn name() -> &'static str {
        "Nostr"
    }
    fn desc() -> &'static str {
        "Nostr DMs"
    }
}

pub fn default_nostr_relays() -> Vec<String> {
    vec![
        "wss://relay.damus.io".to_string(),
        "wss://nos.lol".to_string(),
        "wss://relay.primal.net".to_string(),
        "wss://relay.snort.social".to_string(),
    ]
}

// -- Notion --

/// Notion integration configuration (`[notion]`).
///
/// When `enabled = true`, the agent polls a Notion database for pending tasks
/// and exposes a `notion` tool for querying, reading, creating, and updating pages.
/// Requires `api_key` (or the `NOTION_API_KEY` env var) and `database_id`.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "notion"]
pub struct NotionConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_key: String,
    #[serde(default)]
    pub database_id: String,
    #[serde(default = "default_notion_poll_interval")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_notion_status_prop")]
    pub status_property: String,
    #[serde(default = "default_notion_input_prop")]
    pub input_property: String,
    #[serde(default = "default_notion_result_prop")]
    pub result_property: String,
    #[serde(default = "default_notion_max_concurrent")]
    pub max_concurrent: usize,
    #[serde(default = "default_notion_recover_stale")]
    pub recover_stale: bool,
}

fn default_notion_poll_interval() -> u64 {
    5
}
fn default_notion_status_prop() -> String {
    "Status".into()
}
fn default_notion_input_prop() -> String {
    "Input".into()
}
fn default_notion_result_prop() -> String {
    "Result".into()
}
fn default_notion_max_concurrent() -> usize {
    4
}
fn default_notion_recover_stale() -> bool {
    true
}

impl Default for NotionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: String::new(),
            database_id: String::new(),
            poll_interval_secs: default_notion_poll_interval(),
            status_property: default_notion_status_prop(),
            input_property: default_notion_input_prop(),
            result_property: default_notion_result_prop(),
            max_concurrent: default_notion_max_concurrent(),
            recover_stale: default_notion_recover_stale(),
        }
    }
}

/// Jira integration configuration (`[jira]`).
///
/// When `enabled = true`, registers the `jira` tool which can get tickets,
/// search with JQL, and add comments. Requires `base_url` and `api_token`
/// (or the `JIRA_API_TOKEN` env var).
///
/// ## Defaults
/// - `enabled`: `false`
/// - `allowed_actions`: `["get_ticket"]` — read-only by default.
///   Add `"search_tickets"` or `"comment_ticket"` to unlock them.
/// - `timeout_secs`: `30`
///
/// ## Auth
/// Jira Cloud uses HTTP Basic auth: `email` + `api_token`.
/// Jira Server/Data Center uses Bearer token auth: omit `email` and set
/// `api_token` to a personal access token.
/// `api_token` is stored encrypted at rest; set it here or via `JIRA_API_TOKEN`.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "jira"]
pub struct JiraConfig {
    /// Enable the `jira` tool. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Atlassian instance base URL, e.g. `https://yourco.atlassian.net`.
    #[serde(default)]
    pub base_url: String,
    /// Jira account email used for Basic auth (Cloud).
    /// Omit for Server/DC deployments using Bearer token auth.
    /// An empty string (`email = ""`) deserializes as `None`. Configs
    /// that round-tripped the empty default to disk would otherwise
    /// silently regress to Basic auth with empty username, since the
    /// email-required validation was dropped when Server/DC Bearer-token
    /// support landed.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_email_skip_empty"
    )]
    pub email: Option<String>,
    /// Jira API token. Encrypted at rest. Falls back to `JIRA_API_TOKEN` env var.
    #[serde(default)]
    #[secret]
    #[credential_class = "encrypted_secret"]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub api_token: String,
    /// Actions the agent is permitted to call.
    /// Valid values: `"get_ticket"`, `"search_tickets"`, `"comment_ticket"`,
    /// `"list_projects"`, `"myself"`, `"list_transitions"`,
    /// `"transition_ticket"`, `"create_ticket"`.
    /// Defaults to `["get_ticket"]` (read-only).
    #[serde(default = "default_jira_allowed_actions")]
    pub allowed_actions: Vec<String>,
    /// Request timeout in seconds. Default: `30`.
    #[serde(default = "default_jira_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_jira_allowed_actions() -> Vec<String> {
    vec!["get_ticket".to_string()]
}

fn default_jira_timeout_secs() -> u64 {
    30
}

impl Default for JiraConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: String::new(),
            email: None,
            api_token: String::new(),
            allowed_actions: default_jira_allowed_actions(),
            timeout_secs: default_jira_timeout_secs(),
        }
    }
}

///
/// Controls the read-only cloud transformation analysis tools:
/// IaC review, migration assessment, cost analysis, and architecture review.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "cloud_ops"]
pub struct CloudOpsConfig {
    /// Enable cloud operations tools. Default: false.
    #[serde(default)]
    pub enabled: bool,
    /// Default cloud model_provider for analysis context. Default: "aws".
    #[serde(default = "default_cloud_ops_cloud")]
    pub default_cloud: String,
    /// Supported cloud model_providers. Default: [`aws`, `azure`, `gcp`].
    #[serde(default = "default_cloud_ops_supported_clouds")]
    pub supported_clouds: Vec<String>,
    /// Supported IaC tools for review. Default: \[`terraform`\].
    #[serde(default = "default_cloud_ops_iac_tools")]
    pub iac_tools: Vec<String>,
    /// Monthly USD threshold to flag cost items. Default: 100.0.
    #[serde(default = "default_cloud_ops_cost_threshold")]
    pub cost_threshold_monthly_usd: f64,
    /// Well-Architected Frameworks to check against. Default: \[`aws-waf`\].
    #[serde(default = "default_cloud_ops_waf")]
    pub well_architected_frameworks: Vec<String>,
}

impl Default for CloudOpsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_cloud: default_cloud_ops_cloud(),
            supported_clouds: default_cloud_ops_supported_clouds(),
            iac_tools: default_cloud_ops_iac_tools(),
            cost_threshold_monthly_usd: default_cloud_ops_cost_threshold(),
            well_architected_frameworks: default_cloud_ops_waf(),
        }
    }
}

impl CloudOpsConfig {
    pub fn validate(&self) -> Result<()> {
        if self.enabled {
            if self.default_cloud.trim().is_empty() {
                anyhow::bail!(
                    "cloud_ops.default_cloud must not be empty when cloud_ops is enabled"
                );
            }
            if self.supported_clouds.is_empty() {
                anyhow::bail!(
                    "cloud_ops.supported_clouds must not be empty when cloud_ops is enabled"
                );
            }
            for (i, cloud) in self.supported_clouds.iter().enumerate() {
                if cloud.trim().is_empty() {
                    validation_bail!(
                        RequiredFieldEmpty,
                        format!("cloud_ops.supported_clouds[{i}]"),
                        "cloud_ops.supported_clouds[{i}] must not be empty"
                    );
                }
            }
            if !self.supported_clouds.contains(&self.default_cloud) {
                anyhow::bail!(
                    "cloud_ops.default_cloud '{}' is not in cloud_ops.supported_clouds {:?}",
                    self.default_cloud,
                    self.supported_clouds
                );
            }
            if self.cost_threshold_monthly_usd < 0.0 {
                anyhow::bail!(
                    "cloud_ops.cost_threshold_monthly_usd must be non-negative, got {}",
                    self.cost_threshold_monthly_usd
                );
            }
            if self.iac_tools.is_empty() {
                anyhow::bail!("cloud_ops.iac_tools must not be empty when cloud_ops is enabled");
            }
        }
        Ok(())
    }
}

fn default_cloud_ops_cloud() -> String {
    "aws".into()
}

fn default_cloud_ops_supported_clouds() -> Vec<String> {
    vec!["aws".into(), "azure".into(), "gcp".into()]
}

fn default_cloud_ops_iac_tools() -> Vec<String> {
    vec!["terraform".into()]
}

fn default_cloud_ops_cost_threshold() -> f64 {
    100.0
}

fn default_cloud_ops_waf() -> Vec<String> {
    vec!["aws-waf".into()]
}

// ── Conversational AI ──────────────────────────────────────────────

fn default_conversational_ai_language() -> String {
    "en".into()
}

fn default_conversational_ai_supported_languages() -> Vec<String> {
    vec!["en".into(), "de".into(), "fr".into(), "it".into()]
}

fn default_conversational_ai_escalation_threshold() -> f64 {
    0.3
}

fn default_conversational_ai_max_turns() -> usize {
    50
}

fn default_conversational_ai_timeout_secs() -> u64 {
    1800
}

/// Conversational AI agent builder configuration (`[conversational_ai]` section).
///
/// **Status: Reserved for future use.** This configuration is parsed but not yet
/// consumed by the runtime. Setting `enabled = true` will produce a startup warning.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "conversational_ai"]
pub struct ConversationalAiConfig {
    /// Enable conversational AI features. Default: false.
    #[serde(default)]
    pub enabled: bool,
    /// Default language for conversations (BCP-47 tag). Default: "en".
    #[serde(default = "default_conversational_ai_language")]
    pub default_language: String,
    /// Supported languages for conversations. Default: [`en`, `de`, `fr`, `it`].
    #[serde(default = "default_conversational_ai_supported_languages")]
    pub supported_languages: Vec<String>,
    /// Automatically detect user language from message content. Default: true.
    #[serde(default = "default_true")]
    pub auto_detect_language: bool,
    /// Intent confidence below this threshold triggers escalation. Default: 0.3.
    #[serde(default = "default_conversational_ai_escalation_threshold")]
    pub escalation_confidence_threshold: f64,
    /// Maximum conversation turns before auto-ending. Default: 50.
    #[serde(default = "default_conversational_ai_max_turns")]
    pub max_conversation_turns: usize,
    /// Conversation timeout in seconds (inactivity). Default: 1800.
    #[serde(default = "default_conversational_ai_timeout_secs")]
    pub conversation_timeout_secs: u64,
    /// Enable conversation analytics tracking. Default: false (privacy-by-default).
    #[serde(default)]
    pub analytics_enabled: bool,
    /// Optional tool name for RAG-based knowledge base lookup during conversations.
    #[serde(default)]
    pub knowledge_base_tool: Option<String>,
}

impl ConversationalAiConfig {
    /// Returns `true` when the feature is disabled (the default).
    ///
    /// Used by `#[serde(skip_serializing_if)]` to omit the entire
    /// `[conversational_ai]` section from newly-generated config files,
    /// avoiding user confusion over an undocumented / experimental section.
    pub fn is_disabled(&self) -> bool {
        !self.enabled
    }
}

impl Default for ConversationalAiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_language: default_conversational_ai_language(),
            supported_languages: default_conversational_ai_supported_languages(),
            auto_detect_language: true,
            escalation_confidence_threshold: default_conversational_ai_escalation_threshold(),
            max_conversation_turns: default_conversational_ai_max_turns(),
            conversation_timeout_secs: default_conversational_ai_timeout_secs(),
            analytics_enabled: false,
            knowledge_base_tool: None,
        }
    }
}

// ── Security ops config ─────────────────────────────────────────

/// Managed Cybersecurity Service (MCSS) dashboard agent configuration (`[security_ops]`).
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "security_ops"]
pub struct SecurityOpsConfig {
    /// Enable security operations support. This no longer registers a
    /// model-callable tool: the diagnostics module stays compiled but is
    /// unreachable from every assembly path, and no operator surface
    /// exists for it. The flag is kept parseable so existing config files
    /// load unchanged; the daemon notes the withheld section at startup.
    #[serde(default)]
    pub enabled: bool,
    /// Directory containing incident response playbook definitions (JSON).
    #[serde(default = "default_playbooks_dir")]
    pub playbooks_dir: String,
    /// Automatically triage incoming alerts without user prompt.
    #[serde(default)]
    pub auto_triage: bool,
    /// Require human approval before executing playbook actions.
    #[serde(default = "default_require_approval")]
    pub require_approval_for_actions: bool,
    /// Maximum severity level that can be auto-remediated without approval.
    /// One of: "low", "medium", "high", "critical". Default: "low".
    #[serde(default = "default_max_auto_severity")]
    pub max_auto_severity: String,
    /// Directory for generated security reports.
    #[serde(default = "default_report_output_dir")]
    pub report_output_dir: String,
    /// Optional SIEM webhook URL for alert ingestion.
    #[serde(default)]
    pub siem_integration: Option<String>,
}

fn default_playbooks_dir() -> String {
    default_path_under_config_dir("playbooks")
}

fn default_require_approval() -> bool {
    true
}

fn default_max_auto_severity() -> String {
    "low".into()
}

fn default_report_output_dir() -> String {
    default_path_under_config_dir("security-reports")
}

impl Default for SecurityOpsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            playbooks_dir: default_playbooks_dir(),
            auto_triage: false,
            require_approval_for_actions: true,
            max_auto_severity: default_max_auto_severity(),
            report_output_dir: default_report_output_dir(),
            siem_integration: None,
        }
    }
}

// ── Config impl ──────────────────────────────────────────────────

impl Default for Config {
    fn default() -> Self {
        let home =
            UserDirs::new().map_or_else(|| PathBuf::from("."), |u| u.home_dir().to_path_buf());
        let zeroclaw_dir = home.join(".zeroclaw");

        Self {
            data_dir: zeroclaw_dir.join("data"),
            config_path: zeroclaw_dir.join("config.toml"),
            composition: None,
            env_overridden_paths: std::collections::HashSet::new(),
            pre_override_snapshots: std::collections::HashMap::new(),
            onepassword_reference_snapshots: std::collections::HashMap::new(),
            dirty_paths: std::collections::HashSet::new(),
            degraded_security: Vec::new(),
            degraded_sections: Vec::new(),
            retired_surface_warnings: Vec::new(),
            loaded_from: None,
            schema_version: crate::migration::CURRENT_SCHEMA_VERSION,
            providers: crate::providers::Providers::default(),
            model_routes: Vec::new(),
            embedding_routes: Vec::new(),
            observability: ObservabilityConfig::default(),
            trust: crate::scattered_types::TrustConfig::default(),
            backup: BackupConfig::default(),
            data_retention: DataRetentionConfig::default(),
            cloud_ops: CloudOpsConfig::default(),
            conversational_ai: ConversationalAiConfig::default(),
            security: SecurityConfig::default(),
            security_ops: SecurityOpsConfig::default(),
            runtime: RuntimeConfig::default(),
            reliability: ReliabilityConfig::default(),
            scheduler: SchedulerConfig::default(),
            eval: crate::scattered_types::EvalHarnessConfig::default(),
            pacing: PacingConfig::default(),
            skills: SkillsConfig::default(),
            pipeline: PipelineConfig::default(),
            heartbeat: HeartbeatConfig::default(),
            todotracker: TodoTrackerConfig::default(),
            cron: HashMap::new(),
            acp: AcpConfig::default(),
            channels: ChannelsConfig::default(),
            memory: MemoryConfig::default(),
            storage: StorageConfig::default(),
            tunnel: TunnelConfig::default(),
            gateway: GatewayConfig::default(),
            a2a: crate::multi_agent::A2aServerSection::default(),
            wss: WssConfig::default(),
            composio: ComposioConfig::default(),
            microsoft365: Microsoft365Config::default(),
            secrets: SecretsConfig::default(),
            browser: BrowserConfig::default(),
            http_request: HttpRequestConfig::default(),
            multimodal: MultimodalConfig::default(),
            media_pipeline: MediaPipelineConfig::default(),
            web_fetch: WebFetchConfig::default(),
            link_enricher: LinkEnricherConfig::default(),
            text_browser: TextBrowserConfig::default(),
            web_search: WebSearchConfig::default(),
            project_intel: ProjectIntelConfig::default(),
            google_workspace: GoogleWorkspaceConfig::default(),
            proxy: ProxyConfig::default(),
            cost: CostConfig::default(),
            peripherals: PeripheralsConfig::default(),
            agents: HashMap::new(),
            risk_profiles: HashMap::new(),
            runtime_profiles: HashMap::new(),
            personas: HashMap::new(),
            cards: HashMap::new(),
            companion_memory: crate::companion::CompanionMemoryConfig::default(),
            skill_bundles: HashMap::new(),
            knowledge_bundles: HashMap::new(),
            mcp_bundles: HashMap::new(),
            peer_groups: HashMap::new(),
            hooks: HooksConfig::default(),
            hardware: HardwareConfig::default(),
            query_classification: QueryClassificationConfig::default(),
            transcription: TranscriptionConfig::default(),
            tts: TtsConfig::default(),
            mcp: McpConfig::default(),
            nodes: NodesConfig::default(),
            onboard_state: OnboardStateConfig::default(),
            notion: NotionConfig::default(),
            jira: JiraConfig::default(),
            node_transport: NodeTransportConfig::default(),
            knowledge: KnowledgeConfig::default(),
            linkedin: LinkedInConfig::default(),
            image_gen: ImageGenConfig::default(),
            file_upload: FileUploadConfig::default(),
            file_upload_bundle: FileUploadBundleConfig::default(),
            file_download: FileDownloadConfig::default(),
            plugins: PluginsConfig::default(),
            locale: None,
            verifiable_intent: VerifiableIntentConfig::default(),
            sop: SopConfig::default(),
            shell_tool: ShellToolConfig::default(),
            escalation: EscalationConfig::default(),
        }
    }
}

fn default_config_and_data_dirs() -> Result<(PathBuf, PathBuf)> {
    let config_dir = default_config_dir()?;
    // The second value is the shared instance data directory
    // (databases + state files). Per-agent identity + markdown lives
    // at `<config-dir>/agents/<alias>/workspace/`, resolved separately
    // via `Config::agent_workspace_dir`.
    Ok((config_dir.clone(), config_dir.join("data")))
}

fn default_config_dir() -> Result<PathBuf> {
    if let Ok(custom) = std::env::var("ZEROCLAW_CONFIG_DIR") {
        let custom = custom.trim();
        if !custom.is_empty() {
            return Ok(expand_tilde_path(custom));
        }
    }

    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return Ok(PathBuf::from(home).join(".zeroclaw"));
    }

    let home = UserDirs::new()
        .map(|u| u.home_dir().to_path_buf())
        .context("Could not find home directory")?;
    Ok(home.join(".zeroclaw"))
}

/// Canonical on-disk directory for a locale's runtime/zerocode FTL catalogues:
/// `<config_dir>/data/ftl/<locale>/`. This is where `zeroclaw locales fetch`
/// writes downloaded translations and where the runtime i18n loader reads them.
/// `<config_dir>` honors `ZEROCLAW_CONFIG_DIR` and otherwise defaults to
/// `~/.zeroclaw`. The zerocode binary mirrors this path inline (it carries no
/// `zeroclaw-*` dependency).
pub fn ftl_locale_dir(locale: &str) -> Result<PathBuf> {
    Ok(default_config_dir()?.join("data").join("ftl").join(locale))
}

/// The FTL catalogues that `zeroclaw locales fetch` / the daemon's
/// `locales/fetch` RPC can download, as `(name, upstream-path-template,
/// output-filename)`. `{locale}` is substituted per request. This is the single
/// source of truth — a caller supplies only a catalog *name* matched against
/// this table, never a path.
pub const FTL_CATALOGS: &[(&str, &str, &str)] = &[
    (
        "cli",
        "crates/zeroclaw-runtime/locales/{locale}/cli.ftl",
        "cli.ftl",
    ),
    (
        "tools",
        "crates/zeroclaw-runtime/locales/{locale}/tools.ftl",
        "tools.ftl",
    ),
    (
        "zerocode",
        "apps/zerocode/locales/{locale}/zerocode.ftl",
        "zerocode.ftl",
    ),
];

/// Build a default path string by joining `relative` onto the resolved
/// platform config dir. The form sees the resolved absolute path
/// (`/home/<user>/.zeroclaw/<relative>` on Linux,
/// `C:\Users\<user>\.zeroclaw\<relative>` on Windows, etc.) instead of a
/// literal `~/...` token that doesn't expand on Windows. Falls back to
/// `~/.zeroclaw/<relative>` if the platform dir can't be resolved (rare —
/// e.g. no HOME and `directories::UserDirs` returns None); the runtime's
/// `expand_tilde_path()` handles that literal at use-time.
///
/// Switching to platform-native config locations (`~/Library/Application
/// Support/zeroclaw/` on macOS, `%APPDATA%\zeroclaw\` on Windows) is the
/// schema-v3 follow-up tracked in — that needs a migration to move
/// existing users' configs.
fn default_path_under_config_dir(relative: &str) -> String {
    match default_config_dir() {
        Ok(dir) => dir.join(relative).to_string_lossy().into_owned(),
        Err(_) => format!("~/.zeroclaw/{relative}"),
    }
}

pub fn resolve_config_dir_for_data(data_dir: &Path) -> (PathBuf, PathBuf) {
    let data_config_dir = data_dir.to_path_buf();
    if data_config_dir.join("config.toml").exists() {
        return (data_config_dir.clone(), data_config_dir.join("data"));
    }

    let legacy_config_dir = data_dir.parent().map(|parent| parent.join(".zeroclaw"));
    if let Some(legacy_dir) = legacy_config_dir {
        if legacy_dir.join("config.toml").exists() {
            return (legacy_dir, data_config_dir);
        }

        // Accept either the new "data" suffix or the legacy "workspace"
        // suffix; the V2->V3 filesystem migration renames the on-disk
        // dir but operator-set env-var paths from before the rename
        // still resolve correctly.
        if data_dir.file_name().is_some_and(|name| {
            name == std::ffi::OsStr::new("data") || name == std::ffi::OsStr::new("workspace")
        }) {
            return (legacy_dir, data_config_dir);
        }
    }

    (data_config_dir.clone(), data_config_dir.join("data"))
}

pub async fn classify_runtime_config_kind(config_path: &Path) -> RuntimeConfigKind {
    if path_is_under(config_path, &std::env::temp_dir()) {
        return RuntimeConfigKind::Temporary;
    }

    let Ok((default_config_dir, default_data_dir)) = default_config_and_data_dirs() else {
        return RuntimeConfigKind::Custom;
    };
    let Ok((resolved_config_dir, _data_dir, source)) =
        resolve_runtime_config_dirs(&default_config_dir, &default_data_dir).await
    else {
        return RuntimeConfigKind::Custom;
    };

    if !paths_equal_lexically(config_path, &resolved_config_dir.join("config.toml")) {
        return RuntimeConfigKind::Custom;
    }

    match source {
        ConfigResolutionSource::DefaultConfigDir | ConfigResolutionSource::HomebrewConfigDir => {
            RuntimeConfigKind::Default
        }
        ConfigResolutionSource::EnvConfigDir
        | ConfigResolutionSource::EnvDataDir
        | ConfigResolutionSource::EnvWorkspaceLegacy => RuntimeConfigKind::Custom,
    }
}

/// Resolve the current runtime config/data directories.
///
/// This mirrors the same precedence used by `Config::load_or_init()`:
/// `ZEROCLAW_CONFIG_DIR` > `ZEROCLAW_DATA_DIR` > `ZEROCLAW_WORKSPACE`
/// (deprecated) > defaults.
pub async fn resolve_runtime_dirs() -> Result<(PathBuf, PathBuf)> {
    let (default_zeroclaw_dir, default_data_dir) = default_config_and_data_dirs()?;
    let (config_dir, data_dir, _) =
        resolve_runtime_config_dirs(&default_zeroclaw_dir, &default_data_dir).await?;
    Ok((config_dir, data_dir))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigResolutionSource {
    EnvConfigDir,
    EnvDataDir,
    EnvWorkspaceLegacy,
    DefaultConfigDir,
    HomebrewConfigDir,
}

impl ConfigResolutionSource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::EnvConfigDir => "ZEROCLAW_CONFIG_DIR",
            Self::EnvDataDir => "ZEROCLAW_DATA_DIR",
            Self::EnvWorkspaceLegacy => "ZEROCLAW_WORKSPACE",
            Self::DefaultConfigDir => "default",
            Self::HomebrewConfigDir => "homebrew",
        }
    }
}

/// Expand tilde in paths, falling back to `UserDirs` when HOME is unset.
///
/// In non-TTY environments (e.g. cron), HOME may not be set, causing
/// `shellexpand::tilde` to return the literal `~` unexpanded. This helper
/// detects that case and uses `directories::UserDirs` as a fallback.
fn expand_tilde_path(path: &str) -> PathBuf {
    let expanded = shellexpand::tilde(path);
    let expanded_str = expanded.as_ref();

    // If the path still starts with '~', tilde expansion failed (HOME unset)
    if expanded_str.starts_with('~') {
        if let Some(user_dirs) = UserDirs::new() {
            let home = user_dirs.home_dir();
            // Replace leading ~ with home directory
            if let Some(rest) = expanded_str.strip_prefix('~') {
                return home.join(rest.trim_start_matches(['/', '\\']));
            }
        }
        // If UserDirs also fails, log a warning and use the literal path
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"path": path})),
            "Failed to expand tilde: HOME environment variable is not set and UserDirs failed. \
             In cron/non-TTY environments, use absolute paths or set HOME explicitly."
        );
    }

    PathBuf::from(expanded_str)
}

fn path_is_under(path: &Path, root: &Path) -> bool {
    path.components()
        .zip(root.components())
        .all(|(a, b)| a == b)
        && path.components().count() >= root.components().count()
}

fn paths_equal_lexically(a: &Path, b: &Path) -> bool {
    a.components().eq(b.components())
}

/// Returns the legacy plugin directories that still hold installed plugins not
/// visible to the runtime, which now scans [`PluginsConfig::resolved_plugins_dir`].
///
/// Historically `zeroclaw plugin install` wrote to `<data_dir>/plugins` (and,
/// before the data-dir rename, `<install_root>/workspace/plugins`). A directory
/// is reported only when it differs from the configured plugins dir and contains
/// at least one plugin (a subdirectory with a `manifest.toml`). Used to surface a
/// migration hint and to drive `zeroclaw plugin migrate`.
#[must_use]
pub fn legacy_plugin_dirs_with_entries(config: &Config) -> Vec<PathBuf> {
    let target = config.plugins.resolved_plugins_dir();
    [
        config.data_dir.join("plugins"),
        config.install_root_dir().join("workspace").join("plugins"),
    ]
    .into_iter()
    .filter(|legacy| *legacy != target && dir_has_plugin(legacy))
    .collect()
}

/// Returns `true` if `dir` holds at least one plugin (a subdirectory containing a
/// `manifest.toml`).
fn dir_has_plugin(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|entry| entry.path().join("manifest.toml").exists())
}

/// Detect if an executable path lives under a macOS Homebrew prefix and return
/// the Homebrew-managed config directory.
///
/// Homebrew can execute ZeroClaw from `<prefix>/Cellar/zeroclaw/<version>/bin/`,
/// `<prefix>/bin/`, or `<prefix>/opt/zeroclaw/bin/`.
async fn try_resolve_macos_homebrew_config_dir(exe: &Path) -> Option<PathBuf> {
    let parts = exe.iter().collect::<Vec<_>>();
    let prefix = match parts.as_slice() {
        [prefix @ .., cellar, formula, _version, bin, exe_name]
            if *cellar == std::ffi::OsStr::new("Cellar")
                && *formula == std::ffi::OsStr::new("zeroclaw")
                && *bin == std::ffi::OsStr::new("bin")
                && *exe_name == std::ffi::OsStr::new("zeroclaw") =>
        {
            prefix.iter().collect::<PathBuf>()
        }
        [prefix @ .., opt, formula, bin, exe_name]
            if *opt == std::ffi::OsStr::new("opt")
                && *formula == std::ffi::OsStr::new("zeroclaw")
                && *bin == std::ffi::OsStr::new("bin")
                && *exe_name == std::ffi::OsStr::new("zeroclaw") =>
        {
            let prefix = prefix.iter().collect::<PathBuf>();
            if !prefix.as_os_str().is_empty()
                && fs::metadata(prefix.join("Cellar"))
                    .await
                    .is_ok_and(|metadata| metadata.is_dir())
            {
                prefix
            } else {
                return None;
            }
        }
        [prefix @ .., bin, exe_name]
            if *bin == std::ffi::OsStr::new("bin")
                && *exe_name == std::ffi::OsStr::new("zeroclaw") =>
        {
            let prefix = prefix.iter().collect::<PathBuf>();
            if !prefix.as_os_str().is_empty()
                && fs::metadata(prefix.join("Cellar"))
                    .await
                    .is_ok_and(|metadata| metadata.is_dir())
            {
                prefix
            } else {
                return None;
            }
        }
        _ => return None,
    };
    Some(prefix.join("var").join("zeroclaw"))
}

async fn resolve_runtime_config_dirs(
    default_zeroclaw_dir: &Path,
    default_data_dir: &Path,
) -> Result<(PathBuf, PathBuf, ConfigResolutionSource)> {
    if let Ok(custom_config_dir) = std::env::var("ZEROCLAW_CONFIG_DIR") {
        let custom_config_dir = custom_config_dir.trim();
        if !custom_config_dir.is_empty() {
            // If the operator ALSO set ZEROCLAW_DATA_DIR or
            // ZEROCLAW_WORKSPACE, CONFIG_DIR wins; surface the
            // collision so they know which one took effect.
            if std::env::var("ZEROCLAW_DATA_DIR")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .is_some()
            {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                    "ZEROCLAW_CONFIG_DIR is set; ZEROCLAW_DATA_DIR is ignored \
                     (CONFIG_DIR pins both the config directory and the data \
                     directory under it)."
                );
            }
            if std::env::var("ZEROCLAW_WORKSPACE")
                .ok()
                .filter(|v| !v.is_empty())
                .is_some()
            {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                    "ZEROCLAW_CONFIG_DIR is set; ZEROCLAW_WORKSPACE (deprecated) \
                     is ignored. ZEROCLAW_WORKSPACE will be removed in a future \
                     release; switch any remaining references to ZEROCLAW_DATA_DIR."
                );
            }
            let zeroclaw_dir = expand_tilde_path(custom_config_dir);
            return Ok((
                zeroclaw_dir.clone(),
                zeroclaw_dir.join("data"),
                ConfigResolutionSource::EnvConfigDir,
            ));
        }
    }

    if let Ok(custom_data) = std::env::var("ZEROCLAW_DATA_DIR")
        && !custom_data.trim().is_empty()
    {
        if std::env::var("ZEROCLAW_WORKSPACE")
            .ok()
            .filter(|v| !v.is_empty())
            .is_some()
        {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "ZEROCLAW_DATA_DIR and ZEROCLAW_WORKSPACE are both set; \
                 ZEROCLAW_WORKSPACE (deprecated) is ignored. \
                 ZEROCLAW_WORKSPACE will be removed in a future release."
            );
        }
        let expanded = expand_tilde_path(&custom_data);
        let (zeroclaw_dir, data_dir) = resolve_config_dir_for_data(&expanded);
        return Ok((zeroclaw_dir, data_dir, ConfigResolutionSource::EnvDataDir));
    }

    if let Ok(custom_workspace) = std::env::var("ZEROCLAW_WORKSPACE")
        && !custom_workspace.is_empty()
    {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "ZEROCLAW_WORKSPACE is deprecated; use ZEROCLAW_DATA_DIR instead. \
             ZEROCLAW_WORKSPACE will be removed in a future release."
        );
        let expanded = expand_tilde_path(&custom_workspace);
        let (zeroclaw_dir, data_dir) = resolve_config_dir_for_data(&expanded);
        return Ok((
            zeroclaw_dir,
            data_dir,
            ConfigResolutionSource::EnvWorkspaceLegacy,
        ));
    }

    if cfg!(target_os = "macos")
        && let Ok(exe) = std::env::current_exe()
        && let Some(homebrew_config_dir) = try_resolve_macos_homebrew_config_dir(&exe).await
    {
        return Ok((
            homebrew_config_dir.clone(),
            homebrew_config_dir.join("workspace"),
            ConfigResolutionSource::HomebrewConfigDir,
        ));
    }

    Ok((
        default_zeroclaw_dir.to_path_buf(),
        default_data_dir.to_path_buf(),
        ConfigResolutionSource::DefaultConfigDir,
    ))
}

fn config_dir_creation_error(path: &Path) -> String {
    format!(
        "Failed to create config directory: {}. If running as an OpenRC service, \
         ensure this path is writable by user 'zeroclaw'.",
        path.display()
    )
}

/// Top-level keys that must always appear in the saved config even
/// when their value equals the default. `schema_version` is the
/// migration detector's anchor — dropping it from a freshly-saved
/// config would make the next load mis-detect the file as V1 (no
/// version key = V1).
const SAVE_PRESERVE_KEYS: &[&str] = &["schema_version"];

/// Insert a blank line before every `[section]` header that doesn't
/// already have one, so the serialized TOML reads as discrete blocks
/// instead of running every section header directly after the
/// previous line (`toml::to_string_pretty` doesn't gap between a
/// trailing scalar and the next section header).
fn ensure_blank_line_before_sections(toml: &str) -> String {
    let mut out = String::with_capacity(toml.len() + 64);
    let mut prev_line_blank = true; // start of file counts as blank
    for line in toml.lines() {
        let is_section_header = line.starts_with('[');
        if is_section_header && !prev_line_blank {
            out.push('\n');
        }
        out.push_str(line);
        out.push('\n');
        prev_line_blank = line.trim().is_empty();
    }
    out
}

/// Walk `actual` and drop every key whose value matches the same
/// key's value in `defaults`. Tables recurse; the recursion drops a
/// sub-table when every one of its keys was itself dropped (i.e. the
/// sub-table contained only defaults). Keys that don't appear in
/// `defaults` are operator-added and always survive.
///
/// HashMap-keyed sub-trees (e.g. `agents`, `providers.models.<family>`)
/// are not in the typed default tree, so their operator-added aliases
/// pass through this filter unchanged.
fn prune_default_values(actual: &mut toml::Table, defaults: &toml::Table) {
    let keys: Vec<String> = actual.keys().cloned().collect();
    for key in keys {
        if SAVE_PRESERVE_KEYS.contains(&key.as_str()) {
            continue;
        }
        let Some(default_value) = defaults.get(&key) else {
            // Operator added this key; not in the typed default tree.
            // Always keep — recursing in would either be a no-op or
            // strip operator content.
            continue;
        };
        let Some(child) = actual.remove(&key) else {
            continue;
        };
        let pruned = match (child, default_value) {
            (toml::Value::Table(mut child_table), toml::Value::Table(default_subtable)) => {
                prune_default_values(&mut child_table, default_subtable);
                if child_table.is_empty() {
                    None
                } else {
                    Some(toml::Value::Table(child_table))
                }
            }
            (child, default_value) => {
                if &child == default_value {
                    None
                } else {
                    Some(child)
                }
            }
        };
        if let Some(value) = pruned {
            actual.insert(key, value);
        }
    }
}

fn is_local_ollama_endpoint(api_url: Option<&str>) -> bool {
    let Some(raw) = api_url.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };

    reqwest::Url::parse(raw)
        .ok()
        .and_then(|url| url.host_str().map(|host| host.to_ascii_lowercase()))
        .is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1" | "0.0.0.0"))
}

fn is_official_ollama_cloud_endpoint(api_url: Option<&str>) -> bool {
    let Some(raw) = api_url.map(str::trim).filter(|value| !value.is_empty()) else {
        return false;
    };

    reqwest::Url::parse(raw)
        .ok()
        .and_then(|url| {
            url.host_str().map(|host| {
                host.eq_ignore_ascii_case("ollama.com")
                    || host.eq_ignore_ascii_case("api.ollama.com")
            })
        })
        .unwrap_or(false)
}

fn has_ollama_cloud_credential(config_api_key: Option<&str>) -> bool {
    config_api_key
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
}

/// Ensure that essential bootstrap files exist in the workspace directory.
///
/// When the workspace is created outside of Quickstart (e.g., non-tty
/// daemon/cron sessions), these files would otherwise be missing. This function
/// creates sensible defaults that allow the agent to operate with a basic identity.
pub async fn ensure_bootstrap_files(workspace_dir: &Path) -> Result<()> {
    let defaults: &[(&str, &str)] = &[
        (
            "IDENTITY.md",
            "# IDENTITY.md — Who Am I?\n\n\
             I am ZeroClaw, an autonomous AI agent.\n\n\
             ## Traits\n\
             - Helpful, precise, and safety-conscious\n\
             - I prioritize clarity and correctness\n",
        ),
        (
            "SOUL.md",
            "# SOUL.md — Who You Are\n\n\
             You are ZeroClaw, an autonomous AI agent.\n\n\
             ## Core Principles\n\
             - Be helpful and accurate\n\
             - Respect user intent and boundaries\n\
             - Ask before taking destructive actions\n\
             - Prefer safe, reversible operations\n",
        ),
    ];

    for (filename, content) in defaults {
        let path = workspace_dir.join(filename);
        if !path.exists() {
            fs::write(&path, content)
                .await
                .with_context(|| format!("Failed to create default {filename} in workspace"))?;
        }
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ExtraNestedModelProviderTable {
    family: String,
    alias: String,
    nested: String,
}

impl Config {
    /// External-peer usernames authorized on `<channel_type>.<alias>`.
    ///
    /// A `[peer_groups.<name>]` contributes when its `channel` field either
    /// matches `channel_type` (type-wide group, applies to every alias of
    /// that type) or matches the full dotted `"<channel_type>.<alias>"`
    /// (instance-scoped group, applies to that one alias only).
    pub fn channel_external_peers(&self, channel_type: &str, alias: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for group in self.peer_groups.values() {
            let group_matches = match group.channel.split_once('.') {
                Some((ty, al)) => ty == channel_type && al == alias,
                None => group.channel == channel_type,
            };
            if !group_matches {
                continue;
            }
            for peer in &group.external_peers {
                let username = peer.as_str().to_string();
                if seen.insert(username.clone()) {
                    out.push(username);
                }
            }
        }
        out
    }

    /// Voice-peer usernames for `<channel_type>.<alias>` that should always
    /// receive TTS voice replies.
    ///
    /// A `[peer_groups.<name>]` contributes when its `channel` field matches
    /// (type-wide or dotted) **and** `output_modality = "voice"`.
    ///
    /// This is the live-resolve counterpart of `channel_external_peers`,
    /// filtered to voice-only peer groups. No cache — single source of truth
    /// is `self.peer_groups`.
    pub fn channel_voice_peers(&self, channel_type: &str, alias: &str) -> Vec<String> {
        use crate::multi_agent::OutputModality;

        let mut out: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for group in self.peer_groups.values() {
            if group.output_modality != OutputModality::Voice {
                continue;
            }
            let group_matches = match group.channel.split_once('.') {
                Some((ty, al)) => ty == channel_type && al == alias,
                None => group.channel == channel_type,
            };
            if !group_matches {
                continue;
            }
            for peer in &group.external_peers {
                let username = peer.as_str().to_string();
                if seen.insert(username.clone()) {
                    out.push(username);
                }
            }
        }
        out
    }

    /// Sender usernames authorized to issue `/model --agent <model>` on
    /// `<channel_type>.<alias>` for agent `agent_alias`. A
    /// `[peer_groups.<name>]` contributes when its `channel` matches
    /// (type-wide or dotted), its `admin_for_agent_scope = true`, and its
    /// `agents` list (if non-empty) contains `agent_alias`. Returns
    /// deduped and sorted. Live-resolve against the `Config` this method
    /// is called on — no internal cache. Edits to `peer_groups` take
    /// effect at the next `Config::channel_agent_scope_admins` call; the
    /// orchestrator gate reads through a snapshot of this `Config`, so
    /// operator edits become visible after the runtime context is rebuilt
    /// (daemon restart).
    pub fn channel_agent_scope_admins(
        &self,
        channel_type: &str,
        alias: &str,
        agent_alias: &str,
    ) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for group in self.peer_groups.values() {
            if !group.admin_for_agent_scope {
                continue;
            }
            let group_matches = match group.channel.split_once('.') {
                Some((ty, al)) => ty == channel_type && al == alias,
                None => group.channel == channel_type,
            };
            if !group_matches {
                continue;
            }
            // Agent-bound: when the group's `agents` list is non-empty,
            // only grant the privilege if `agent_alias` appears in it.
            // An empty `agents` list is interpreted as "all agents"
            // (backward-compatible channel-wide default).
            if !group.agents.is_empty() && !group.agents.iter().any(|a| a.as_str() == agent_alias) {
                continue;
            }
            for peer in &group.external_peers {
                let username = peer.as_str().to_string();
                if seen.insert(username.clone()) {
                    out.push(username);
                }
            }
        }
        out.sort();
        out
    }

    /// Collect the `IntegrationDescriptor` from every nested config that
    /// declares one via `#[integration(...)]`. Adding a new toggleable
    /// integration is one struct-level attribute on the new config + one
    /// row in this method. The integrations registry consumes the result
    /// without per-vendor branches.
    pub fn integration_descriptors(&self) -> Vec<crate::config::IntegrationDescriptor> {
        // BrowserConfig and GoogleWorkspaceConfig carry
        // `#[integration(...)]` annotations on V3, so the macro emits
        // `integration_descriptor()` on each. Cron has been flattened
        // to `HashMap<String, CronJobDecl>` with no enable toggle, so
        // it gets a hand-crafted descriptor whose `active` reflects
        // whether any job is configured. Display copy lives next to
        // the field so the registry never branches on a category name.
        vec![
            self.browser.integration_descriptor(),
            self.google_workspace.integration_descriptor(),
            crate::config::IntegrationDescriptor {
                display_name: "Cron",
                description: "Scheduled tasks",
                category: "ToolsAutomation",
                active: !self.cron.is_empty(),
            },
        ]
    }

    /// Return top-level TOML keys in `raw_toml` that Config does not recognise.
    ///
    /// Keys present in `Config::default()` serialization pass immediately.
    /// Remaining keys are probed: the key is deserialized in isolation and
    /// the result compared to the default — a changed output means serde
    /// consumed it (covers `Option<T>` fields and `#[serde(alias)]` names).
    /// V1 legacy keys (consumed by migration) are also accepted.
    pub fn unknown_keys(raw_toml: &str) -> Vec<String> {
        let raw: toml::Table = match raw_toml.parse() {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        static DEFAULTS: OnceLock<toml::Table> = OnceLock::new();
        let defaults = DEFAULTS.get_or_init(|| {
            toml::to_string(&Config::default())
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or_default()
        });
        raw.keys()
            .filter(|key| {
                if defaults.contains_key(key.as_str()) {
                    return false;
                }
                if crate::migration::V1_LEGACY_KEYS.contains(&key.as_str()) {
                    return false;
                }
                let mut t = toml::Table::new();
                t.insert((*key).clone(), raw[key.as_str()].clone());
                let consumed = toml::to_string(&t)
                    .ok()
                    .and_then(|s| toml::from_str::<Config>(&s).ok())
                    .and_then(|c| toml::to_string(&c).ok())
                    .and_then(|s| s.parse::<toml::Table>().ok())
                    .is_some_and(|t| t != *defaults);
                !consumed
            })
            .cloned()
            .collect()
    }

    /// Return `<kind>.<family>` entries under `[providers]` in `raw_toml`
    /// whose family is not a known typed slot (kinds: models, tts,
    /// transcription). Serde silently drops these sections at deserialize
    /// time, so a typo'd family (`[providers.models.antropic.x]`) or one
    /// from a newer binary vanishes on reload: the alias "works" in the
    /// session that created it, then disappears after restart.
    pub fn unknown_provider_families(raw_toml: &str) -> Vec<String> {
        let raw: toml::Table = match raw_toml.parse() {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        let Some(kinds) = raw.get("providers").and_then(toml::Value::as_table) else {
            return Vec::new();
        };
        let kind_slots: &[(&str, &[&str])] = &[
            ("models", crate::providers::ModelProviders::slot_names()),
            ("tts", crate::providers::TtsProviders::slot_names()),
            (
                "transcription",
                crate::providers::TranscriptionProviders::slot_names(),
            ),
        ];
        let mut out = Vec::new();
        for (kind, slots) in kind_slots {
            let Some(families) = kinds.get(*kind).and_then(toml::Value::as_table) else {
                continue;
            };
            out.extend(
                families
                    .keys()
                    .filter(|k| !slots.contains(&k.as_str()))
                    .map(|k| format!("{kind}.{k}")),
            );
        }
        out
    }

    /// Return extra-nested model-provider alias tables that look like a
    /// misplaced provider config, e.g.
    /// `[providers.models.zai.default.default]`.
    ///
    /// Serde treats the first `default` as the alias and silently ignores the
    /// second `default` child table. When that child table carries provider
    /// fields such as `model`, `api_key`, or family-specific fields, the
    /// operator's settings are dropped while the empty alias still validates as
    /// an in-progress provider. This raw-TOML detector runs before that shape
    /// is erased by typed deserialization.
    fn extra_nested_model_provider_tables(raw_toml: &str) -> Vec<ExtraNestedModelProviderTable> {
        let raw: toml::Table = match raw_toml.parse() {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        let Some(models) = raw
            .get("providers")
            .and_then(toml::Value::as_table)
            .and_then(|providers| providers.get("models"))
            .and_then(toml::Value::as_table)
        else {
            return Vec::new();
        };

        let mut out = Vec::new();
        for (family, aliases_value) in models {
            if !crate::providers::ModelProviders::slot_names().contains(&family.as_str()) {
                continue;
            }
            let Some(aliases) = aliases_value.as_table() else {
                continue;
            };
            for (alias, alias_value) in aliases {
                let Some(alias_table) = alias_value.as_table() else {
                    continue;
                };
                for (nested_key, nested_value) in alias_table {
                    let Some(nested_table) = nested_value.as_table() else {
                        continue;
                    };
                    if nested_table.is_empty()
                        || Self::is_model_provider_table_valued_field(nested_key)
                    {
                        continue;
                    }
                    out.push(ExtraNestedModelProviderTable {
                        family: family.clone(),
                        alias: alias.clone(),
                        nested: nested_key.clone(),
                    });
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    fn is_model_provider_table_valued_field(key: &str) -> bool {
        // These shared model-provider fields legitimately deserialize from
        // nested TOML tables under an alias. Every other table-valued child of
        // `[providers.models.<family>.<alias>]` is an ignored extra section,
        // regardless of whether its contents are shared or family-specific
        // provider fields.
        matches!(
            key,
            "chat_template_kwargs" | "extra_headers" | "pricing" | "provider_extra"
        )
    }

    /// Returns `true` if `path` was populated by a `ZEROCLAW_*` env-var
    /// override at load time. O(1) HashSet lookup; safe to call per row in
    /// list-rendering paths (`config list`, dashboard, quickstart).
    pub fn prop_is_env_overridden(&self, path: &str) -> bool {
        self.env_overridden_paths.contains(path)
    }

    pub async fn load_or_init() -> Result<Self> {
        let (default_zeroclaw_dir, default_workspace_dir) = default_config_and_data_dirs()?;

        // Resolve env overrides FIRST so the migration runs against
        // the install root the operator actually uses. Running the
        // migration against `default_zeroclaw_dir` would silently skip
        // any install reached via `ZEROCLAW_CONFIG_DIR` or
        // `ZEROCLAW_WORKSPACE`.
        let (zeroclaw_dir, _legacy_workspace_dir, resolution_source) =
            resolve_runtime_config_dirs(&default_zeroclaw_dir, &default_workspace_dir).await?;

        // One-time, V<3 → V3 ONLY move of `<install>/workspace/` into
        // `<install>/agents/default/workspace/`. The "default" alias is
        // the migration bridge — it must NEVER appear on a fresh install
        // or on a V3 install that already declared its own aliases.
        //
        // Gate strictly on the on-disk config's `schema_version`:
        // - missing config.toml → fresh install, skip.
        // - schema_version >= 3 → already V3, skip.
        // - schema_version 1 or 2 → upgrade in progress, run.
        // Anything else (parse failure, weird value) is treated as
        // "don't touch the filesystem"; the TOML migrator will surface
        // the real error.
        let config_toml_path = zeroclaw_dir.join("config.toml");
        let needs_fs_migration = config_toml_path.is_file()
            && matches!(
                std::fs::read_to_string(&config_toml_path)
                    .ok()
                    .and_then(|raw| toml::from_str::<toml::Value>(&raw).ok())
                    .and_then(|v| crate::migration::detect_version(&v).ok()),
                Some(v) if v < crate::migration::CURRENT_SCHEMA_VERSION
            );
        if needs_fs_migration
            && let Err(e) = crate::schema::v2::migrate_v2_to_v3_install_filesystem(&zeroclaw_dir)
        {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({
                        "install": zeroclaw_dir.display().to_string(),
                        "error": format!("{}", e),
                    })),
                "[system] filesystem migration failed; continuing with legacy layout"
            );
        } else if !needs_fs_migration
            && let Err(e) =
                crate::schema::v2::relocate_default_agent_skills_to_shared(&zeroclaw_dir)
        {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({
                        "install": zeroclaw_dir.display().to_string(),
                        "error": format!("{}", e),
                    })),
                "[system] skills relocation to shared workspace failed; continuing"
            );
        }

        let config_path = zeroclaw_dir.join("config.toml");

        // The install dir is the only directory `load_or_init` creates
        // unconditionally. Per-agent workspaces (`agents/<alias>/workspace/`)
        // are seeded lazily at agent-loop entry by
        // `Agent::from_config_with_session_cwd_and_mcp`, which runs
        // `ensure_bootstrap_files` for the agent it is starting. There
        // is no fresh-install "default" agent and therefore no
        // `agents/default/workspace/` synthesized at boot; the only
        // legitimate origin for that directory is the V1/V2→V3
        // legacy-workspace migration above, which fires only when a
        // pre-multi-agent install's `<install>/workspace/` is present
        // and needs to be moved into the new layout.
        //
        // `config.data_dir` resolves to `<install>/data/` — the shared
        // instance data directory holding databases (memory, sessions,
        // cost records) and hygiene/state files. Per-agent identity
        // and markdown (MEMORY.md, IDENTITY.md, SOUL.md) lives at
        // `Config::agent_workspace_dir(alias)` instead.
        let data_dir = zeroclaw_dir.join("data");
        fs::create_dir_all(&data_dir).await.with_context(|| {
            format!(
                "Failed to create data directory: {}",
                data_dir.display().to_string()
            )
        })?;
        // Legacy alias retained for clarity in the struct initializer
        // and existing field assignments below.
        let workspace_dir = data_dir;

        // `<install>/shared/` — root workspace shared across every agent
        // on the host. Holds skills, skill bundles, and other content
        // not scoped to a single agent. Per-agent state still lives at
        // `<install>/agents/<alias>/workspace/`.
        let shared_dir = zeroclaw_dir.join("shared");
        fs::create_dir_all(&shared_dir).await.with_context(|| {
            format!(
                "Failed to create shared workspace directory: {}",
                shared_dir.display()
            )
        })?;

        fs::create_dir_all(&zeroclaw_dir)
            .await
            .with_context(|| config_dir_creation_error(&zeroclaw_dir))?;

        if config_path.exists() {
            // Warn if config file is world-readable (may contain API keys)
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = fs::metadata(&config_path).await
                    && meta.permissions().mode() & 0o004 != 0
                {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                        &format!(
                            "Config file {:?} is world-readable (mode {:o}). \
                             Consider restricting with: chmod 600 {:?}",
                            config_path,
                            meta.permissions().mode() & 0o777,
                            config_path
                        )
                    );
                }
            }

            let contents = fs::read_to_string(&config_path)
                .await
                .context("Failed to read config file")?;

            // Deserialize the config with the standard TOML parser.
            //
            // Previously this used `serde_ignored::deserialize` for both
            // deserialization and unknown-key detection. However,
            // `serde_ignored` silently drops field values inside nested
            // structs that carry `#[serde(default)]` (e.g. the entire
            // `[autonomy]` table), causing user-supplied values to be
            // replaced by defaults.
            //
            // We now deserialize with `toml::from_str` (which is correct)
            // and run `serde_ignored` separately just for diagnostics.
            //
            // `migrate_to_current` parses the TOML, detects the schema
            // version, runs the typed V1→V2→V3 chain via `V1Config::migrate`
            // / `V2Config::migrate`, and deserializes the result into the
            // current `Config` shape.
            //
            // Detect the on-disk version up-front so we can emit one WARN
            // line when the daemon auto-migrates an older config in memory:
            // the disk file is left untouched and the user is advised to lock
            // the migration in with `zeroclaw config migrate`. The parsed raw
            // root also feeds the composition gate below, so parse once here.
            let raw_root = toml::from_str::<toml::Value>(&contents).ok();
            let stale_version = raw_root
                .as_ref()
                .and_then(|v| crate::migration::detect_version(v).ok())
                .filter(|n| *n != crate::migration::CURRENT_SCHEMA_VERSION);

            // Retired-section tombstone: serde silently drops the unknown
            // nested section (no `deny_unknown_fields`), so deployments
            // carrying it keep parsing; the shared helper records one
            // structured warning per retired section so the retirement is
            // visible instead of a silent no-op. Runs BEFORE the
            // composition hard-error gate so a config with both problems
            // still surfaces the tombstone WARN even though the load then
            // fails. Sunset: this tombstone is a compatibility shim and
            // will be removed in a later announced window.
            let retired_section_warnings =
                crate::validation_warnings::retired_section_tombstones(&contents)
                    .into_iter()
                    .chain(crate::validation_warnings::retired_field_tombstones(
                        &contents,
                    ))
                    .collect::<Vec<_>>();

            // `composition` hard-error gate. The key is brand-new (no
            // release predating it can legitimately carry a value this
            // binary didn't ship), so an invalid value is an operator
            // typo, never a legacy artifact. Without this gate the
            // resilient loader below would silently drop the key and
            // resolve absent → `full`, widening the assembled tool
            // surface past what the operator asked for. Validity is
            // decided by the enum's own deserializer (single source of
            // truth for accepted values); the message names the
            // documented set including the `legacy` alias, which the raw
            // serde error omits.
            if let Some(raw_composition) = raw_root.as_ref().and_then(|v| v.get("composition"))
                && <crate::composition::Composition as serde::Deserialize>::deserialize(
                    raw_composition.clone(),
                )
                .is_err()
            {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "value": raw_composition.to_string(),
                        })),
                    "config.toml `composition` value is invalid; refusing to load"
                );
                anyhow::bail!(
                    "config.toml `composition` value {} is invalid; expected one of {}",
                    raw_composition,
                    crate::composition::DOCUMENTED_VALUES
                );
            }

            // Daemon load must never hard-fail on a malformed config — the
            // operator needs the process up to repair it. The resilient path
            // degrades (dropping invalid blocks to defaults); security-critical
            // drops are recorded on `degraded_security` for exposure gating.
            // Strict validation lives in `zeroclaw config migrate`.
            let salvage = crate::migration::migrate_to_current_salvaged(&contents);
            let mut config: Config = salvage.config;
            config.degraded_security = salvage.dropped_security;
            config.degraded_sections = salvage.dropped;
            if let Some(from_version) = stale_version {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                    &format!(
                        "Config at {} is schema_version {from_version}; auto-migrated to {} in memory. \
                     Run `zeroclaw config migrate` to commit the migration to disk. \
                     V0.8.0 also replaced the env-var override grammar; see \
                     https://github.com/zeroclaw-labs/zeroclaw/blob/master/docs/book/src/reference/env-vars.md \
                     for the migration recipes.",
                        config_path.display().to_string(),
                        crate::migration::CURRENT_SCHEMA_VERSION
                    )
                );
            }

            // Ensure the built-in default auto_approve entries are always
            // present on a `risk_profiles.default` entry that already
            // exists (typically post-V1/V2→V3 migration). When a user
            // specifies `auto_approve` in their TOML (e.g. to add a
            // custom tool), serde replaces the default list instead of
            // merging — this re-adds the framework defaults so safe
            // tools like `weather` and `calculator` keep their
            // auto-approve status.
            //
            // Users who want to require approval for a default tool can
            // add it to `always_ask`, which takes precedence over
            // `auto_approve` in the approval decision (see approval/mod.rs).
            //
            // Skipped when the loaded config has no `risk_profiles.default`
            // entry: we will not synthesize a `default` alias here.
            // `default` is a migration artifact (V1/V2→V3
            // single-instance bridge); a config that arrives without it
            // is a legitimate multi-aliased shape and must not have one
            // injected at load time.
            if let Some(default_profile) = config.risk_profiles.get_mut("default") {
                default_profile.ensure_default_auto_approve();
            }

            // Detect unknown top-level config keys by comparing the raw
            // TOML table keys against what Config actually deserializes.
            // This replaces the previous serde_ignored-based approach which
            // had false-positive issues with #[serde(default)] nested structs.
            for key in Self::unknown_keys(&contents) {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"key": key})),
                    "Unknown config key ignored: \"\". Check config.toml for typos or deprecated options."
                );
            }
            // Unknown provider families are dropped by serde, so an alias
            // created under a typo'd or unsupported family silently vanishes
            // on reload while agents.*.model_provider still references it.
            for entry in Self::unknown_provider_families(&contents) {
                let (kind, family) = entry.split_once('.').unwrap_or(("models", entry.as_str()));
                let reference = if kind == "models" {
                    "any agents.*.model_provider referencing them will fail to resolve; \
                     run `zeroclaw providers` for valid family names"
                } else {
                    "references to its aliases will fail to resolve"
                };
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"kind": kind, "family": family})),
                    &format!(
                        "[providers.{kind}.{family}] section dropped: not a known {kind} \
                        provider family. Its aliases will not load and {reference}."
                    )
                );
            }
            // Extra child tables under a known model-provider alias are also
            // erased by serde. If the child carries provider-shaped fields,
            // call out the likely mistaken nesting instead of letting the
            // parent alias look like an ordinary unfinished provider.
            for entry in Self::extra_nested_model_provider_tables(&contents) {
                let family = entry.family.as_str();
                let alias = entry.alias.as_str();
                let nested = entry.nested.as_str();
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "family": family,
                            "alias": alias,
                            "nested": nested,
                        })),
                    &format!(
                        "[providers.models.{}.{}.{}] section ignored: \
                         model-provider fields must live directly under \
                         [providers.models.{}.{}]. Move the nested keys up one level.",
                        family, alias, nested, family, alias
                    )
                );
            }
            // Set computed paths that are skipped during serialization
            config.config_path = config_path.clone();
            config.data_dir = workspace_dir;
            config.loaded_from = Some(config_path.clone());

            // Ensure each configured skill-bundle's resolved directory
            // exists on disk so the bundle has somewhere for skills to
            // land immediately. Idempotent.
            let install_root = config.install_root_dir();
            for alias in config.skill_bundles.keys().cloned().collect::<Vec<_>>() {
                if let Ok(dir) =
                    crate::skill_bundles::resolve_directory(&config, &install_root, &alias)
                    && let Err(e) = std::fs::create_dir_all(&dir)
                {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                        &format!(
                            "skill-bundle '{alias}' directory creation failed at {}: {e}",
                            dir.display().to_string()
                        )
                    );
                }
            }

            let store = crate::secrets::SecretStore::new(&zeroclaw_dir, config.secrets.encrypt);
            config.onepassword_reference_snapshots =
                collect_onepassword_reference_snapshots(&config);
            // Decrypt all #[secret]-annotated fields via Configurable derive
            config = tokio::task::spawn_blocking(move || {
                config.decrypt_secrets(&store)?;
                Ok::<_, anyhow::Error>(config)
            })
            .await
            .context("Config secret decryption task failed")??;

            // Apply ZEROCLAW_<lowercase_path> env-var overrides. Hard-errors
            // on any unresolvable path — no silent ignores. Tracks overridden
            // paths and per-path pre-override snapshots so save() can mask
            // env-injected values back to the original on-disk state.
            let applied = crate::env_overrides::apply_env_overrides(&mut config)?;
            config.env_overridden_paths = applied.paths;
            config.pre_override_snapshots = applied.snapshots;
            config.retired_surface_warnings = retired_section_warnings
                .into_iter()
                .chain(applied.tombstone_warnings)
                .collect();

            // Validation must NOT prevent the daemon from booting. If
            // it did, a single broken agent reference would lock the
            // operator out of `/config` — the only place they can fix
            // it. Demote to a startup warning; the gateway and dashboard
            // still come up so the user can navigate to the bad section
            // and repair it.
            if let Err(e) = config.validate() {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": format!("{e:#}")})),
                    "[system] config has validation errors — booting anyway so you \
                     can fix them via /config or `zeroclaw config set`"
                );
            }
            ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"path": config.config_path.display().to_string(), "workspace": config.data_dir.display().to_string(), "source": resolution_source.as_str(), "initialized": true})), "Config loaded");
            Ok(config)
        } else {
            let mut config = Config {
                config_path: config_path.clone(),
                data_dir: workspace_dir,
                ..Config::default()
            };
            // A brand-new install starts on the minimal companion
            // composition. This bootstrap and the shipped config
            // templates (dev container, production container images,
            // Kubernetes sample) are the places a fresh config file is
            // produced, and all of them pin the key explicitly.
            // Existing installs are never migrated: an absent
            // `composition` field keeps resolving as `full`, so upgrades
            // do not silently change the tool surface.
            config.composition = Some(crate::composition::Composition::Minimal);
            // Save defaults FIRST so env-injected values never reach the
            // freshly-created config file. Env overrides apply post-save to
            // populate the in-memory Config for the running process.
            config.save().await?;
            // This value just wrote the file; mark provenance so later full
            // saves of the same value stay legitimate.
            config.loaded_from = Some(config_path.clone());

            // Restrict permissions on newly created config file (may contain API keys)
            #[cfg(unix)]
            {
                use std::{fs::Permissions, os::unix::fs::PermissionsExt};
                let _ = fs::set_permissions(&config_path, Permissions::from_mode(0o600)).await;
            }

            let applied = crate::env_overrides::apply_env_overrides(&mut config)?;
            config.env_overridden_paths = applied.paths;
            config.pre_override_snapshots = applied.snapshots;
            config.retired_surface_warnings = applied.tombstone_warnings;

            // Same boot-resilience as the load-existing branch above:
            // a fresh-init config can't realistically fail validation,
            // but if it does we still want the daemon up.
            if let Err(e) = config.validate() {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": format!("{e:#}")})),
                    "[system] freshly-initialized config has validation errors — \
                     booting anyway so you can fix them via /config"
                );
            }
            ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"path": config.config_path.display().to_string(), "workspace": config.data_dir.display().to_string(), "source": resolution_source.as_str(), "initialized": true})), "Config loaded");
            Ok(config)
        }
    }

    /// Collect non-fatal validation warnings — config that loads and
    /// validates successfully (`validate()` returns `Ok(())`) but will fail
    /// at runtime because of a logical inconsistency the schema cannot
    /// enforce structurally.
    ///
    /// Called by `validate()` (which emits each warning via `tracing::warn!`
    /// for log visibility) and by the gateway HTTP API (which returns the
    /// structured list in `PropResponse` / `PatchResponse` so dashboard
    /// callers see the same signal the CLI sees on stderr).
    ///
    /// Adding a new warning: append a check here, pick a stable `code`,
    /// and document the code in `validation_warnings.rs`.
    pub fn collect_warnings(&self) -> Vec<crate::validation_warnings::ValidationWarning> {
        let mut warnings = Vec::new();
        // Load-time observations (retired env prefixes, retired file
        // sections) have no other in-config representation; replay them so
        // the structured warning surface stays the single exit point.
        warnings.extend(self.retired_surface_warnings.iter().cloned());
        self.collect_fallback_warnings(&mut warnings);
        self.collect_cross_provider_summary_model_warnings(&mut warnings);
        self.collect_a2a_exposed_skills_warnings(&mut warnings);
        self.collect_memory_semantic_search_warnings(&mut warnings);
        // Must run after `collect_cross_provider_summary_model_warnings`: it
        // scans `warnings` to suppress its generic inert `summary_model`
        // warning when the more specific cross-provider diagnostic already
        // covers the same path.
        self.collect_context_compression_ignored_warnings(&mut warnings);
        self.collect_deprecated_otp_action_gating_warnings(&mut warnings);
        warnings.extend(validate_memory_semantics(&self.memory));
        // `wire_api` is only honored by bring-your-own-endpoint families; on a
        // branded family with a fixed wire protocol it is silently ignored.
        // Surface that so an operator who sets it on, e.g., `mistral` learns it
        // has no effect instead of debugging unexpected runtime behavior.
        for (family, alias, entry) in self.providers.models.iter_entries() {
            if entry.wire_api.is_some() && !crate::provider_aliases::family_honors_wire_api(family)
            {
                warnings.push(crate::validation_warnings::ValidationWarning::new(
                    "wire_api_not_supported_for_family",
                    format!(
                        "wire_api is set on `{family}.{alias}` but the `{family}` family has a \
                         fixed wire protocol and ignores it. wire_api only takes effect on the \
                         openai, llamacpp, and custom (openai-compatible) families."
                    ),
                    format!("providers.models.{family}.{alias}.wire_api"),
                ));
            }
        }
        warnings
    }

    /// Surface sqlite semantic/hybrid search with no effective embedder. The
    /// runtime treats `NoopEmbedding` as dimensions 0 and silently skips the
    /// vector path, so derive this from the canonical memory settings instead
    /// of storing another effective-mode field.
    fn collect_memory_semantic_search_warnings(
        &self,
        warnings: &mut Vec<crate::validation_warnings::ValidationWarning>,
    ) {
        if !Self::memory_backend_is_sqlite(&self.memory.backend) {
            return;
        }
        if !matches!(
            &self.memory.search_mode,
            SearchMode::Embedding | SearchMode::Hybrid
        ) {
            return;
        }
        if self.memory_has_effective_embedder() {
            return;
        }

        let search_mode = match &self.memory.search_mode {
            SearchMode::Bm25 => "bm25",
            SearchMode::Embedding => "embedding",
            SearchMode::Hybrid => "hybrid",
        };
        warnings.push(crate::validation_warnings::ValidationWarning::new(
            "memory_semantic_search_without_embedder",
            format!(
                "memory.search_mode is {search_mode:?} on sqlite memory, but no effective \
                 embedding provider is configured; vector search is skipped and \
                 recall falls back to keyword-only behavior. Configure \
                 memory.embedding_provider or a valid memory.embedding_model \
                 hint route, or set memory.search_mode = \"bm25\"."
            ),
            "memory.search_mode",
        ));
    }

    fn memory_backend_is_sqlite(backend: &str) -> bool {
        let trimmed = backend.trim();
        let family = trimmed
            .split_once('.')
            .map_or(trimmed, |(family, _)| family)
            .trim();
        family.eq_ignore_ascii_case("sqlite")
    }

    fn memory_has_effective_embedder(&self) -> bool {
        let fallback = || Self::memory_embedding_settings_have_effective_embedder(&self.memory);
        let Some(hint) = self
            .memory
            .embedding_model
            .strip_prefix("hint:")
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return fallback();
        };

        let Some(route) = self
            .embedding_routes
            .iter()
            .find(|route| route.hint.trim() == hint)
        else {
            return fallback();
        };

        if Self::embedding_route_has_effective_embedder(route, self.memory.embedding_dimensions) {
            return true;
        }
        fallback()
    }

    fn memory_embedding_settings_have_effective_embedder(memory: &MemoryConfig) -> bool {
        let provider = memory.embedding_provider.trim();
        !provider.is_empty()
            && !provider.eq_ignore_ascii_case("none")
            && memory.embedding_dimensions > 0
    }

    fn embedding_route_has_effective_embedder(
        route: &EmbeddingRouteConfig,
        fallback_dimensions: usize,
    ) -> bool {
        let provider = route.model_provider.trim();
        let model = route.model.trim();
        let dimensions = route.dimensions.unwrap_or(fallback_dimensions);
        !provider.is_empty()
            && !provider.eq_ignore_ascii_case("none")
            && !model.is_empty()
            && dimensions > 0
    }

    /// Surface the A2A `exposed_skills` filter that can never resolve a skill.
    /// `exposed_skills` is a narrowing filter over the agent's resolved skill
    /// set, which comes from `skill_bundles`. An agent that lists
    /// `a2a.exposed_skills` but declares no `skill_bundles` advertises an empty
    /// `skills: []` array on its agent card with no error, because every wanted
    /// id is dropped for belonging to a bundle the agent does not declare. The
    /// same silent emptiness happens when a wanted id names no skill in any
    /// declared bundle, or when the owning bundle's include/exclude filter
    /// excludes it; those need disk resolution, so this offline check covers the
    /// common structural case: exposed skills with zero declared bundles.
    fn collect_a2a_exposed_skills_warnings(
        &self,
        warnings: &mut Vec<crate::validation_warnings::ValidationWarning>,
    ) {
        for (agent_alias, agent) in &self.agents {
            if agent.a2a.exposed_skills.is_empty() {
                continue;
            }
            if !agent.skill_bundles.is_empty() {
                continue;
            }
            warnings.push(crate::validation_warnings::ValidationWarning::new(
                "a2a_exposed_skills_without_bundles",
                format!(
                    "agent `{agent_alias}` lists a2a.exposed_skills but declares no \
                     skill_bundles, so its agent card advertises no skills (skills: []). \
                     exposed_skills is a filter over the agent's resolved skill set; add \
                     the owning bundle(s) to agents.{agent_alias}.skill_bundles so the \
                     listed skills resolve."
                ),
                format!("agents.{agent_alias}.a2a.exposed_skills"),
            ));
        }
    }

    /// Surface cross-provider ambiguity in a legacy config while reporting
    /// the current contract: context compression has no runtime consumer, so
    /// this knob is inert like every other `context_compression` field (see
    /// `collect_context_compression_ignored_warnings`). This diagnostic adds
    /// config-shape detail as a more specific companion for the same line. The
    /// deprecated
    /// `runtime_profiles.<p>.context_compression.summary_model` is a bare
    /// model id that names no provider of its own — it would need to be
    /// resolved onto each consuming agent's OWN provider were the field ever
    /// read again — so when a single profile is shared by agents resolving
    /// to MORE THAN ONE distinct provider, that one bare id is ambiguous for
    /// at least one of them. A `summary_provider` supplies provider identity
    /// for this narrower diagnostic and excludes the corresponding value from
    /// the ambiguity count; it does not make context compression functional.
    ///
    /// The diagnostic is offline and deterministic: no schema bump, no
    /// network, and no model catalog. It names the profile, the affected
    /// agents, and their differing providers, then recommends removing the
    /// unsupported setting or waiting for an accepted compression design.
    fn collect_cross_provider_summary_model_warnings(
        &self,
        warnings: &mut Vec<crate::validation_warnings::ValidationWarning>,
    ) {
        for (profile_alias, profile) in &self.runtime_profiles {
            // Only the deprecated bare summary_model lacks provider identity.
            // A profile-level summary_provider excludes this narrower
            // ambiguity shape, but context compression remains inert.
            if !profile
                .context_compression
                .summary_provider
                .trim()
                .is_empty()
            {
                continue;
            }
            let Some(summary_model) = profile.context_compression.summary_model.as_deref() else {
                continue;
            };
            if summary_model.trim().is_empty() {
                continue;
            }

            // Gather agents that reference this profile and have no agent-level
            // summary_provider identity. An override excludes that agent from
            // this ambiguity diagnostic but does not make compression
            // functional. Resolve the provider that would be paired with the
            // bare model if a future implementation consumed this config.
            let mut affected: Vec<(String, String)> = Vec::new();
            for (agent_alias, agent) in &self.agents {
                if agent.runtime_profile.trim() != profile_alias {
                    continue;
                }
                if !agent.summary_provider.trim().is_empty() {
                    continue;
                }
                let provider_label = self.canonical_provider_label(agent.model_provider.trim());
                affected.push((agent_alias.clone(), provider_label));
            }

            // Cross-provider ambiguity requires distinct providers. A
            // same-provider bare id is still inert, but the generic
            // context-compression warning reports that fact without this
            // additional ambiguity detail.
            let distinct: std::collections::BTreeSet<&str> =
                affected.iter().map(|(_, p)| p.as_str()).collect();
            if distinct.len() < 2 {
                continue;
            }

            let mut agents_sorted: Vec<&(String, String)> = affected.iter().collect();
            agents_sorted.sort_by(|a, b| a.0.cmp(&b.0));
            let detail = agents_sorted
                .iter()
                .map(|(name, provider)| format!("{name} -> {provider}"))
                .collect::<Vec<_>>()
                .join(", ");

            warnings.push(crate::validation_warnings::ValidationWarning::new(
                "cross_provider_summary_model",
                format!(
                    "runtime_profiles.{profile_alias}.context_compression.summary_model \
                     ({summary_model:?}) is set, but context compression is not currently \
                     implemented in the runtime; this setting has no effect. It is also a \
                     bare model id reused by agents resolving to different providers \
                     ({detail}), which names no provider of its own and would be ambiguous \
                     for at least one of them if compression were read again. Remove the \
                     unsupported context_compression setting (every context_compression \
                     field, including summary_provider, is currently inert), or wait for a \
                     separately accepted compression design before configuring it."
                ),
                format!("runtime_profiles.{profile_alias}.context_compression.summary_model"),
            ));
        }
    }

    /// Surface every non-default `context_compression` knob as inert: the
    /// runtime context compressor was removed and nothing in the
    /// workspace reads `context_compression` at runtime anymore, so the whole
    /// struct — `enabled`, thresholds, protected counts, summarizer limits,
    /// provider selection, tool-result retrimming — has no effect. Mirrors
    /// `validate_memory_semantics`: one warning per non-default field, so a
    /// user sees exactly which of their authored knobs are dead. A field
    /// explicitly written at its default value is indistinguishable from an
    /// omitted one post-deserialization and stays silent (same limitation as
    /// `validate_memory_semantics`).
    ///
    /// Only `[runtime_profiles.<alias>.context_compression]` is checked —
    /// `AliasedAgentConfig` (`[agents.<alias>]`) has no `context_compression`
    /// field of its own, and the legacy pre-V3 `[agent.context_compression]`
    /// top-level table (folded into `[runtime_profiles.default]` by the V2→V3
    /// migration, see `schema/v2.rs`) has already collapsed into this same
    /// surface by the time `Config` exists, so a single pass over
    /// `runtime_profiles` covers both the historical and current authored
    /// forms without double-warning.
    fn collect_context_compression_ignored_warnings(
        &self,
        warnings: &mut Vec<crate::validation_warnings::ValidationWarning>,
    ) {
        let defaults = crate::scattered_types::ContextCompressionConfig::default();
        for (alias, profile) in &self.runtime_profiles {
            let cc = &profile.context_compression;

            // `enabled = true` gets its own message: it is the master switch
            // users flip expecting compression to happen at all.
            if cc.enabled {
                warnings.push(crate::validation_warnings::ValidationWarning::new(
                    "context_compression_unsupported",
                    format!(
                        "runtime_profiles.{alias}.context_compression.enabled is set but context \
                         compression is not currently implemented in the runtime (the compressor \
                         was removed in); this setting has no effect."
                    ),
                    format!("runtime_profiles.{alias}.context_compression.enabled"),
                ));
            }

            // Every other knob: flag any value that differs from the default.
            let mut inert: Vec<&'static str> = Vec::new();
            if (cc.threshold_ratio - defaults.threshold_ratio).abs() > f64::EPSILON {
                inert.push("threshold_ratio");
            }
            if cc.protect_first_n != defaults.protect_first_n {
                inert.push("protect_first_n");
            }
            if cc.protect_last_n != defaults.protect_last_n {
                inert.push("protect_last_n");
            }
            if cc.max_passes != defaults.max_passes {
                inert.push("max_passes");
            }
            if cc.summary_max_chars != defaults.summary_max_chars {
                inert.push("summary_max_chars");
            }
            if cc.source_max_chars != defaults.source_max_chars {
                inert.push("source_max_chars");
            }
            if cc.timeout_secs != defaults.timeout_secs {
                inert.push("timeout_secs");
            }
            if cc.summary_provider != defaults.summary_provider {
                inert.push("summary_provider");
            }
            if cc.summary_model != defaults.summary_model {
                // Ordering dependency: `collect_warnings()` runs
                // `collect_cross_provider_summary_model_warnings` before this
                // helper, so a cross-provider `summary_model` diagnostic for
                // this same path is already in `warnings`. That diagnostic
                // reports the same "has no effect" fact plus the specific
                // cross-provider agents affected, so it wins and the generic
                // inert warning is skipped to avoid printing two warnings
                // for the same config line. All other `summary_model`
                // shapes (single-provider, unshared) still get the inert
                // warning; no other diagnostic covers them.
                let summary_model_path =
                    format!("runtime_profiles.{alias}.context_compression.summary_model");
                if !warnings.iter().any(|w| {
                    w.code == "cross_provider_summary_model" && w.path == summary_model_path
                }) {
                    inert.push("summary_model");
                }
            }
            if cc.identifier_policy != defaults.identifier_policy {
                inert.push("identifier_policy");
            }
            if cc.tool_result_retrim_chars != defaults.tool_result_retrim_chars {
                inert.push("tool_result_retrim_chars");
            }
            if cc.tool_result_trim_exempt != defaults.tool_result_trim_exempt {
                inert.push("tool_result_trim_exempt");
            }

            for field in inert {
                warnings.push(crate::validation_warnings::ValidationWarning::new(
                    "context_compression_unsupported",
                    format!(
                        "runtime_profiles.{alias}.context_compression.{field} is set to a \
                         non-default value but context compression is not currently implemented \
                         in the runtime (the compressor was removed in); this setting has \
                         no effect."
                    ),
                    format!("runtime_profiles.{alias}.context_compression.{field}"),
                ));
            }
        }
    }

    /// Surface every authored `[security.otp]` action-gating knob as
    /// deprecated and unsupported: ZeroClaw has no OTP action-gating, so
    /// `gated_actions`, `gated_domains`, `gated_domain_categories`, and
    /// `challenge_max_attempts` have no runtime consumer and provide no
    /// authorization boundary. OTP itself remains a live authentication
    /// mechanism — `enabled`, `token_ttl_secs`, and `cache_valid_secs`
    /// drive TOTP validation, the replay-cache window, and the e-stop
    /// resume challenge (`method` is parsed for forward compatibility;
    /// only TOTP is implemented today) — and authorization for high-risk
    /// actions is owned by the Tachi approval/grant plane with Node-local
    /// enforcement, not by OTP config.
    ///
    /// Mirrors `collect_context_compression_ignored_warnings`: one warning
    /// per non-default field, so a user sees exactly which of their
    /// authored knobs are dead. A field explicitly written at its default
    /// value is indistinguishable from an omitted one post-deserialization
    /// and stays silent (same limitation as that collector).
    fn collect_deprecated_otp_action_gating_warnings(
        &self,
        warnings: &mut Vec<crate::validation_warnings::ValidationWarning>,
    ) {
        let otp = &self.security.otp;
        let deprecated = "is deprecated and unsupported: ZeroClaw has no OTP action-gating, so it \
                          is parsed for backward compatibility but not enforced and provides no \
                          authorization boundary. High-risk action authorization is owned by the \
                          Tachi approval/grant plane with Node-local enforcement. Remove the \
                          setting.";
        if otp.gated_actions != default_otp_gated_actions() {
            warnings.push(crate::validation_warnings::ValidationWarning::new(
                "otp_action_gating_unsupported",
                format!("security.otp.gated_actions {deprecated}"),
                "security.otp.gated_actions",
            ));
        }
        if !otp.gated_domains.is_empty() {
            warnings.push(crate::validation_warnings::ValidationWarning::new(
                "otp_action_gating_unsupported",
                format!("security.otp.gated_domains {deprecated}"),
                "security.otp.gated_domains",
            ));
        }
        if !otp.gated_domain_categories.is_empty() {
            warnings.push(crate::validation_warnings::ValidationWarning::new(
                "otp_action_gating_unsupported",
                format!("security.otp.gated_domain_categories {deprecated}"),
                "security.otp.gated_domain_categories",
            ));
        }
        if otp.challenge_max_attempts != default_otp_challenge_max_attempts() {
            warnings.push(crate::validation_warnings::ValidationWarning::new(
                "otp_action_gating_unsupported",
                format!("security.otp.challenge_max_attempts {deprecated}"),
                "security.otp.challenge_max_attempts",
            ));
        }
    }

    /// Canonical label for an agent's resolved model provider, used to decide
    /// whether two agents sit on distinct providers. A non-empty ref that
    /// resolves through `[providers.models]` collapses to its canonical
    /// `<family>.<alias>` so equivalent spellings (bare vs dotted) compare
    /// equal; an empty or unresolved ref keeps its raw form (empty becomes a
    /// stable sentinel) so it still participates as a distinct bucket.
    fn canonical_provider_label(&self, provider_ref: &str) -> String {
        if provider_ref.is_empty() {
            return "<agent default provider>".to_string();
        }
        match self.providers.models.find_by_name(provider_ref) {
            Some((family, alias, _)) => format!("{family}.{alias}"),
            None => provider_ref.to_string(),
        }
    }

    /// Surface non-fatal issues in per-alias `fallback` chains: dangling refs
    /// (a fallback naming an alias that is not configured) and cycles (a
    /// fallback path that loops back onto itself). Both are warn-and-skip — the
    /// chain still loads and runs with the offending edge pruned at build time.
    fn collect_fallback_warnings(
        &self,
        warnings: &mut Vec<crate::validation_warnings::ValidationWarning>,
    ) {
        for (family, alias, cfg) in self.providers.models.iter_entries() {
            self.collect_fallback_model_warnings(family, alias, cfg, warnings);
            if cfg.fallback.is_empty() {
                continue;
            }
            let root = format!("{family}.{alias}");
            let mut visited: Vec<String> = vec![root.clone()];
            self.walk_fallback(&root, &cfg.fallback, &mut visited, 1, warnings);
        }
    }

    /// Surface `fallback_models` entries the build path silently skips: blank
    /// entries and entries that duplicate the alias's primary `model`. The skip
    /// itself is safe, but without a warning an operator never learns that a
    /// listed fallback model is doing nothing.
    fn collect_fallback_model_warnings(
        &self,
        family: &str,
        alias: &str,
        cfg: &ModelProviderConfig,
        warnings: &mut Vec<crate::validation_warnings::ValidationWarning>,
    ) {
        let Some(primary) = cfg.model.as_deref() else {
            return;
        };
        for (i, model) in cfg.fallback_models.iter().enumerate() {
            let path = format!("providers.models.{family}.{alias}.fallback_models[{i}]");
            if model.trim().is_empty() {
                warnings.push(crate::validation_warnings::ValidationWarning::new(
                    "empty_fallback_model",
                    format!(
                        "fallback_models entry {i} on {family}.{alias} is empty; \
                         it is skipped at runtime"
                    ),
                    path,
                ));
            } else if model == primary {
                warnings.push(crate::validation_warnings::ValidationWarning::new(
                    "fallback_model_duplicates_primary",
                    format!(
                        "fallback_models entry {model:?} on {family}.{alias} duplicates the \
                         primary model; it is skipped at runtime"
                    ),
                    path,
                ));
            }
        }
    }

    fn walk_fallback(
        &self,
        from: &str,
        refs: &[crate::providers::ModelProviderRef],
        visited: &mut Vec<String>,
        depth: usize,
        warnings: &mut Vec<crate::validation_warnings::ValidationWarning>,
    ) {
        if depth > crate::providers::MAX_FALLBACK_DEPTH {
            warnings.push(crate::validation_warnings::ValidationWarning::new(
                "max_fallback_depth_exceeded",
                format!(
                    "fallback chain from {from} exceeds the maximum depth of {}; \
                     deeper links are pruned at runtime",
                    crate::providers::MAX_FALLBACK_DEPTH
                ),
                format!("providers.models.{from}.fallback"),
            ));
            return;
        }
        for (i, fallback_ref) in refs.iter().enumerate() {
            let raw = fallback_ref.as_str().trim();
            if raw.is_empty() {
                continue;
            }
            let path = format!("providers.models.{from}.fallback[{i}]");
            let Some((family, alias, cfg)) = self.providers.models.find_by_name(raw) else {
                warnings.push(crate::validation_warnings::ValidationWarning::new(
                    "dangling_fallback_ref",
                    format!(
                        "fallback {raw:?} on {from} does not resolve to a configured \
                         providers.models entry; this fallback link is skipped at runtime"
                    ),
                    path,
                ));
                continue;
            };
            let resolved = format!("{family}.{alias}");
            if visited.iter().any(|v| v == &resolved) {
                warnings.push(crate::validation_warnings::ValidationWarning::new(
                    "fallback_cycle",
                    format!(
                        "fallback {raw:?} on {from} closes a cycle \
                         ({} -> {resolved}); the cycle edge is pruned at runtime",
                        visited.join(" -> ")
                    ),
                    path,
                ));
                continue;
            }
            visited.push(resolved.clone());
            self.walk_fallback(&resolved, &cfg.fallback, visited, depth + 1, warnings);
            visited.pop();
        }
    }

    /// Walk every channel HashMap that carries reply pacing fields and yield
    /// `(dotted-path-prefix, &dyn HasReplyPacing)` pairs. Used by validation
    /// and by anywhere that wants a single source of truth for which channel
    /// types participate in pacing.
    pub fn reply_pacing_entries(&self) -> Vec<(String, &dyn HasReplyPacing)> {
        let c = &self.channels;
        fn rows<'a, C: HasReplyPacing>(
            ch_type: &'static str,
            map: &'a std::collections::HashMap<String, C>,
        ) -> impl Iterator<Item = (String, &'a dyn HasReplyPacing)> + 'a {
            map.iter().map(move |(alias, cfg)| {
                (
                    format!("channels.{ch_type}.{alias}"),
                    cfg as &dyn HasReplyPacing,
                )
            })
        }
        rows("telegram", &c.telegram)
            .chain(rows("discord", &c.discord))
            .chain(rows("slack", &c.slack))
            .chain(rows("mattermost", &c.mattermost))
            .chain(rows("webhook", &c.webhook))
            .chain(rows("imessage", &c.imessage))
            .chain(rows("matrix", &c.matrix))
            .chain(rows("signal", &c.signal))
            .chain(rows("whatsapp", &c.whatsapp))
            .collect()
    }

    /// Validate configuration values that would cause runtime failures.
    ///
    /// Called after TOML deserialization and env-override application to catch
    /// obviously invalid values early instead of failing at arbitrary runtime points.
    pub fn validate(&self) -> Result<()> {
        validate_memory_rerank_config(&self.memory)?;
        if let Err(reason) = self.companion_memory.validate() {
            validation_bail!(
                RequiredFieldEmpty,
                "companion_memory.owner.principal_id",
                "{reason}"
            );
        }
        if let Err(reason) = crate::node_allowlist::validate_config(self) {
            validation_bail!(InvalidFormat, "allowed_tools", "{reason}");
        }

        // Tunnel — OpenVPN
        if self.tunnel.tunnel_provider.trim() == "openvpn" {
            let openvpn = self.tunnel.openvpn.as_ref().ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                    "tunnel.tunnel_provider='openvpn' rejected: [tunnel.openvpn] block missing"
                );
                anyhow::Error::msg("tunnel.tunnel_provider='openvpn' requires [tunnel.openvpn]")
            })?;

            if openvpn.config_file.trim().is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    "tunnel.openvpn.config_file",
                    "tunnel.openvpn.config_file must not be empty"
                );
            }
            if openvpn.connect_timeout_secs == 0 {
                validation_bail!(
                    InvalidNumericRange,
                    "tunnel.openvpn.connect_timeout_secs",
                    "tunnel.openvpn.connect_timeout_secs must be greater than 0"
                );
            }
        }

        let mut lucid_aliases: Vec<&String> = self.storage.lucid.keys().collect();
        lucid_aliases.sort();
        for alias in lucid_aliases {
            let lucid = &self.storage.lucid[alias];
            if lucid
                .binary_path
                .as_ref()
                .is_some_and(|path| path.trim().is_empty())
            {
                validation_bail!(
                    RequiredFieldEmpty,
                    format!("storage.lucid.{alias}.binary_path"),
                    "storage.lucid.{alias}.binary_path must not be empty"
                );
            }
            if matches!(lucid.recall_timeout_ms, Some(0)) {
                validation_bail!(
                    InvalidNumericRange,
                    format!("storage.lucid.{alias}.recall_timeout_ms"),
                    "storage.lucid.{alias}.recall_timeout_ms must be greater than 0"
                );
            }
            if matches!(lucid.store_timeout_ms, Some(0)) {
                validation_bail!(
                    InvalidNumericRange,
                    format!("storage.lucid.{alias}.store_timeout_ms"),
                    "storage.lucid.{alias}.store_timeout_ms must be greater than 0"
                );
            }
        }

        // Reply-pacing bounds — both `reply_min_interval_secs` and
        // `reply_queue_depth_max` walk through one entry list so adding
        // a new paced channel only requires extending `reply_pacing_entries`.
        for (path_prefix, cfg) in self.reply_pacing_entries() {
            let secs = cfg.reply_min_interval_secs();
            if secs > REPLY_MIN_INTERVAL_MAX_SECS {
                let path = format!("{path_prefix}.reply_min_interval_secs");
                validation_bail!(
                    InvalidNumericRange,
                    path,
                    "{path} = {secs} is out of range; must be 0..={REPLY_MIN_INTERVAL_MAX_SECS}"
                );
            }
            let depth = cfg.reply_queue_depth_max();
            if depth > REPLY_QUEUE_DEPTH_CEILING {
                let path = format!("{path_prefix}.reply_queue_depth_max");
                validation_bail!(
                    InvalidNumericRange,
                    path,
                    "{path} = {depth} is out of range; must be 0..={REPLY_QUEUE_DEPTH_CEILING}"
                );
            }
        }

        for (alias, tg) in &self.channels.telegram {
            tg.validate_bot_token(alias)?;
            validate_http_base_url(
                &format!("channels.telegram.{alias}.api_base_url"),
                &tg.api_base_url,
            )?;
        }

        for (alias, dc) in &self.channels.discord {
            dc.validate_bot_token(alias)?;
        }

        // Git forge channel: a PAT-backed provider must name its API origin
        // explicitly. There is deliberately no default host - every request
        // carries `access_token` as a bearer credential, so guessing an
        // endpoint (e.g. gitea.com) would disclose the token to a host the
        // operator never configured. Only enabled aliases are checked so a
        // half-filled disabled block doesn't fail the whole config.
        for (alias, git) in &self.channels.git {
            let provider = git.provider.trim().to_ascii_lowercase();
            if git.enabled && matches!(provider.as_str(), "gitea" | "forgejo") {
                let path = format!("channels.git.{alias}.api_base_url");
                match git.api_base_url.as_deref() {
                    None => validation_bail!(
                        RequiredFieldEmpty,
                        path,
                        "{path} is required when provider = \"{provider}\": set the \
                         instance's API base URL including /api/v1 (e.g. \
                         https://git.example.org/api/v1); no default host is assumed \
                         because API requests carry the access token"
                    ),
                    Some(url) => validate_http_base_url(&path, url)?,
                }
            }
        }

        for name in self.http_request.secrets.keys() {
            if name.is_empty()
                || name.len() > 64
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                validation_bail!(
                    InvalidFormat,
                    format!("http_request.secrets.{name}"),
                    "http_request.secrets key {name:?} must contain 1..=64 ASCII letters, numbers, underscores, or hyphens"
                );
            }
        }

        // Gateway
        if self.gateway.host.trim().is_empty() {
            validation_bail!(
                RequiredFieldEmpty,
                "gateway.host",
                "gateway.host must not be empty"
            );
        }
        if self.nodes.mdns.max_peers == 0 {
            validation_bail!(
                InvalidNumericRange,
                "nodes.mdns.max_peers",
                "nodes.mdns.max_peers must be greater than 0"
            );
        }
        if self.nodes.mdns.announce_interval_secs == 0 {
            validation_bail!(
                InvalidNumericRange,
                "nodes.mdns.announce_interval_secs",
                "nodes.mdns.announce_interval_secs must be greater than 0"
            );
        }
        if self.nodes.mdns.peer_ttl_secs == 0 {
            validation_bail!(
                InvalidNumericRange,
                "nodes.mdns.peer_ttl_secs",
                "nodes.mdns.peer_ttl_secs must be greater than 0"
            );
        }
        if self.nodes.mdns.peer_ttl_secs <= self.nodes.mdns.announce_interval_secs {
            validation_bail!(
                InvalidNumericRange,
                "nodes.mdns.peer_ttl_secs",
                "nodes.mdns.peer_ttl_secs must be greater than nodes.mdns.announce_interval_secs"
            );
        }
        if matches!(self.transcription.max_audio_bytes, Some(0)) {
            validation_bail!(
                InvalidNumericRange,
                "transcription.max_audio_bytes",
                "transcription.max_audio_bytes must be greater than zero"
            );
        }
        if self.channels.max_concurrent_per_channel == 0 {
            validation_bail!(
                InvalidNumericRange,
                "channels.max_concurrent_per_channel",
                "channels.max_concurrent_per_channel must be greater than 0"
            );
        }
        // Typed-memory producers are a SQLite-only slice: the enabled write
        // path stores through `store_with_options_and_agent`, and only the
        // SQLite backend persists the full typed StoreOptions surface. Any
        // other backend would hit the trait default's explicit failure deep
        // inside spawned background consolidation, so reject the
        // configuration at load instead.
        if self.memory.types.enabled || self.memory.consolidation_extract_facts {
            let flag_path = if self.memory.types.enabled {
                "memory.types.enabled"
            } else {
                "memory.consolidation_extract_facts"
            };
            // Mirror the runtime's backend classification
            // (`backend_kind_from_dotted`): trim, take the prefix before the
            // first dot, compare case-insensitively.
            let backend_kind = self
                .memory
                .backend
                .trim()
                .split('.')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if backend_kind != "sqlite" {
                validation_bail!(
                    InvalidFormat,
                    flag_path,
                    "{flag_path} = true requires memory.backend = \"sqlite\" (typed memory storage is SQLite-only), but memory.backend = {:?}",
                    self.memory.backend
                );
            }
            for (alias, agent) in &self.agents {
                if agent.memory.backend != crate::multi_agent::MemoryBackendKind::Sqlite {
                    let agent_backend = agent.memory.backend;
                    validation_bail!(
                        InvalidFormat,
                        format!("agents.{alias}.memory.backend"),
                        "{flag_path} = true requires every agent on the sqlite memory backend (typed memory storage is SQLite-only), but agents.{alias}.memory.backend = {agent_backend:?}",
                    );
                }
            }
        }
        for (alias, agent) in &self.agents {
            if agent.precheck.timeout_secs == 0 {
                validation_bail!(
                    InvalidNumericRange,
                    format!("agents.{alias}.precheck.timeout_secs"),
                    "agents.{alias}.precheck.timeout_secs must be greater than 0"
                );
            }
        }
        // Cards: a card carries an agent's voice, its tool grants, and its
        // MCP reach as one authored unit. Letting it sit alongside the
        // pointers/fields it supersedes would mean an agent's real
        // authority depends on a precedence rule, and the failure mode of a
        // forgotten precedence rule is an agent reaching further than its
        // author believed. Refuse the ambiguity.
        //
        // `mcp_bundles` is checked here too: `Config::resolved_agent_config`
        // has a carded agent's `cards[card].grants.mcp_bundles` REPLACE
        // `agents.<alias>.mcp_bundles` outright (not union), so a raw
        // `mcp_bundles` left on a carded agent is silently ignored — the
        // same forgotten-precedence hazard this check exists to kill.
        // Configs that set `mcp_bundles` on an agent as a pre-card
        // workaround must drop it once the agent is carded: the card is now
        // the sole authority for that agent's MCP reach.
        for (alias, agent) in &self.agents {
            let card = agent.card.as_str().trim();
            if card.is_empty() {
                continue;
            }
            let Some(definition) = self.cards.get(card) else {
                validation_bail!(
                    DanglingReference,
                    format!("agents.{alias}.card"),
                    "agents.{alias}.card = {card:?} but no [cards.{card}] entry is configured"
                );
            };
            if let Err(reason) = definition.validate() {
                validation_bail!(
                    InvalidFormat,
                    format!("cards.{card}.grants"),
                    "cards.{card}: {reason}"
                );
            }
            for (field, value) in [
                ("persona", agent.persona.as_str()),
                ("risk_profile", agent.risk_profile.as_str()),
            ] {
                if !value.trim().is_empty() {
                    validation_bail!(
                        InvalidFormat,
                        format!("agents.{alias}.{field}"),
                        "agents.{alias} sets both card = {card:?} and {field} = {value:?}; \
                         a card already carries both, so set the card alone or drop it"
                    );
                }
            }
            // `mcp_bundles` is a `Vec<String>`, not a `&str` field, so it
            // cannot join the loop above — same check, own arm.
            if !agent.mcp_bundles.is_empty() {
                validation_bail!(
                    InvalidFormat,
                    format!("agents.{alias}.mcp_bundles"),
                    "agents.{alias} sets both card = {card:?} and mcp_bundles = {:?}; \
                     the card already carries the agent's MCP reach, so set the card \
                     alone or drop mcp_bundles",
                    agent.mcp_bundles
                );
            }
        }
        // Heartbeat agent: when heartbeat is enabled, the agent field
        // must name a configured agent.
        if self.heartbeat.enabled {
            let hb_agent = self.heartbeat.agent.trim();
            if hb_agent.is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    "heartbeat.agent",
                    "heartbeat.agent must reference a configured agent when heartbeat.enabled = true"
                );
            }
            if !self.agents.contains_key(hb_agent) {
                validation_bail!(
                    DanglingReference,
                    "heartbeat.agent",
                    "heartbeat.agent = {hb_agent:?} but no [agents.{hb_agent}] entry is configured"
                );
            }
        }
        if let Some(ref prefix) = self.gateway.path_prefix {
            // Validate the raw value — no silent trimming so the stored
            // value is exactly what was validated.
            if !prefix.is_empty() {
                if !prefix.starts_with('/') {
                    validation_bail!(
                        InvalidFormat,
                        "gateway.path_prefix",
                        "gateway.path_prefix must start with '/'"
                    );
                }
                if prefix.ends_with('/') {
                    validation_bail!(
                        InvalidFormat,
                        "gateway.path_prefix",
                        "gateway.path_prefix must not end with '/' (including bare '/')"
                    );
                }
                // Reject characters unsafe for URL paths or HTML/JS injection.
                // Whitespace is intentionally excluded from the allowed set.
                if let Some(bad) = prefix.chars().find(|c| {
                    !matches!(c, '/' | '-' | '_' | '.' | '~'
                        | 'a'..='z' | 'A'..='Z' | '0'..='9'
                        | '!' | '$' | '&' | '\'' | '(' | ')' | '*' | '+' | ',' | ';' | '='
                        | ':' | '@')
                }) {
                    anyhow::bail!(
                        "gateway.path_prefix contains invalid character '{bad}'; \
                         only unreserved and sub-delim URI characters are allowed"
                    );
                }
            }
        }

        // Skill bundles — directories must stay inside `<install>/shared/`
        // and no two bundles may resolve to the same directory. Default
        // directory and the rules themselves live in
        // [`crate::skill_bundles`] so the runtime SkillsService and this
        // validator share one implementation.
        if !self.skill_bundles.is_empty() {
            let install_root = self.install_root_dir();
            for alias in self.skill_bundles.keys() {
                let dir = crate::skill_bundles::resolve_directory(self, &install_root, alias)
                    .map_err(|e| {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Reject
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "skill_bundle": alias,
                                "error": format!("{}", e),
                            })),
                            "skill_bundles.<alias>.directory could not be resolved"
                        );
                        anyhow::Error::msg(e.to_string())
                    })?;
                if let Err(e) = crate::skill_bundles::validate_directory(&dir, &install_root) {
                    validation_bail!(
                        InvalidFormat,
                        format!("skill-bundles.{alias}.directory"),
                        "{e}"
                    );
                }
            }
            if let Err(e) = crate::skill_bundles::validate_uniqueness(self, &install_root) {
                validation_bail!(InvalidFormat, "skill_bundles", "{e}");
            }
        }

        // MCP bundles: resolution is secure by default, so a dangling
        // reference fails closed (the agent is granted fewer servers, never
        // more). Surface the misconfiguration as a warning so an operator is
        // not left wondering why an agent's MCP tools vanished, without
        // turning a typo into a hard startup failure.
        {
            // Operator UX: secure-by-default means an agent with no
            // `mcp_bundles` grant connects to ZERO MCP servers. When MCP is
            // enabled and `[[mcp.servers]]` is non-empty but no
            // `[mcp_bundles.*]` exists at all, every agent silently gets
            // nothing. That is the inverse of the original silent
            // no-op and surprises operators upgrading from <0.8.3. Warn
            // once at startup so this surfaces via `doctor` and the
            // standard startup warning stream.
            if self.mcp.enabled && !self.mcp.servers.is_empty() && self.mcp_bundles.is_empty() {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note,)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "mcp_servers_configured": self.mcp.servers.len(),
                        })),
                    "[[mcp.servers]] is configured but no [mcp_bundles.*] bundles exist; no agent will receive any MCP tools. Add a [mcp_bundles.<alias>] entry and reference it from agents.<alias>.mcp_bundles to grant access."
                );
            }

            let known_servers: std::collections::HashSet<&str> =
                self.mcp.servers.iter().map(|s| s.name.as_str()).collect();
            for (bundle_alias, bundle) in &self.mcp_bundles {
                for server in bundle.servers.iter().chain(bundle.exclude.iter()) {
                    if !known_servers.contains(server.as_str()) {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "mcp_bundle": bundle_alias,
                                "server": server,
                            })),
                            "mcp_bundles.<alias> references an MCP server name not in [mcp.servers]; it grants nothing"
                        );
                    }
                }
            }
            for agent_alias in self.agents.keys() {
                let Some(agent) = self.resolved_agent_config(agent_alias) else {
                    continue;
                };
                for bundle_alias in &agent.mcp_bundles {
                    if !self.mcp_bundles.contains_key(bundle_alias) {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "agent": agent_alias,
                                "mcp_bundle": bundle_alias,
                            })),
                            "agents.<alias>.mcp_bundles references an undefined [mcp_bundles.<alias>]; the agent is granted no servers from it"
                        );
                    }
                }
            }
        }

        // Validate every configured risk profile. Each profile stands on
        // its own — there is no "active" or "default" risk profile concept;
        // an agent's `risk_profile` field names exactly which one applies.
        let mut profile_aliases: Vec<&String> = self.risk_profiles.keys().collect();
        profile_aliases.sort();
        for profile_alias in profile_aliases {
            let profile = &self.risk_profiles[profile_alias];
            for (i, env_name) in profile.shell_env_passthrough.iter().enumerate() {
                if !is_valid_env_var_name(env_name) {
                    anyhow::bail!(
                        "risk_profiles.{profile_alias}.shell_env_passthrough[{i}] is invalid ({env_name}); expected [A-Za-z_][A-Za-z0-9_]*"
                    );
                }
            }
        }

        // Security OTP / estop
        if self.security.otp.challenge_max_attempts == 0 {
            validation_bail!(
                InvalidNumericRange,
                "security.otp.challenge_max_attempts",
                "security.otp.challenge_max_attempts must be greater than 0"
            );
        }
        if self.security.otp.token_ttl_secs == 0 {
            validation_bail!(
                InvalidNumericRange,
                "security.otp.token_ttl_secs",
                "security.otp.token_ttl_secs must be greater than 0"
            );
        }
        if self.security.otp.cache_valid_secs == 0 {
            validation_bail!(
                InvalidNumericRange,
                "security.otp.cache_valid_secs",
                "security.otp.cache_valid_secs must be greater than 0"
            );
        }
        if self.security.otp.cache_valid_secs < self.security.otp.token_ttl_secs {
            anyhow::bail!(
                "security.otp.cache_valid_secs must be greater than or equal to security.otp.token_ttl_secs"
            );
        }
        if self.security.otp.challenge_max_attempts == 0 {
            validation_bail!(
                InvalidNumericRange,
                "security.otp.challenge_max_attempts",
                "security.otp.challenge_max_attempts must be greater than 0"
            );
        }
        for (i, action) in self.security.otp.gated_actions.iter().enumerate() {
            let normalized = action.trim();
            if normalized.is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    format!("security.otp.gated_actions[{i}]"),
                    "security.otp.gated_actions[{i}] must not be empty"
                );
            }
            if !normalized
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                anyhow::bail!(
                    "security.otp.gated_actions[{i}] contains invalid characters: {normalized}"
                );
            }
            if !default_otp_gated_actions()
                .iter()
                .any(|known| known == normalized)
            {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "action": normalized,
                            "known_actions": default_otp_gated_actions(),
                        })),
                    "security.otp.gated_actions is deprecated and never enforced (ZeroClaw has no \
                     OTP action-gating); entry does not match a known action name: "
                );
            }
        }
        DomainMatcher::new(
            &self.security.otp.gated_domains,
            &self.security.otp.gated_domain_categories,
        )
        .with_context(
            || "Invalid security.otp.gated_domains or security.otp.gated_domain_categories",
        )?;
        if self.security.estop.state_file.trim().is_empty() {
            validation_bail!(
                RequiredFieldEmpty,
                "security.estop.state_file",
                "security.estop.state_file must not be empty"
            );
        }
        if !(0.0..=1.0).contains(&self.security.leak_detection.sensitivity) {
            validation_bail!(
                InvalidNumericRange,
                "security.leak_detection.sensitivity",
                "security.leak_detection.sensitivity must be between 0.0 and 1.0"
            );
        }

        // Scheduler
        if self.scheduler.max_concurrent == 0 {
            validation_bail!(
                InvalidNumericRange,
                "scheduler.max_concurrent",
                "scheduler.max_concurrent must be greater than 0"
            );
        }
        if self.scheduler.max_tasks == 0 {
            validation_bail!(
                InvalidNumericRange,
                "scheduler.max_tasks",
                "scheduler.max_tasks must be greater than 0"
            );
        }

        // Model routes
        for (i, route) in self.model_routes.iter().enumerate() {
            if route.hint.trim().is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    format!("model_routes[{i}].hint"),
                    "model_routes[{i}].hint must not be empty"
                );
            }
            let mp = route.model_provider.trim();
            if mp.is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    format!("model_routes[{i}].model_provider"),
                    "model_routes[{i}].model_provider must not be empty"
                );
            }
            // Route refs are dotted `<type>.<alias>` and must resolve to a
            // configured `[providers.models.<type>.<alias>]` entry. Unresolved
            // routes are dropped at runtime construction; rejecting them here
            // keeps that drift visible at config-load time.
            match mp.split_once('.') {
                Some((ty, inner)) if !ty.is_empty() && !inner.is_empty() => {
                    if self.providers.models.find(ty, inner).is_none() {
                        validation_bail!(
                            DanglingReference,
                            format!("model_routes[{i}].model_provider"),
                            "model_routes[{i}].model_provider = {mp:?} but providers.models.{ty}.{inner} is not configured",
                        );
                    }
                }
                _ => validation_bail!(
                    InvalidFormat,
                    format!("model_routes[{i}].model_provider"),
                    "model_routes[{i}].model_provider must be dotted form `<type>.<alias>` (got {mp:?})",
                ),
            }
            if route.model.trim().is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    format!("model_routes[{i}].model"),
                    "model_routes[{i}].model must not be empty"
                );
            }
        }

        // Embedding routes
        for (i, route) in self.embedding_routes.iter().enumerate() {
            if route.hint.trim().is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    format!("embedding_routes[{i}].hint"),
                    "embedding_routes[{i}].hint must not be empty"
                );
            }
            let mp = route.model_provider.trim();
            if mp.is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    format!("embedding_routes[{i}].model_provider"),
                    "embedding_routes[{i}].model_provider must not be empty"
                );
            }
            // Embedding routes resolve against the same model-provider map;
            // there is no separate `providers.embeddings` typed section.
            match mp.split_once('.') {
                Some((ty, inner)) if !ty.is_empty() && !inner.is_empty() => {
                    if self.providers.models.find(ty, inner).is_none() {
                        validation_bail!(
                            DanglingReference,
                            format!("embedding_routes[{i}].model_provider"),
                            "embedding_routes[{i}].model_provider = {mp:?} but providers.models.{ty}.{inner} is not configured",
                        );
                    }
                }
                _ => validation_bail!(
                    InvalidFormat,
                    format!("embedding_routes[{i}].model_provider"),
                    "embedding_routes[{i}].model_provider must be dotted form `<type>.<alias>` (got {mp:?})",
                ),
            }
            if route.model.trim().is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    format!("embedding_routes[{i}].model"),
                    "embedding_routes[{i}].model must not be empty"
                );
            }
        }

        for (type_key, alias_key, profile) in self.providers.models.iter_entries() {
            let profile_name = format!("{type_key}.{alias_key}");

            let has_uri = profile
                .uri
                .as_deref()
                .map(str::trim)
                .is_some_and(|value| !value.is_empty());

            // Entries created by migration from top-level fields use the
            // model_provider type+alias as the map key and may not have
            // explicit `uri` (the model_provider factory resolves the
            // family's default endpoint via `ModelEndpoint`). An entry
            // with no identifying information at all is almost always an
            // in-progress quickstart state — the user picked the model
            // provider but hasn't filled anything in yet. Warn but don't
            // bail; the runtime falls back to family-default endpoint at
            // use time, and a chat against the unconfigured model
            // provider fails with a clear error then.
            let has_api_key = profile
                .api_key
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty());
            let has_model = profile
                .model
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty());
            if !has_uri && !has_api_key && !has_model {
                ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"model_provider": profile_name, "profile_name": profile_name})), "providers.models. is empty (no uri / api_key / model). \
                     Skipping at runtime; run `zeroclaw quickstart` (or use the dashboard) \
                     to make this model_provider usable.");
                continue;
            }

            if let Some(uri) = profile.uri.as_deref().map(str::trim)
                && !uri.is_empty()
            {
                let parsed = reqwest::Url::parse(uri).with_context(|| {
                    format!("providers.models.{profile_name}.uri is not a valid URL")
                })?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    anyhow::bail!("providers.models.{profile_name}.uri must use http/https");
                }
            }

            if let Some(temp) = profile.temperature {
                validate_temperature(temp).map_err(|e| {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "profile": profile_name,
                                "temperature": temp,
                                "error": format!("{}", e),
                            })),
                        "providers.models.<alias>.temperature rejected"
                    );
                    anyhow::Error::msg(format!("providers.models.{profile_name}.temperature: {e}"))
                })?;
            }

            for (key, value) in &profile.pricing {
                if value.is_nan() {
                    anyhow::bail!(
                        "providers.models.{profile_name}.pricing.{key}: value must not be NaN"
                    );
                }
                if *value < 0.0 {
                    anyhow::bail!(
                        "providers.models.{profile_name}.pricing.{key}: value must be >= 0.0 (got {value})"
                    );
                }
            }
        }

        // Non-fatal validation warnings: surfaced both via tracing (CLI sees
        // on stderr) and via Config::collect_warnings (gateway HTTP returns
        // structured to dashboard callers). Single source of truth lives in
        // collect_warnings; emit each one to tracing here so the existing
        // log behavior is preserved.
        for w in self.collect_warnings() {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"path": w.path, "code": w.code})),
                &format!("{}", w.message)
            );
        }

        // Ollama cloud-routing safety checks
        for (alias, cfg) in &self.providers.models.ollama {
            let entry = &cfg.base;
            if !entry
                .model
                .as_deref()
                .is_some_and(|model| model.trim().ends_with(":cloud"))
            {
                continue;
            }

            if is_local_ollama_endpoint(entry.uri.as_deref()) {
                anyhow::bail!(
                    "providers.models.ollama.{alias}.model uses ':cloud', but uri is local or unset. Set uri to a remote Ollama endpoint (for example https://ollama.com)."
                );
            }
            if is_official_ollama_cloud_endpoint(entry.uri.as_deref())
                && !has_ollama_cloud_credential(entry.api_key.as_deref())
            {
                anyhow::bail!(
                    "providers.models.ollama.{alias}.model uses ':cloud', but no API key is configured. Set api_key on [providers.models.ollama.{alias}] (or via the schema-mirror grammar: ZEROCLAW_providers__models__ollama__{alias}__api_key=<value>)."
                );
            }
        }

        // Microsoft 365
        if self.microsoft365.enabled {
            let tenant = self
                .microsoft365
                .tenant_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            if tenant.is_none() {
                anyhow::bail!(
                    "microsoft365.tenant_id must not be empty when microsoft365 is enabled"
                );
            }
            let client = self
                .microsoft365
                .client_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            if client.is_none() {
                anyhow::bail!(
                    "microsoft365.client_id must not be empty when microsoft365 is enabled"
                );
            }
            let flow = self.microsoft365.auth_flow.trim();
            if flow != "client_credentials" && flow != "device_code" {
                anyhow::bail!(
                    "microsoft365.auth_flow must be 'client_credentials' or 'device_code'"
                );
            }
            if flow == "client_credentials"
                && self
                    .microsoft365
                    .client_secret
                    .as_deref()
                    .is_none_or(|s| s.trim().is_empty())
            {
                anyhow::bail!(
                    "microsoft365.client_secret must not be empty when auth_flow is 'client_credentials'"
                );
            }
        }

        // Microsoft 365
        if self.microsoft365.enabled {
            let tenant = self
                .microsoft365
                .tenant_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            if tenant.is_none() {
                anyhow::bail!(
                    "microsoft365.tenant_id must not be empty when microsoft365 is enabled"
                );
            }
            let client = self
                .microsoft365
                .client_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            if client.is_none() {
                anyhow::bail!(
                    "microsoft365.client_id must not be empty when microsoft365 is enabled"
                );
            }
            let flow = self.microsoft365.auth_flow.trim();
            if flow != "client_credentials" && flow != "device_code" {
                anyhow::bail!("microsoft365.auth_flow must be client_credentials or device_code");
            }
            if flow == "client_credentials"
                && self
                    .microsoft365
                    .client_secret
                    .as_deref()
                    .is_none_or(|s| s.trim().is_empty())
            {
                anyhow::bail!(
                    "microsoft365.client_secret must not be empty when auth_flow is client_credentials"
                );
            }
        }

        // MCP
        if self.mcp.enabled {
            validate_mcp_config(&self.mcp)?;
        }

        // Knowledge graph
        if self.knowledge.enabled {
            if self.knowledge.max_nodes == 0 {
                validation_bail!(
                    InvalidNumericRange,
                    "knowledge.max_nodes",
                    "knowledge.max_nodes must be greater than 0"
                );
            }
            if self.knowledge.db_path.trim().is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    "knowledge.db_path",
                    "knowledge.db_path must not be empty"
                );
            }
        }

        // Google Workspace allowed_services validation
        let mut seen_gws_services = std::collections::HashSet::new();
        for (i, service) in self.google_workspace.allowed_services.iter().enumerate() {
            let normalized = service.trim();
            if normalized.is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    format!("google_workspace.allowed_services[{i}]"),
                    "google_workspace.allowed_services[{i}] must not be empty"
                );
            }
            if !normalized
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
            {
                anyhow::bail!(
                    "google_workspace.allowed_services[{i}] contains invalid characters: {normalized}"
                );
            }
            if !seen_gws_services.insert(normalized.to_string()) {
                anyhow::bail!(
                    "google_workspace.allowed_services contains duplicate entry: {normalized}"
                );
            }
        }

        // Build the effective allowed-services set for cross-validation.
        // When the operator leaves allowed_services empty the tool falls back to
        // DEFAULT_GWS_SERVICES; use the same constant here so validation is
        // consistent in both cases.
        let effective_services: std::collections::HashSet<&str> =
            if self.google_workspace.allowed_services.is_empty() {
                DEFAULT_GWS_SERVICES.iter().copied().collect()
            } else {
                self.google_workspace
                    .allowed_services
                    .iter()
                    .map(|s| s.trim())
                    .collect()
            };

        let mut seen_gws_operations = std::collections::HashSet::new();
        for (i, operation) in self.google_workspace.allowed_operations.iter().enumerate() {
            let service = operation.service.trim();
            let resource = operation.resource.trim();

            if service.is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    format!("google_workspace.allowed_operations[{i}].service"),
                    "google_workspace.allowed_operations[{i}].service must not be empty"
                );
            }
            if resource.is_empty() {
                anyhow::bail!(
                    "google_workspace.allowed_operations[{i}].resource must not be empty"
                );
            }

            if !effective_services.contains(service) {
                anyhow::bail!(
                    "google_workspace.allowed_operations[{i}].service '{service}' is not in the \
                     effective allowed_services; this entry can never match at runtime"
                );
            }
            if !service
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
            {
                anyhow::bail!(
                    "google_workspace.allowed_operations[{i}].service contains invalid characters: {service}"
                );
            }
            if !resource
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
            {
                anyhow::bail!(
                    "google_workspace.allowed_operations[{i}].resource contains invalid characters: {resource}"
                );
            }

            if let Some(ref sub_resource) = operation.sub_resource {
                let sub = sub_resource.trim();
                if sub.is_empty() {
                    anyhow::bail!(
                        "google_workspace.allowed_operations[{i}].sub_resource must not be empty when present"
                    );
                }
                if !sub
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
                {
                    anyhow::bail!(
                        "google_workspace.allowed_operations[{i}].sub_resource contains invalid characters: {sub}"
                    );
                }
            }

            if operation.methods.is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    format!("google_workspace.allowed_operations[{i}].methods"),
                    "google_workspace.allowed_operations[{i}].methods must not be empty"
                );
            }

            let mut seen_methods = std::collections::HashSet::new();
            for (j, method) in operation.methods.iter().enumerate() {
                let normalized = method.trim();
                if normalized.is_empty() {
                    anyhow::bail!(
                        "google_workspace.allowed_operations[{i}].methods[{j}] must not be empty"
                    );
                }
                if !normalized
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
                {
                    anyhow::bail!(
                        "google_workspace.allowed_operations[{i}].methods[{j}] contains invalid characters: {normalized}"
                    );
                }
                if !seen_methods.insert(normalized.to_string()) {
                    anyhow::bail!(
                        "google_workspace.allowed_operations[{i}].methods contains duplicate entry: {normalized}"
                    );
                }
            }

            let sub_key = operation
                .sub_resource
                .as_deref()
                .map(str::trim)
                .unwrap_or("");
            let operation_key = format!("{service}:{resource}:{sub_key}");
            if !seen_gws_operations.insert(operation_key.clone()) {
                anyhow::bail!(
                    "google_workspace.allowed_operations contains duplicate service/resource/sub_resource entry: {operation_key}"
                );
            }
        }

        // Project intelligence
        if self.project_intel.enabled {
            let lang = &self.project_intel.default_language;
            if !["en", "de", "fr", "it"].contains(&lang.as_str()) {
                anyhow::bail!(
                    "project_intel.default_language must be one of: en, de, fr, it (got '{lang}')"
                );
            }
            let sens = &self.project_intel.risk_sensitivity;
            if !["low", "medium", "high"].contains(&sens.as_str()) {
                anyhow::bail!(
                    "project_intel.risk_sensitivity must be one of: low, medium, high (got '{sens}')"
                );
            }
            if let Some(ref tpl_dir) = self.project_intel.templates_dir
                && !std::path::Path::new(tpl_dir).exists()
            {
                anyhow::bail!("project_intel.templates_dir path does not exist: {tpl_dir}");
            }
        }

        // Proxy (delegate to existing validation)
        self.proxy.validate()?;
        self.cloud_ops.validate()?;

        // Skills — extra registries
        {
            let mut seen = std::collections::HashSet::new();
            for (i, reg) in self.skills.extra_registries.iter().enumerate() {
                if reg.name.trim().is_empty() {
                    anyhow::bail!("skills.extra_registries[{i}].name must not be empty");
                }
                if !ExternalRegistry::is_valid_name(&reg.name) {
                    anyhow::bail!(
                        "skills.extra_registries[{i}].name '{}' is invalid; use only lowercase ASCII letters, numbers, '-' or '_' so it can be addressed as registry:<name>/<skill>",
                        reg.name
                    );
                }
                if !seen.insert(reg.name.as_str()) {
                    anyhow::bail!("skills.extra_registries has duplicate name '{}'", reg.name);
                }
                if reg.url.trim().is_empty() {
                    anyhow::bail!(
                        "skills.extra_registries[{}].url must not be empty",
                        reg.name
                    );
                }
                if reg.kind != ExternalRegistryKind::Git {
                    anyhow::bail!(
                        "skills.extra_registries[{}].kind must be 'git' (got '{}'); other protocols are not yet supported",
                        reg.name,
                        reg.kind
                    );
                }
                match reqwest::Url::parse(&reg.url) {
                    Ok(u) if matches!(u.scheme(), "http" | "https" | "file") => {}
                    Ok(u) => anyhow::bail!(
                        "skills.extra_registries[{}].url scheme '{}' is unsupported (use http, https, or file)",
                        reg.name,
                        u.scheme()
                    ),
                    Err(e) => anyhow::bail!(
                        "skills.extra_registries[{}].url is not a valid URL: {e}",
                        reg.name
                    ),
                }
            }
        }

        // Notion
        if self.notion.enabled {
            if self.notion.database_id.trim().is_empty() {
                anyhow::bail!("notion.database_id must not be empty when notion.enabled = true");
            }
            if self.notion.poll_interval_secs == 0 {
                validation_bail!(
                    InvalidNumericRange,
                    "notion.poll_interval_secs",
                    "notion.poll_interval_secs must be greater than 0"
                );
            }
            if self.notion.max_concurrent == 0 {
                validation_bail!(
                    InvalidNumericRange,
                    "notion.max_concurrent",
                    "notion.max_concurrent must be greater than 0"
                );
            }
            if self.notion.status_property.trim().is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    "notion.status_property",
                    "notion.status_property must not be empty"
                );
            }
            if self.notion.input_property.trim().is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    "notion.input_property",
                    "notion.input_property must not be empty"
                );
            }
            if self.notion.result_property.trim().is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    "notion.result_property",
                    "notion.result_property must not be empty"
                );
            }
        }

        // Pinggy tunnel region — validate allowed values (case-insensitive, auto-lowercased at runtime).
        if let Some(ref pinggy) = self.tunnel.pinggy
            && let Some(ref region) = pinggy.region
        {
            let r = region.trim().to_ascii_lowercase();
            if !r.is_empty() && !matches!(r.as_str(), "us" | "eu" | "ap" | "br" | "au") {
                anyhow::bail!(
                    "tunnel.pinggy.region must be one of: us, eu, ap, br, au (or omitted for auto)"
                );
            }
        }

        // Jira
        if self.jira.enabled {
            if self.jira.base_url.trim().is_empty() {
                anyhow::bail!("jira.base_url must not be empty when jira.enabled = true");
            }
            if self.jira.api_token.trim().is_empty()
                && std::env::var("JIRA_API_TOKEN")
                    .unwrap_or_default()
                    .trim()
                    .is_empty()
            {
                anyhow::bail!(
                    "jira.api_token must be set (or JIRA_API_TOKEN env var) when jira.enabled = true"
                );
            }
            let valid_actions = [
                "get_ticket",
                "search_tickets",
                "comment_ticket",
                "list_projects",
                "myself",
                "list_transitions",
                "transition_ticket",
                "create_ticket",
            ];
            for action in &self.jira.allowed_actions {
                if !valid_actions.contains(&action.as_str()) {
                    anyhow::bail!(
                        "jira.allowed_actions contains unknown action: '{}'. \
                         Valid: get_ticket, search_tickets, comment_ticket, list_projects, myself, list_transitions, transition_ticket, create_ticket",
                        action
                    );
                }
            }
        }

        // Nevis IAM — delegate to NevisConfig::validate() for field-level checks
        if let Err(msg) = self.security.nevis.validate() {
            anyhow::bail!("security.nevis: {msg}");
        }

        // Per-profile validation: the context-compression summarizer provider
        // ref must resolve to a configured `[providers.models.*]` alias.
        // Empty = inherit (valid). A shared profile fails loud at config time
        // instead of only when some agent using it compresses.
        let mut profile_aliases: Vec<&String> = self.runtime_profiles.keys().collect();
        profile_aliases.sort();
        for palias in profile_aliases {
            let value = self.runtime_profiles[palias]
                .context_compression
                .summary_provider
                .trim();
            if value.is_empty() {
                continue;
            }
            match value.split_once('.') {
                Some((ty, inner)) if !ty.is_empty() && !inner.is_empty() => {
                    let exists = self
                        .get_map_keys(&format!("providers.models.{ty}"))
                        .is_some_and(|keys| keys.iter().any(|k| k == inner));
                    if !exists {
                        validation_bail!(
                            DanglingReference,
                            format!(
                                "runtime_profiles.{palias}.context_compression.summary_provider"
                            ),
                            "runtime_profiles.{palias}.context_compression.summary_provider = {value:?} but providers.models.{ty}.{inner} is not configured",
                        );
                    }
                }
                _ => validation_bail!(
                    InvalidFormat,
                    format!("runtime_profiles.{palias}.context_compression.summary_provider"),
                    "runtime_profiles.{palias}.context_compression.summary_provider must be dotted form `<type>.<alias>` (got {value:?})",
                ),
            }
        }

        // Per-agent validation. Mandatory + alias-existence checks live
        // here so the gateway PATCH path returns structured per-field
        // errors and the frontend never owns this rule. Sorted iteration
        // keeps error ordering stable across runs.
        let mut agent_aliases: Vec<&String> = self.agents.keys().collect();
        agent_aliases.sort();
        for alias in agent_aliases {
            let agent = &self.agents[alias];

            // model_provider: mandatory, dotted `<type>.<inner>` ref into
            // model_providers.<type>.<inner>.
            let mp = agent.model_provider.trim();
            if mp.is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    format!("agents.{alias}.model_provider"),
                    "agents.{alias}.model_provider must reference a configured model model_provider (e.g. \"anthropic.default\")",
                );
            }
            match mp.split_once('.') {
                Some((ty, inner)) if !ty.is_empty() && !inner.is_empty() => {
                    if !crate::providers::ModelProviders::slot_names().contains(&ty) {
                        validation_bail!(
                            DanglingReference,
                            format!("agents.{alias}.model_provider"),
                            "agents.{alias}.model_provider = {mp:?} but {ty:?} is not a known provider family; check [providers.models.<family>.<alias>] in config.toml (valid families: `zeroclaw providers`)",
                        );
                    }
                    let exists = self
                        .get_map_keys(&format!("providers.models.{ty}"))
                        .is_some_and(|keys| keys.iter().any(|k| k == inner));
                    if !exists {
                        validation_bail!(
                            DanglingReference,
                            format!("agents.{alias}.model_provider"),
                            "agents.{alias}.model_provider = {mp:?} but [providers.models.{ty}.{inner}] is not configured",
                        );
                    }
                }
                _ => validation_bail!(
                    InvalidFormat,
                    format!("agents.{alias}.model_provider"),
                    "agents.{alias}.model_provider must be dotted form `<type>.<alias>` (got {mp:?})",
                ),
            }

            // channels: each entry is a dotted `<type>.<inner>` ref into
            // channels.<type>.<inner>. Empty list is valid (delegate-only agent).
            // Uses the schema-derived `get_map_keys` so new channel types
            // surface here automatically — no per-type match arm.
            for (i, ch) in agent.channels.iter().enumerate() {
                let trimmed = ch.trim();
                match trimmed.split_once('.') {
                    Some((ty, inner)) if !ty.is_empty() && !inner.is_empty() => {
                        // `get_map_keys` stores section names using the raw
                        // field ident (snake), the same dotted form the
                        // operator sees in TOML (`gmail_push`, `voice_call`,
                        // `nextcloud_talk`). Look up verbatim.
                        let exists = self
                            .get_map_keys(&format!("channels.{ty}"))
                            .is_some_and(|keys| keys.iter().any(|k| k == inner));
                        if !exists {
                            validation_bail!(
                                DanglingReference,
                                format!("agents.{alias}.channels[{i}]"),
                                "agents.{alias}.channels[{i}] = {trimmed:?} but channels.{ty}.{inner} is not configured",
                            );
                        }
                    }
                    _ => validation_bail!(
                        InvalidFormat,
                        format!("agents.{alias}.channels[{i}]"),
                        "agents.{alias}.channels[{i}] must be dotted form `<type>.<alias>` (got {trimmed:?})",
                    ),
                }
            }

            // Per-agent provider refs that resolve into the typed provider
            // sections. Empty = no preference for that category (no TTS / no
            // STT for this agent), which is valid. Non-empty values must
            // match a configured `[providers.<category>.<type>.<alias>]`
            // entry, fail loud with the dangling ref otherwise.
            // there is no global default-X-provider concept — every consumer
            // either picks a configured alias or opts out entirely.
            let typed_provider_refs: &[(&str, &str, &str)] = &[
                ("providers.tts", "tts_provider", agent.tts_provider.trim()),
                (
                    "providers.transcription",
                    "transcription_provider",
                    agent.transcription_provider.trim(),
                ),
                // New field:
                (
                    "providers.models",
                    "classifier_provider",
                    agent.classifier_provider.trim(),
                ),
                // Agent-level context-compression summarizer override.
                (
                    "providers.models",
                    "summary_provider",
                    agent.summary_provider.trim(),
                ),
            ];
            for (section_prefix, field, value) in typed_provider_refs {
                if value.is_empty() {
                    continue;
                }
                match value.split_once('.') {
                    Some((ty, inner)) if !ty.is_empty() && !inner.is_empty() => {
                        let exists = self
                            .get_map_keys(&format!("{section_prefix}.{ty}"))
                            .is_some_and(|keys| keys.iter().any(|k| k == inner));
                        if !exists {
                            validation_bail!(
                                DanglingReference,
                                format!("agents.{alias}.{field}"),
                                "agents.{alias}.{field} = {value:?} but {section_prefix}.{ty}.{inner} is not configured",
                            );
                        }
                    }
                    _ => validation_bail!(
                        InvalidFormat,
                        format!("agents.{alias}.{field}"),
                        "agents.{alias}.{field} must be dotted form `<type>.<alias>` (got {value:?})",
                    ),
                }
            }

            // Bare-alias bundle refs. Tuple is (kebab section path, kebab
            // agent field name, value list). Both names use the schema's
            // kebab form: section name matches what `get_map_keys` expects
            // (macro converts snake→kebab via `snake_to_kebab` per
            // crates/zeroclaw-macros/src/lib.rs:1056); field name matches
            // what `prop_fields()` emits, so DanglingReference paths bind
            // directly to the right inline error in the dashboard form.
            let bare_multi: &[(&str, &str, &[String])] = &[
                ("skill_bundles", "skill_bundles", &agent.skill_bundles),
                (
                    "knowledge_bundles",
                    "knowledge_bundles",
                    &agent.knowledge_bundles,
                ),
                ("mcp_bundles", "mcp_bundles", &agent.mcp_bundles),
            ];
            for (section, field, values) in bare_multi {
                for (i, key) in values.iter().enumerate() {
                    let trimmed = key.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let exists = self
                        .get_map_keys(section)
                        .is_some_and(|keys| keys.iter().any(|k| k == trimmed));
                    if !exists {
                        validation_bail!(
                            DanglingReference,
                            format!("agents.{alias}.{field}[{i}]"),
                            "agents.{alias}.{field}[{i}] = {trimmed:?} but {section}.{trimmed} is not configured",
                        );
                    }
                }
            }
            let bare_single: &[(&str, &str, &str)] = &[
                ("risk_profiles", "risk_profile", agent.risk_profile.as_str()),
                (
                    "runtime_profiles",
                    "runtime_profile",
                    agent.runtime_profile.as_str(),
                ),
            ];
            for (section, field, raw) in bare_single {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let exists = self
                    .get_map_keys(section)
                    .is_some_and(|keys| keys.iter().any(|k| k == trimmed));
                if !exists {
                    validation_bail!(
                        DanglingReference,
                        format!("agents.{alias}.{field}"),
                        "agents.{alias}.{field} = {trimmed:?} but {section}.{trimmed} is not configured",
                    );
                }
            }

            // risk_profile is mandatory for enabled agents — there is no
            // global fallback, so an enabled agent with no profile can't
            // gate its actions. Run this check last so the more specific
            // dangling/format errors above surface first.
            //
            // A carded agent satisfies this through its card, which carries a
            // profile of its own. The invariant is unchanged — every enabled
            // agent still has exactly one profile — only where it is written
            // moves.
            if agent.enabled && agent.risk_profile.trim().is_empty() {
                let card = agent.card.as_str().trim();
                let carded_profile = self
                    .cards
                    .get(card)
                    .map(|card| card.risk_profile.as_str().trim())
                    .filter(|profile| !profile.is_empty());
                if let Some(carded_profile) = carded_profile {
                    // A card profile is still only a reference; accepting a
                    // dangling alias makes the agent look gated while no
                    // policy exists to enforce at runtime.
                    if !self.risk_profiles.contains_key(carded_profile) {
                        validation_bail!(
                            DanglingReference,
                            format!("cards.{card}.risk_profile"),
                            "cards.{card}.risk_profile = {carded_profile:?} but no [risk_profiles.{carded_profile}] entry is configured",
                        );
                    }
                } else {
                    validation_bail!(
                        RequiredFieldEmpty,
                        format!("agents.{alias}.risk_profile"),
                        "agents.{alias}.risk_profile must reference a configured [risk_profiles.<alias>] entry",
                    );
                }
            }

            // workspace.access: keys must point at OTHER agents, never
            // self, and every target must be a configured agent.
            for (target, mode) in &agent.workspace.access {
                let target_str = target.as_str();
                if target_str == alias.as_str() {
                    validation_bail!(
                        InvalidFormat,
                        format!("agents.{alias}.workspace.access.{target_str}"),
                        "agents.{alias}.workspace.access.{target_str} = {mode:?} but {target_str} is this agent itself; an agent always has full access to its own workspace, so self-references in the cross-agent allowlist are not permitted",
                    );
                }
                if !self.agents.contains_key(target_str) {
                    validation_bail!(
                        DanglingReference,
                        format!("agents.{alias}.workspace.access.{target_str}"),
                        "agents.{alias}.workspace.access.{target_str} = {mode:?} but agents.{target_str} is not configured",
                    );
                }
            }

            // workspace.read_memory_from: every alias must exist as a
            // configured agent and must use the same MemoryBackendKind
            // as the declaring agent. Mismatched backends fail at
            // config load rather than producing a runtime error when
            // the per-agent memory plumbing consumes the allowlist.
            let agent_backend = agent.memory.backend;
            for (i, target) in agent.workspace.read_memory_from.iter().enumerate() {
                let target_str = target.as_str();
                if target_str == alias.as_str() {
                    validation_bail!(
                        InvalidFormat,
                        format!("agents.{alias}.workspace.read_memory_from[{i}]"),
                        "agents.{alias}.workspace.read_memory_from[{i}] = {target_str:?} but {target_str} is this agent itself; an agent always sees its own memory rows, so self-references in the cross-agent allowlist are not permitted",
                    );
                }
                let Some(target_agent) = self.agents.get(target_str) else {
                    validation_bail!(
                        DanglingReference,
                        format!("agents.{alias}.workspace.read_memory_from[{i}]"),
                        "agents.{alias}.workspace.read_memory_from[{i}] = {target_str:?} but agents.{target_str} is not configured",
                    );
                };
                if target_agent.memory.backend != agent_backend {
                    let target_backend = target_agent.memory.backend;
                    validation_bail!(
                        InvalidFormat,
                        format!("agents.{alias}.workspace.read_memory_from[{i}]"),
                        "agents.{alias}.workspace.read_memory_from[{i}] points at agents.{target_str} which uses memory backend {target_backend:?}, but agents.{alias} uses {agent_backend:?}; the allowlist must point at same-backend siblings only",
                    );
                }
            }
        }

        // Peer groups: every member alias must exist as a configured
        // agent, and the group's channel must be in each member's
        // channels list. Mutual opt-in resolution happens at runtime;
        // this cross-reference check keeps misconfigured group
        // members from looking like real peer relationships at load
        // time.
        let mut peer_group_names: Vec<&String> = self.peer_groups.keys().collect();
        peer_group_names.sort();
        for group_name in peer_group_names {
            let group = &self.peer_groups[group_name];
            let group_channel = group.channel.trim();
            if group_channel.is_empty() {
                validation_bail!(
                    RequiredFieldEmpty,
                    format!("peer_groups.{group_name}.channel"),
                    "peer_groups.{group_name}.channel must name a channel type (e.g. \"discord\") or dotted alias (e.g. \"discord.work\")",
                );
            }
            // `get_map_keys` stores section names using the raw field ident
            // (snake); look up the channel type verbatim.
            let (group_channel_type, group_channel_alias) = match group_channel.split_once('.') {
                Some((ty, al)) => (ty, Some(al)),
                None => (group_channel, None),
            };
            let channel_aliases = self.get_map_keys(&format!("channels.{group_channel_type}"));
            if channel_aliases.is_none() {
                validation_bail!(
                    DanglingReference,
                    format!("peer_groups.{group_name}.channel"),
                    "peer_groups.{group_name}.channel = {group_channel:?} but no [channels.{group_channel_type}.*] block is configured",
                );
            }
            if let Some(alias) = group_channel_alias {
                let exists = channel_aliases
                    .as_ref()
                    .is_some_and(|keys| keys.iter().any(|k| k == alias));
                if !exists {
                    validation_bail!(
                        DanglingReference,
                        format!("peer_groups.{group_name}.channel"),
                        "peer_groups.{group_name}.channel = {group_channel:?} but [channels.{group_channel_type}.{alias}] is not configured",
                    );
                }
            }
            for (i, member) in group.agents.iter().enumerate() {
                let member_str = member.as_str();
                let Some(member_agent) = self.agents.get(member_str) else {
                    validation_bail!(
                        DanglingReference,
                        format!("peer_groups.{group_name}.agents[{i}]"),
                        "peer_groups.{group_name}.agents[{i}] = {member_str:?} but agents.{member_str} is not configured",
                    );
                };
                let has_channel_match = member_agent.channels.iter().any(|ch| {
                    let ch_str = ch.as_str();
                    match group_channel_alias {
                        Some(alias) => ch_str == format!("{group_channel_type}.{alias}"),
                        None => ch_str.starts_with(&format!("{group_channel_type}.")),
                    }
                });
                if !has_channel_match {
                    let needs_msg = match group_channel_alias {
                        Some(alias) => format!("entry for {group_channel_type}.{alias}"),
                        None => format!("entry of type {group_channel_type:?}"),
                    };
                    validation_bail!(
                        InvalidFormat,
                        format!("peer_groups.{group_name}.agents[{i}]"),
                        "peer_groups.{group_name}.agents[{i}] = {member_str:?} but agents.{member_str}.channels has no {needs_msg}",
                    );
                }
            }
        }

        if self.plugins.limits.call_fuel == 0 {
            validation_bail!(
                InvalidNumericRange,
                "plugins.limits.call_fuel",
                "plugins.limits.call_fuel must be greater than 0; a zero budget traps every plugin call before it runs"
            );
        }
        if self.plugins.limits.max_memory_mb == 0 {
            validation_bail!(
                InvalidNumericRange,
                "plugins.limits.max_memory_mb",
                "plugins.limits.max_memory_mb must be greater than 0; a zero cap rejects every plugin at instantiation"
            );
        }
        if self.plugins.limits.max_table_elements == 0 {
            validation_bail!(
                InvalidNumericRange,
                "plugins.limits.max_table_elements",
                "plugins.limits.max_table_elements must be greater than 0; a zero ceiling rejects every plugin that allocates a table"
            );
        }
        if self.plugins.limits.max_instances == 0 {
            validation_bail!(
                InvalidNumericRange,
                "plugins.limits.max_instances",
                "plugins.limits.max_instances must be greater than 0; a zero ceiling rejects every plugin at instantiation"
            );
        }

        Ok(())
    }

    pub fn mark_dirty(&mut self, path: &str) {
        self.dirty_paths.insert(path.to_string());
    }

    /// Auto-create the parent map key for a dotted set-prop `path` when it does
    /// not yet exist, so a `set_prop` on a brand-new alias's field materializes
    /// the entry first.
    ///
    /// Returns `true` iff it REFUSED to create the reserved `default` agent: a
    /// set-prop surface should then surface a reserved-name error rather than the
    /// generic "not configured" that the still-missing entry would otherwise
    /// produce. Returns `false` in every other case (created, already existed, or
    /// the path is not a map-keyed entry). The bool is advisory; statement-callers
    /// that do not distinguish the reserved case may ignore it.
    ///
    /// `#[resource_key]` sections are excluded. Their keys are values drawn from
    /// another domain (model id, voice, tool name) and may themselves contain
    /// dots, so the first-dot split below would read
    /// `cost.rates.providers.models.openai.gpt-4.1.input_per_mtok` as the bogus
    /// alias `gpt-4` plus a nonsense tail, and `create_map_key` skips
    /// `validate_alias_key` for those sections so nothing else rejects it.
    /// Rate rows are created explicitly through `POST /api/config/map-key`
    /// instead.
    pub fn ensure_map_key_for_path(&mut self, path: &str) -> bool {
        use crate::traits::MapKeyKind;
        let mut best: Option<&'static str> = None;
        for s in Self::map_key_sections()
            .iter()
            .filter(|s| s.kind == MapKeyKind::Map)
            .filter(|s| !s.resource_key)
        {
            let prefix = format!("{}.", s.path);
            if path.starts_with(&prefix)
                && path.len() > prefix.len()
                && best.is_none_or(|b| s.path.len() > b.len())
            {
                best = Some(s.path);
            }
        }
        let Some(section) = best else {
            return false;
        };
        let rest = &path[section.len() + 1..];
        let Some(alias) = rest.split('.').next().filter(|a| !a.is_empty()) else {
            return false;
        };
        if self
            .get_map_keys(section)
            .is_some_and(|keys| keys.iter().any(|k| k == alias))
        {
            return false;
        }
        // Never auto-vivify the reserved `default` agent from a set-prop path: a
        // prop write under a nonexistent `agents.default` must not materialize the
        // reserved runtime fallback (the operator would get an agent the rename
        // guard then traps). This is the set-prop analogue of the create guard in
        // `create_map_key_checked`. The migration that legitimately synthesizes
        // `agents.default` calls `create_map_key` directly, not this path, and an
        // already-present `default` is left untouched by the existence check above.
        if section == "agents" && crate::alias_refs::is_reserved_agent_alias(alias) {
            // Make the refusal observable: callers that check the return surface a
            // reserved-name error, and this WARN with a stable `error_key` lets an
            // operator see why a set-prop on a fresh `agents.default` was dropped.
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "error_key": "config.reserved_agent_vivify_refused",
                        "alias": alias,
                        "path": path,
                    })),
                "refused to auto-create the reserved `default` agent from a set-prop path"
            );
            return true;
        }
        let _ = self.create_map_key(section, alias);
        false
    }

    pub fn clear_dirty(&mut self) {
        self.dirty_paths.clear();
    }

    pub fn set_prop_persistent(&mut self, name: &str, value_str: &str) -> Result<()> {
        self.set_prop(name, value_str)?;
        self.mark_dirty(name);
        Ok(())
    }

    pub fn set_secret_persistent(&mut self, name: &str, value: String) -> Result<()> {
        self.set_secret(name, value)?;
        self.mark_dirty(name);
        Ok(())
    }

    async fn resolve_config_path_for_save(&self) -> Result<PathBuf> {
        if self
            .config_path
            .parent()
            .is_some_and(|parent| !parent.as_os_str().is_empty())
        {
            return Ok(self.config_path.clone());
        }

        let (default_zeroclaw_dir, default_workspace_dir) = default_config_and_data_dirs()?;
        let (zeroclaw_dir, _workspace_dir, source) =
            resolve_runtime_config_dirs(&default_zeroclaw_dir, &default_workspace_dir).await?;
        let file_name = self
            .config_path
            .file_name()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| std::ffi::OsStr::new("config.toml"));
        let resolved = zeroclaw_dir.join(file_name);
        ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"path": self.config_path.display().to_string(), "resolved": resolved.display().to_string(), "source": source.as_str()})), "Config path missing parent directory; resolving from runtime environment");
        Ok(resolved)
    }

    /// Full save. Refuses to overwrite an existing config file unless this
    /// value was loaded from that exact file (`loaded_from`), so a default,
    /// programmatically built, or repointed `Config` cannot replace an
    /// operator's config with a near-empty snapshot. Creating a
    /// missing file (first run) always succeeds — a value that never read
    /// a file gets exactly one create and must then reload or use
    /// `force_save()`; `force_save()` is the explicit overwrite path.
    pub async fn save(&self) -> Result<()> {
        self.save_impl(false).await.map(|_| ())
    }

    /// Same as `save()`, but skips the loaded-provenance guard. Only for
    /// callers that verifiably intend to replace an existing file with a
    /// `Config` that never read it.
    pub async fn force_save(&self) -> Result<()> {
        self.save_impl(true).await.map(|_| ())
    }

    async fn save_impl(&self, force: bool) -> Result<PathBuf> {
        // Encrypt secrets before serialization
        let mut config_to_save = self.clone();
        // Stamp the current schema version on every write. The in-memory
        // config is always at `CURRENT_SCHEMA_VERSION` (load-time migration
        // brings it forward), but pin it explicitly so a full save can never
        // emit a body-newer-than-label file. See `save_dirty` and.
        config_to_save.schema_version = crate::migration::CURRENT_SCHEMA_VERSION;
        let config_path = self.resolve_config_path_for_save().await?;
        // Fail closed: a metadata error must not read as "file absent" and
        // slip an unproven save past the guard.
        let target_exists = config_path.try_exists().with_context(|| {
            format!(
                "Cannot check config target {}; refusing to save",
                config_path.display()
            )
        })?;
        if !force && target_exists && self.loaded_from.as_ref() != Some(&config_path) {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "error_key": "config.save_refused_unproven_overwrite",
                        "path": config_path.display().to_string(),
                    })),
                "refusing to overwrite an existing config this Config was never loaded from"
            );
            anyhow::bail!(
                "Refusing to overwrite existing config at {}: this Config was \
                 not loaded from that file (default-constructed, built \
                 programmatically, or loaded from a different file), so saving \
                 would replace the operator's file with a near-empty snapshot. \
                 Load it first (e.g. `Config::load_or_init`) or call \
                 `force_save()` to overwrite deliberately.",
                config_path.display()
            );
        }
        let zeroclaw_dir = config_path
            .parent()
            .context("Config path must have a parent directory")?;
        let store = crate::secrets::SecretStore::new(zeroclaw_dir, self.secrets.encrypt);

        // Restore env-overridden paths to their pre-override snapshots before
        // encryption, so values supplied via `ZEROCLAW_*` env vars never reach
        // disk. Snapshots were captured at apply time from the post-decrypt
        // in-memory state, so secrets carry the original plaintext that
        // `encrypt_secrets()` will re-encrypt to fresh ciphertext that
        // decrypts back to the same value.
        if !self.pre_override_snapshots.is_empty() {
            crate::env_overrides::mask_env_overrides_for_save(
                &mut config_to_save,
                &self.pre_override_snapshots,
            )?;
        }
        restore_onepassword_references_for_save(
            &mut config_to_save,
            &self.onepassword_reference_snapshots,
            &self.dirty_paths,
        )?;

        // Encrypt all #[secret]-annotated fields via Configurable derive
        config_to_save.encrypt_secrets(&store)?;

        // Serialize, then prune fields whose values match
        // `Config::default()` so the on-disk config carries only the
        // operator's actual choices (no hundreds of lines of struct
        // defaults the operator never touched). The schema's
        // `#[serde(default = "...")]` annotations re-supply the
        // defaults on load, so the pruned file round-trips identically.
        let mut new_table: toml::Table = toml::Value::try_from(&config_to_save)
            .context("Failed to serialize config to TOML value")?
            .try_into()
            .context("Serialized config is not a TOML table")?;
        let default_table: toml::Table = toml::Value::try_from(Config::default())
            .ok()
            .and_then(|v| v.try_into().ok())
            .unwrap_or_default();
        prune_default_values(&mut new_table, &default_table);
        let new_toml = ensure_blank_line_before_sections(
            &toml::to_string_pretty(&new_table).context("Failed to serialize pruned config")?,
        );

        // If an existing config file is present, sync the new values onto it
        // to preserve comments and formatting. Otherwise, use the fresh serialization.
        let toml_str = if config_path.exists() {
            let existing = fs::read_to_string(&config_path).await.unwrap_or_default();
            if existing.is_empty() {
                new_toml
            } else {
                let mut doc: toml_edit::DocumentMut = existing
                    .parse()
                    .context("Failed to parse existing config for comment preservation")?;
                crate::migration::sync_table(doc.as_table_mut(), &new_table);
                // sync_table preserves existing decor verbatim, so newly
                // inserted sections lack the blank-line gap before their
                // header until the post-processor runs.
                ensure_blank_line_before_sections(&doc.to_string())
            }
        } else {
            new_toml
        };

        write_config_atomically(&config_path, &toml_str).await?;
        // Report the path actually written so `save_dirty`'s create
        // fallback can record provenance without re-resolving (the
        // resolver re-reads the environment for parentless paths).
        Ok(config_path)
    }

    /// Incremental save: only the paths in `self.dirty_paths` are written
    /// against the existing on-disk file. Non-dirty entries (including
    /// secret ciphertext) are left untouched; dirty paths whose value
    /// equals the schema default are removed from the doc instead of
    /// written. Falls back to a full `save()` when the file doesn't
    /// exist yet. Clears the dirty set on success.
    pub async fn save_dirty(&mut self) -> Result<()> {
        if self.dirty_paths.is_empty() {
            return Ok(());
        }

        let config_path = self.resolve_config_path_for_save().await?;
        if !config_path.exists() {
            return match self.save_impl(false).await {
                Ok(written) => {
                    // This value just created the file; `written` is the
                    // path the full save actually wrote.
                    self.loaded_from = Some(written);
                    self.clear_dirty();
                    Ok(())
                }
                Err(e) => Err(e),
            };
        }

        let mut config_to_save = self.clone();
        let zeroclaw_dir = config_path
            .parent()
            .context("Config path must have a parent directory")?;
        let store = crate::secrets::SecretStore::new(zeroclaw_dir, self.secrets.encrypt);

        if !self.pre_override_snapshots.is_empty() {
            crate::env_overrides::mask_env_overrides_for_save(
                &mut config_to_save,
                &self.pre_override_snapshots,
            )?;
        }
        restore_onepassword_references_for_save(
            &mut config_to_save,
            &self.onepassword_reference_snapshots,
            &self.dirty_paths,
        )?;
        config_to_save.encrypt_secrets(&store)?;

        let full_table: toml::Table = toml::Value::try_from(&config_to_save)
            .context("Failed to serialize config to TOML value")?
            .try_into()
            .context("Serialized config is not a TOML table")?;
        let default_table: toml::Table = toml::Value::try_from(Config::default())
            .ok()
            .and_then(|v| v.try_into().ok())
            .unwrap_or_default();

        let existing = fs::read_to_string(&config_path).await.with_context(|| {
            format!(
                "Failed to read existing config for incremental save: {}",
                config_path.display()
            )
        })?;
        let mut doc: toml_edit::DocumentMut = existing
            .parse()
            .context("Failed to parse existing config for incremental save")?;

        for path in &self.dirty_paths {
            apply_dirty_path(doc.as_table_mut(), path, &full_table, &default_table)?;
        }

        // Stamp the current schema version. An incremental save writes
        // current-schema-shaped sections (e.g. the dashboard saving a single
        // `agents.<name>.model_provider`) but `schema_version` is never a
        // dirty path, so without this it keeps whatever an older binary first
        // wrote on disk. The resulting body-newer-than-label file then crashes
        // older binaries with an opaque `missing field ...` serde error.
        // `insert` updates the existing key in place (preserving its
        // position at the top of the file) or appends it when absent.
        doc.as_table_mut().insert(
            "schema_version",
            toml_edit::value(i64::from(crate::migration::CURRENT_SCHEMA_VERSION)),
        );

        let toml_str = ensure_blank_line_before_sections(&doc.to_string());

        write_config_atomically(&config_path, &toml_str).await?;
        self.clear_dirty();
        Ok(())
    }
}

fn collect_onepassword_reference_snapshots(
    config: &Config,
) -> std::collections::HashMap<String, String> {
    config
        .prop_fields()
        .into_iter()
        .filter(|field| field.is_secret)
        .filter_map(|field| {
            let value = crate::env_overrides::raw_value_for_path(config, &field.name)?;
            crate::secrets::SecretStore::is_onepassword_ref(&value).then_some((field.name, value))
        })
        .collect()
}

fn restore_onepassword_references_for_save(
    config_to_save: &mut Config,
    snapshots: &std::collections::HashMap<String, String>,
    dirty_paths: &std::collections::HashSet<String>,
) -> Result<()> {
    for (path, value) in snapshots {
        if dirty_paths.contains(path) {
            continue;
        }
        if let Err(err) = config_to_save.set_prop(path, value) {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"path": path, "error": format!("{}", err)})),
                "1Password reference save-restore failed; field retains resolved value"
            );
        }
    }
    Ok(())
}

/// Atomic write shared by `save()` and `save_dirty()`.
async fn write_config_atomically(config_path: &Path, toml_str: &str) -> Result<()> {
    let parent_dir = config_path
        .parent()
        .context("Config path must have a parent directory")?;

    fs::create_dir_all(parent_dir).await.with_context(|| {
        format!(
            "Failed to create config directory: {}",
            parent_dir.display()
        )
    })?;

    let file_name = config_path
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("config.toml");
    let temp_path = parent_dir.join(format!(".{file_name}.tmp-{}", uuid::Uuid::new_v4()));
    let backup_path = parent_dir.join(format!("{file_name}.bak"));

    let mut temp_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp_path)
        .await
        .with_context(|| {
            format!(
                "Failed to create temporary config file: {}",
                temp_path.display()
            )
        })?;
    temp_file
        .write_all(toml_str.as_bytes())
        .await
        .context("Failed to write temporary config contents")?;
    temp_file
        .sync_all()
        .await
        .context("Failed to fsync temporary config file")?;
    drop(temp_file);

    let had_existing_config = config_path.exists();
    if had_existing_config {
        fs::copy(config_path, &backup_path).await.with_context(|| {
            format!(
                "Failed to create config backup before atomic replace: {}",
                backup_path.display()
            )
        })?;
    }

    if let Err(e) = fs::rename(&temp_path, config_path).await {
        let _ = fs::remove_file(&temp_path).await;
        if had_existing_config && backup_path.exists() {
            fs::copy(&backup_path, config_path)
                .await
                .context("Failed to restore config backup")?;
        }
        anyhow::bail!("Failed to atomically replace config file: {e}");
    }

    #[cfg(unix)]
    {
        use std::{fs::Permissions, os::unix::fs::PermissionsExt};
        if let Err(err) = fs::set_permissions(config_path, Permissions::from_mode(0o600)).await {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                &format!(
                    "Failed to harden config permissions to 0600 at {}: {}",
                    config_path.display().to_string(),
                    err
                )
            );
        }
    }

    sync_directory(parent_dir).await?;

    if had_existing_config {
        let _ = fs::remove_file(&backup_path).await;
    }

    Ok(())
}

/// Write the in-memory value at `dotted` into the doc, or delete the leaf
/// when the value is absent or equals the schema default. Segments are
/// kebab→snake-translated; alias keys never carry hyphens (alias rule).
fn apply_dirty_path(
    root: &mut toml_edit::Table,
    dotted: &str,
    full_table: &toml::Table,
    default_table: &toml::Table,
) -> Result<()> {
    let raw: Vec<&str> = dotted.split('.').collect();
    if raw.is_empty() {
        return Ok(());
    }

    // Natural-key `Vec<T>` sections (`#[natural_key = "<f>"]`, currently
    // `mcp.servers`) serialize as `[[mcp.servers]]` — an
    // `Item::ArrayOfTables`, not a Table. The generic walker below
    // assumes every intermediate node is a Table (`as_table()` /
    // `as_table_mut()`), so without this branch a dirty path like
    // `mcp.servers.fs.command` would resolve to `None` in `full_table`,
    // get misclassified as `should_delete`, then bail at the
    // `ArrayOfTables` segment in `delete_path_in_doc` too. Net effect:
    // the per-field MCP editor's edits go nowhere on disk. See the
    // regression test `save_dirty_persists_mcp_server_field_via_natural_key`.
    if let Some(section) = find_natural_key_section_for_path(dotted) {
        apply_dirty_natural_key_path(root, &section, dotted, full_table, default_table);
        return Ok(());
    }

    // `HashMap<String, T>` sections whose key is a `#[resource_key]`
    // (drawn from another domain — a model id, tool name, …) may
    // themselves contain dots, e.g. `gpt-4.1` in
    // `cost.rates.providers.models.openai.gpt-4.1.input_per_mtok`. The
    // blind `raw.split('.')` above and the generic walker below both
    // assume one segment == one dot-delimited token, so a dotted key
    // fragments into bogus segments (`gpt-4`, `1`), the lookup below
    // finds nothing, and the write silently turns into a no-op delete.
    // Longest-match the key against the section's live keys instead —
    // same precedent as the natural-key branch above.
    if let Some(section) = find_map_key_section_for_path(dotted) {
        return apply_dirty_map_key_path(root, &section, dotted, full_table, default_table);
    }

    // Resolve each segment against the in-memory table: struct fields
    // serialize as snake_case (so `input-per-mtok` → `input_per_mtok`), but
    // HashMap keys are preserved verbatim and may legitimately carry hyphens
    // (`claude-opus-4-7`, `tts-1-hd`). Blind `s.replace('-', "_")` mangles
    // those keys and lookup returns None, which apply_dirty_path treats as
    // "delete this path" — silently dropping every cost.rates save.
    let segments: Vec<String> = resolve_dirty_segments(full_table, &raw);
    let segs: Vec<&str> = segments.iter().map(String::as_str).collect();
    write_or_delete_leaf(root, full_table, default_table, &segs);
    Ok(())
}

/// Resolved metadata for a `#[natural_key]` Vec section that matches a
/// given dirty path. `section_path` is the dotted section (e.g.
/// `"mcp.servers"`); `natural_key_field` is the snake_case field name
/// on the inner type that holds each element's alias (e.g. `"name"`).
struct NaturalKeySection {
    section_path: &'static str,
    natural_key_field: &'static str,
}

/// Return the longest natural-key `Kind::List` section whose path is a
/// strict prefix of `dotted` (i.e. `dotted` starts with `<path>.`).
/// Strict-prefix wins so a nested natural-key section would always
/// outscore its parent; today there's only `mcp.servers`, so the lookup
/// is effectively a single-entry match, but the longest-match rule
/// keeps the writer correct when a future `#[natural_key]` Vec lands
/// inside another section.
///
/// Returns the `<section_path>` (e.g. `"mcp.servers"`) and the
/// `<natural_key_field>` (e.g. `"name"`) the writer needs to locate
/// the matching `[[<section_path>]]` element on disk.
///
/// Section paths are exact-matched against the leading segments of
/// `dotted`; the function does **not** apply the kebab→snake fallback
/// that the inner segment resolver (`resolve_dirty_segments`) uses.
/// That is deliberate and mirrors the in-memory routing macro's
/// `section_path == expected` check in
/// `crates/zeroclaw-macros/src/lib.rs` (the `get_map_keys` /
/// `route_vec_path` arms) — making either side dash-tolerant in
/// isolation would let the dispatcher and the writer disagree on
/// which call participates in this branch, which is exactly the
/// memory-vs-disk divergence this PR exists to eliminate. If a
/// future natural-key section needs an underscore in its path AND
/// must be addressable via a kebab wire path, change **both** sides
/// together (the macro arm and this match).
///
/// Plain `Kind::List` Vec sections without `#[natural_key]` (their
/// `natural_key` is `None`) are intentionally excluded — those use the
/// legacy whole-array `ObjectArray` save path and don't need the
/// array-of-tables walker.
fn find_natural_key_section_for_path(dotted: &str) -> Option<NaturalKeySection> {
    use crate::traits::MapKeyKind;

    let mut best: Option<NaturalKeySection> = None;
    for section in Config::map_key_sections().iter() {
        if section.kind != MapKeyKind::List {
            continue;
        }
        let Some(natural_key_field) = section.natural_key else {
            continue;
        };
        // strip_prefix avoids the allocation `format!("{}.", ...)`
        // would do per section per dirty path, and makes the "must
        // have *something* after the dot" guard a natural fallout of
        // the second `strip_prefix('.')` rather than a length check.
        let Some(after) = dotted
            .strip_prefix(section.path)
            .and_then(|s| s.strip_prefix('.'))
        else {
            continue;
        };
        // Empty alias segment ("mcp.servers." with nothing after) can't
        // be addressed by name; skip to avoid matching every otherwise-
        // unmatched path via the empty prefix. Same guard the in-memory
        // `route_vec_path` applies to empty natural keys.
        if after.is_empty() {
            continue;
        }
        let better = best
            .as_ref()
            .is_none_or(|b| section.path.len() > b.section_path.len());
        if better {
            best = Some(NaturalKeySection {
                section_path: section.path,
                natural_key_field,
            });
        }
    }
    best
}

/// Apply a dirty path of the form
/// `<section_path>.<alias>[.<inner_suffix>...]` against an
/// `[[<section_path>]]` array of tables.
///
/// Three shapes show up in practice:
///
///   1. `<section>.<alias>.<inner>` — per-field edit (the dashboard /
///      TUI editor). Locate the in-memory element whose natural-key
///      field equals `<alias>` and reconcile its `<inner>` sub-path
///      against the doc's matching `[[<section>]]` table.
///
///   2. `<section>.<alias>` with the element still in memory — the
///      whole-element write the rename path emits for the *new* alias.
///      Reseed the matching doc entry from the in-memory serialization
///      so a rename `fs` → `filesystem` updates the entire
///      `[[mcp.servers]]` table (in particular its `name` field).
///
///   3. `<section>.<alias>` with the element gone from memory — the
///      delete path the rename emits for the *old* alias, and what
///      `delete_map_key` plus an explicit `mark_dirty` would produce.
///      Drop the matching doc entry.
///
/// All three are handled by walking the doc through the section's
/// non-array parents, locating the `ArrayOfTables` at the section's
/// last path segment, then either editing or removing the entry whose
/// natural-key field matches `<alias>`. The `<inner>` recursion reuses
/// the existing Table-shaped `set_path_in_doc` / `delete_path_in_doc`
/// helpers against the located entry's table.
///
/// **Order-independence invariant.** `Config::dirty_paths` is a
/// `HashSet<String>`, so the three shapes above can apply in any
/// interleaving within a single `save_dirty` batch — in particular,
/// a rename emits both the old and new aliases dirty without an
/// ordering guarantee, and a create-then-edit sequence may walk the
/// per-field path before the whole-element write. Convergence under
/// every ordering is load-bearing here and works only because every
/// branch derives its write from the current in-memory state and the
/// case-1 delete fires only on mem-absence (not on default
/// equivalence). Future edits — e.g. an early-exit that assumes
/// whole-element writes run before per-field ones, or a "skip if
/// already written" optimization keyed on the doc shape — would
/// break this. Preserve commutativity across the three shapes for
/// any single (section, alias) tuple.
fn apply_dirty_natural_key_path(
    root: &mut toml_edit::Table,
    section: &NaturalKeySection,
    dotted: &str,
    full_table: &toml::Table,
    _default_table: &toml::Table,
) {
    // `dotted` is known to start with `<section_path>.` thanks to
    // `find_natural_key_section_for_path`; split the alias and inner
    // suffix off the tail.
    let rest = match dotted
        .strip_prefix(section.section_path)
        .and_then(|s| s.strip_prefix('.'))
    {
        Some(r) => r,
        None => return,
    };
    let (alias, inner_suffix) = match rest.split_once('.') {
        Some((a, i)) => (a, Some(i)),
        None => (rest, None),
    };
    if alias.is_empty() {
        return;
    }

    // Walk the in-memory serialized array at `section_path`. Each
    // element is a `toml::Value::Table`; the alias lives in the
    // `natural_key_field` column.
    let section_segs: Vec<&str> = section.section_path.split('.').collect();
    let mem_array = lookup_path_in_table(full_table, &section_segs).and_then(|v| v.as_array());
    let mem_entry = mem_array.and_then(|arr| {
        arr.iter().find_map(|item| {
            let table = item.as_table()?;
            let alias_value = table.get(section.natural_key_field)?.as_str()?;
            (alias_value == alias).then_some(table)
        })
    });

    // The doc cursor walks the section's parent Tables (e.g. `mcp`) and
    // arrives at the array node (`servers`). The natural-key Vec is
    // always at the deepest segment of `section_path`; the leading
    // segments are plain struct fields and therefore Tables on disk.
    let Some((array_key, parent_segs)) = section_segs.split_last() else {
        return;
    };

    // Inner-suffix path: per-field edit (case 1). Reconcile the inner
    // sub-path on the matched doc table the same way the generic
    // Table-shaped walker does — mem-vs-default comparison decides
    // delete vs write — and bail out before touching the array spine
    // if the entry doesn't exist in either memory or the doc.
    if let Some(inner) = inner_suffix {
        // Look up the same inner sub-path on the default-shaped element.
        // `Config::default()` always has an empty `mcp.servers` (and no
        // default natural-key Vec entry is non-empty), so we synthesize
        // the default by going through the inner type's own
        // `Default` impl via the existing `default_table` plus an
        // alias-keyed lookup. The Vec being empty in defaults means
        // `default_array_entry` is always `None`, which mirrors the
        // existing HashMap behaviour: `default_val` for an alias's
        // child is None too, because the alias key doesn't exist in
        // `Config::default()`. Keep the comparison shape identical so
        // the delete-on-default rule degrades the same way the HashMap
        // path already does (i.e. it doesn't catch "field is set to
        // its serde default"; that limitation is shared and not a
        // regression from this fix). The `_default_table` parameter
        // is intentionally unused for this reason — kept on the
        // signature so the call site in `save_dirty` stays uniform
        // with `apply_dirty_path`, which does consume it.

        let inner_raw: Vec<&str> = inner.split('.').collect();
        // Same dash-aware segment resolution the top-level Table-shaped
        // walker uses (`apply_dirty_path` → `resolve_dirty_segments`), but
        // rooted at the natural-key entry's own serialized table so kebab
        // inner segments like `mcp.servers.fs.tool-timeout-secs` resolve
        // to the snake `tool_timeout_secs` field on disk. Sharing the
        // helper instead of forking is load-bearing: this PR exists
        // because two parallel walks (in-memory vs on-disk) drifted apart;
        // a forked copy here would recreate exactly that hazard, so any
        // future fix to dash handling lands in both code paths at once.
        let inner_segments: Vec<String> = match mem_entry {
            Some(table) => resolve_dirty_segments(table, &inner_raw),
            None => inner_raw.iter().map(|s| (*s).to_string()).collect(),
        };
        let inner_segs: Vec<&str> = inner_segments.iter().map(String::as_str).collect();

        let mem_inner_val = mem_entry.and_then(|t| lookup_path_in_table(t, &inner_segs));

        // mem missing → delete. mem present → write.
        match mem_inner_val {
            Some(value) => {
                let entry_table = ensure_array_of_tables_entry(
                    root,
                    parent_segs,
                    array_key,
                    section.natural_key_field,
                    alias,
                );
                let Some(entry_table) = entry_table else {
                    return;
                };
                let mut pruned = value.clone();
                prune_empty_leaves(&mut pruned);
                set_path_in_doc(entry_table, &inner_segs, &pruned);
            }
            None => {
                // Only walk the doc if we'd actually find something to
                // delete; otherwise an "ambient" delete pass on a fresh
                // alias would scribble an empty `[[mcp.servers]]` entry
                // via `ensure_array_of_tables_entry`'s create path.
                let Some(entry_table) = find_array_of_tables_entry_mut(
                    root,
                    parent_segs,
                    array_key,
                    section.natural_key_field,
                    alias,
                ) else {
                    return;
                };
                delete_path_in_doc(entry_table, &inner_segs);
            }
        }
        return;
    }

    // Bare-alias path (cases 2 and 3). No inner suffix → reconcile the
    // whole element.
    match mem_entry {
        Some(entry) => {
            // Case 2: rewrite the entire array entry from the in-memory
            // serialization. Prunes empties so default fields don't
            // sprawl, matching the per-field path's `prune_empty_leaves`
            // pass.
            let mut value = toml::Value::Table(entry.clone());
            prune_empty_leaves(&mut value);
            let entry_table = ensure_array_of_tables_entry(
                root,
                parent_segs,
                array_key,
                section.natural_key_field,
                alias,
            );
            let Some(entry_table) = entry_table else {
                return;
            };
            // Replace the table's contents wholesale so removed inner
            // fields actually disappear from disk.
            entry_table.clear();
            if let toml::Value::Table(t) = value {
                for (k, v) in t.into_iter() {
                    entry_table.insert(&k, crate::migration::toml_value_to_edit_item(&v));
                }
            }
        }
        None => {
            // Case 3: alias no longer in memory → drop the matching
            // doc entry. If no doc entry matches, no-op.
            delete_array_of_tables_entry(
                root,
                parent_segs,
                array_key,
                section.natural_key_field,
                alias,
            );
        }
    }
}

/// Return the longest `MapKeyKind::Map` section whose path is a strict
/// prefix of `dotted` (i.e. `dotted` starts with `<path>.`). Longest-match
/// mirrors `find_natural_key_section_for_path`: a nested `HashMap` field's
/// own section always outscores a shorter ancestor section.
fn find_map_key_section_for_path(dotted: &str) -> Option<crate::traits::MapKeySection> {
    use crate::traits::MapKeyKind;

    let mut best: Option<crate::traits::MapKeySection> = None;
    for section in Config::map_key_sections() {
        if section.kind != MapKeyKind::Map {
            continue;
        }
        let Some(after) = dotted
            .strip_prefix(section.path)
            .and_then(|s| s.strip_prefix('.'))
        else {
            continue;
        };
        if after.is_empty() {
            continue;
        }
        let better = best
            .as_ref()
            .is_none_or(|b| section.path.len() > b.path.len());
        if better {
            best = Some(section);
        }
    }
    best
}

/// Apply a dirty path of the form `<section_path>.<key>[.<inner_suffix>...]`
/// against a `HashMap<String, T>` section. `<key>` is the literal map key
/// and, for `#[resource_key]` sections (a model id, tool name, … rather
/// than an operator-chosen alias), may itself contain dots — e.g. `gpt-4.1`
/// in `cost.rates.providers.models.openai.gpt-4.1.input_per_mtok`.
///
/// Longest-match `<key>` against the union of keys live in `full_table`
/// (the in-memory state) and in the doc (`root`), so a key that only
/// survives on one side — the delete half of a rename, or a fresh
/// alias not yet flushed — still resolves. This mirrors
/// `apply_dirty_natural_key_path`'s two-sided lookup for `mcp.servers`.
///
/// A whole-entry write (no inner suffix — `<section_path>.<key>` addresses
/// the map value itself, e.g. the create-map-key path) is the case where
/// `<key>` consumes the entire remainder; that always wins over any
/// shorter prefix-plus-dot match into the same key space, so it's checked
/// first.
///
/// Once `<key>` is resolved, the rest of the path reuses the exact
/// mem-vs-default comparison and doc write/delete the generic Table-shaped
/// walker in `apply_dirty_path` uses — the dotted key is just folded into
/// one opaque segment instead of being split on '.' with everything else.
fn apply_dirty_map_key_path(
    root: &mut toml_edit::Table,
    section: &crate::traits::MapKeySection,
    dotted: &str,
    full_table: &toml::Table,
    default_table: &toml::Table,
) -> Result<()> {
    let section_segs: Vec<&str> = section.path.split('.').collect();
    let remainder = &dotted[section.path.len() + 1..];

    let mem_table = lookup_path_in_table(full_table, &section_segs).and_then(|v| v.as_table());
    let doc_table = lookup_table_in_doc(root, &section_segs);

    let keys: Vec<&str> = mem_table
        .into_iter()
        .flat_map(|t| t.keys().map(String::as_str))
        .chain(doc_table.into_iter().flat_map(|t| t.iter().map(|(k, _)| k)))
        .collect();

    // Exact match first: a whole-entry path for key `gpt-4.1` must not be
    // swallowed by `route_hashmap_path` matching the shorter key `gpt-4`
    // plus a bogus inner suffix `1...`. The cost is the inverse shadow: a
    // pathological key literally named `<other-key>.<field>` wins over a
    // field write addressed to `<other-key>` — acceptable because the
    // exact key is the more specific interpretation of the same bytes.
    let key_match: Option<(&str, Option<String>)> = if keys.contains(&remainder) {
        Some((remainder, None))
    } else {
        crate::helpers::route_hashmap_path(dotted, "", section.path, "", keys.iter().copied())
            .map(|(key, inner)| (key, Some(inner)))
    };

    let Some((key, inner)) = key_match else {
        anyhow::bail!(
            "save_dirty: dirty path `{dotted}` addresses map-key section `{}` but its key \
             resolves in neither the in-memory config nor the on-disk file; refusing to \
             silently drop the write",
            section.path
        );
    };

    let mut segments: Vec<String> = section_segs.iter().map(|s| (*s).to_string()).collect();
    segments.push(key.to_string());

    if let Some(inner) = inner {
        // Same dash-aware segment resolution `apply_dirty_natural_key_path`
        // uses for its inner suffix, rooted at the matched key's own
        // serialized table so kebab inner segments (`tool-timeout-secs`)
        // resolve to the snake struct field on disk.
        let key_table = mem_table
            .and_then(|t| t.get(key))
            .and_then(|v| v.as_table());
        let inner_raw: Vec<&str> = inner.split('.').collect();
        let inner_segments: Vec<String> = match key_table {
            Some(t) => resolve_dirty_segments(t, &inner_raw),
            None => inner_raw.iter().map(|s| (*s).to_string()).collect(),
        };
        segments.extend(inner_segments);
    }

    let segs: Vec<&str> = segments.iter().map(String::as_str).collect();
    write_or_delete_leaf(root, full_table, default_table, &segs);
    Ok(())
}

/// Shared tail of the generic and map-key dirty-path walkers: reconcile
/// the resolved `segs` against the doc — delete the leaf when the
/// in-memory value is absent or equals the schema default, otherwise
/// write the pruned in-memory value. One copy so a future fix to the
/// delete-vs-write rule cannot land in one walker only.
fn write_or_delete_leaf(
    root: &mut toml_edit::Table,
    full_table: &toml::Table,
    default_table: &toml::Table,
    segs: &[&str],
) {
    let mem_val = lookup_path_in_table(full_table, segs);
    let default_val = lookup_path_in_table(default_table, segs);

    let should_delete = match (mem_val, default_val) {
        (None, _) => true,
        (Some(m), Some(d)) if m == d => true,
        _ => false,
    };

    if should_delete {
        delete_path_in_doc(root, segs);
    } else if let Some(value) = mem_val {
        let mut pruned = value.clone();
        prune_empty_leaves(&mut pruned);
        set_path_in_doc(root, segs, &pruned);
    }
}

/// Emit a single `WARN` event when a natural-key writer (ensure /
/// find / delete) bails because the on-disk node has the wrong shape
/// — for example an operator hand-edited `mcp.servers = "foo"`, or
/// wrote `servers = [{ name = "fs", ... }]` (an inline array of
/// tables, which `toml_edit` parses as `Item::Value(Value::Array)`
/// rather than the `Item::ArrayOfTables` this writer produces).
///
/// Without this, `config/set` returns success, the in-memory mutation
/// and the dashboard/TUI both reflect the new value, and the on-disk
/// file silently stays stale — the exact symptom the underlying
/// natural-key persistence bug ( fix) exists to eliminate. The
/// `WARN` shape mirrors the 0600-permissions fallback below: same
/// `Action::Note` / `EventOutcome::Unknown` / module path, so log
/// scrapers that already understand the schema warnings will surface
/// these the same way.
///
/// `op` is `"ensure"`, `"find"`, or `"delete"` — which writer bailed.
/// `kind` is `"parent_segment"` or `"array_key"` — which node was
/// wrong-shaped. `which` is the specific seg/key name involved.
fn warn_natural_key_doc_kind_mismatch(
    parent_segs: &[&str],
    array_key: &str,
    alias: &str,
    kind: &str,
    which: &str,
    op: &str,
) {
    let section_path = if parent_segs.is_empty() {
        array_key.to_string()
    } else {
        format!("{}.{array_key}", parent_segs.join("."))
    };
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
            .with_attrs(::serde_json::json!({
                "section_path": section_path,
                "alias": alias,
                "node_kind": kind,
                "node_name": which,
                "operation": op,
            })),
        "natural-key writer: refusing to clobber hand-edited config.toml node; \
         on-disk file may diverge from in-memory state"
    );
}

/// Locate or create the `[[<section_path>]]` entry whose natural-key
/// field equals `alias`, returning a mutable reference to its inner
/// `toml_edit::Table`.
///
/// Three shapes are reconciled here:
///
///   * Parent Tables along `parent_segs` are auto-created if missing
///     (mirrors `set_path_in_doc`).
///   * The `array_key` slot may not yet exist; insert an
///     `ArrayOfTables` if so.
///   * The array may not yet contain an entry whose natural-key field
///     matches `alias`; push a new table with `<natural_key_field> =
///     "<alias>"` seeded.
///
/// Returns `None` only when an existing doc node at `parent_segs` or
/// `array_key` has the wrong kind (e.g. an operator hand-edited
/// `mcp.servers = "foo"`, or wrote an inline `servers = [{ ... }]`
/// array-of-tables that parses as `Item::Value(Value::Array)` rather
/// than the `Item::ArrayOfTables` shape this writer expects); in that
/// case we refuse to clobber the hand-edit and let the save bail
/// rather than data-loss the user's file. A `WARN`-level event is
/// emitted on every wrong-kind bail so the divergence between memory
/// and disk is observable in logs — silent bails were what let the
/// underlying bug ship.
fn ensure_array_of_tables_entry<'a>(
    root: &'a mut toml_edit::Table,
    parent_segs: &[&str],
    array_key: &str,
    natural_key_field: &str,
    alias: &str,
) -> Option<&'a mut toml_edit::Table> {
    let mut cursor: &mut toml_edit::Table = root;
    for seg in parent_segs {
        if !cursor.contains_key(seg) {
            cursor.insert(seg, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        let next = cursor.get_mut(seg).and_then(|i| i.as_table_mut());
        if next.is_none() {
            warn_natural_key_doc_kind_mismatch(
                parent_segs,
                array_key,
                alias,
                "parent_segment",
                seg,
                "ensure",
            );
            return None;
        }
        cursor = next?;
    }
    if !cursor.contains_key(array_key) {
        cursor.insert(
            array_key,
            toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()),
        );
    }
    let aot = cursor
        .get_mut(array_key)
        .and_then(|i| i.as_array_of_tables_mut());
    let Some(aot) = aot else {
        warn_natural_key_doc_kind_mismatch(
            parent_segs,
            array_key,
            alias,
            "array_key",
            array_key,
            "ensure",
        );
        return None;
    };

    let pos = aot
        .iter()
        .position(|t| t.get(natural_key_field).and_then(|i| i.as_str()) == Some(alias));
    let idx = match pos {
        Some(i) => i,
        None => {
            let mut new_table = toml_edit::Table::new();
            new_table.insert(natural_key_field, toml_edit::value(alias.to_string()));
            aot.push(new_table);
            aot.len() - 1
        }
    };
    aot.get_mut(idx)
}

/// Read-only counterpart to [`ensure_array_of_tables_entry`] used by
/// the delete path: never creates a missing array or entry, just
/// locates one if present. Returning `None` on a miss lets the caller
/// skip the doc-mutation step entirely. Distinguishes "node doesn't
/// exist" (silent no-op — the legitimate fresh-alias case) from "node
/// exists but has the wrong kind" (a `WARN`-logged bail — the
/// hand-edited inline-array hazard).
fn find_array_of_tables_entry_mut<'a>(
    root: &'a mut toml_edit::Table,
    parent_segs: &[&str],
    array_key: &str,
    natural_key_field: &str,
    alias: &str,
) -> Option<&'a mut toml_edit::Table> {
    let mut cursor: &mut toml_edit::Table = root;
    for seg in parent_segs {
        let item = cursor.get_mut(seg)?;
        let Some(next) = item.as_table_mut() else {
            warn_natural_key_doc_kind_mismatch(
                parent_segs,
                array_key,
                alias,
                "parent_segment",
                seg,
                "find",
            );
            return None;
        };
        cursor = next;
    }
    let item = cursor.get_mut(array_key)?;
    let Some(aot) = item.as_array_of_tables_mut() else {
        warn_natural_key_doc_kind_mismatch(
            parent_segs,
            array_key,
            alias,
            "array_key",
            array_key,
            "find",
        );
        return None;
    };
    let pos = aot
        .iter()
        .position(|t| t.get(natural_key_field).and_then(|i| i.as_str()) == Some(alias))?;
    aot.get_mut(pos)
}

/// Remove the `[[<section_path>]]` entry whose natural-key field
/// equals `alias`. No-op if the array or entry isn't present. When the
/// removal empties the `ArrayOfTables`, drop the array slot too so the
/// on-disk file doesn't carry a vestigial `[[mcp.servers]]`-shaped
/// blank section header.
fn delete_array_of_tables_entry(
    root: &mut toml_edit::Table,
    parent_segs: &[&str],
    array_key: &str,
    natural_key_field: &str,
    alias: &str,
) {
    let mut cursor: &mut toml_edit::Table = root;
    for seg in parent_segs {
        let Some(item) = cursor.get_mut(seg) else {
            return;
        };
        match item.as_table_mut() {
            Some(next) => cursor = next,
            None => {
                warn_natural_key_doc_kind_mismatch(
                    parent_segs,
                    array_key,
                    alias,
                    "parent_segment",
                    seg,
                    "delete",
                );
                return;
            }
        }
    }
    let Some(item) = cursor.get_mut(array_key) else {
        return;
    };
    let aot = match item.as_array_of_tables_mut() {
        Some(a) => a,
        None => {
            warn_natural_key_doc_kind_mismatch(
                parent_segs,
                array_key,
                alias,
                "array_key",
                array_key,
                "delete",
            );
            return;
        }
    };
    let pos = aot
        .iter()
        .position(|t| t.get(natural_key_field).and_then(|i| i.as_str()) == Some(alias));
    if let Some(idx) = pos {
        aot.remove(idx);
    }
    if aot.is_empty() {
        cursor.remove(array_key);
    }
}

/// Drop empty arrays / tables / strings from a value before writing it
/// to the doc. HashMap entries serialize every default field (no
/// `skip_serializing_if` on individual `Vec<String>` fields), so without
/// this pass an `mcp_bundles.<alias>` write produces `servers = []`,
/// `exclude = []`, etc. The pruned form round-trips identically because
/// each dropped field's serde default IS the dropped value.
fn prune_empty_leaves(value: &mut toml::Value) {
    match value {
        toml::Value::Table(t) => {
            let keys: Vec<String> = t.keys().cloned().collect();
            for key in keys {
                if let Some(inner) = t.get_mut(&key) {
                    prune_empty_leaves(inner);
                }
                let drop = match t.get(&key) {
                    Some(toml::Value::Array(arr)) => arr.is_empty(),
                    Some(toml::Value::Table(inner)) => inner.is_empty(),
                    Some(toml::Value::String(s)) => s.is_empty(),
                    _ => false,
                };
                if drop {
                    t.remove(&key);
                }
            }
        }
        toml::Value::Array(arr) => {
            for item in arr.iter_mut() {
                prune_empty_leaves(item);
            }
        }
        _ => {}
    }
}

fn resolve_dirty_segments(root: &toml::Table, raw: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    let mut current: Option<&toml::Value> = None;
    for seg in raw {
        let table_opt: Option<&toml::Table> = if out.is_empty() {
            Some(root)
        } else {
            current.and_then(|v| v.as_table())
        };
        let resolved = match table_opt {
            Some(t) if t.contains_key(*seg) => (*seg).to_string(),
            Some(t) => {
                let snake = seg.replace('-', "_");
                if t.contains_key(&snake) {
                    snake
                } else {
                    (*seg).to_string()
                }
            }
            None => (*seg).to_string(),
        };
        current = table_opt.and_then(|t| t.get(&resolved));
        out.push(resolved);
    }
    out
}

fn lookup_path_in_table<'a>(root: &'a toml::Table, segs: &[&str]) -> Option<&'a toml::Value> {
    let mut current: Option<&toml::Value> = None;
    for (i, seg) in segs.iter().enumerate() {
        let table = if i == 0 { root } else { current?.as_table()? };
        current = table.get(*seg);
    }
    current
}

/// Read-only walk to the table-like node at `segs`, or `None` if any
/// segment is missing or not table-shaped on disk. Used to read a map-key
/// section's live on-disk keys without mutating the doc. `TableLike`
/// rather than `Table` because ZeroClaw loads (though never writes)
/// hand-edited inline tables — `openai = { "gpt-4.1" = { ... } }` parses
/// as `Item::Value(Value::InlineTable)`, which `as_table()` rejects; a
/// key living only in such a section must still resolve here or the
/// unresolvable-key bail in `apply_dirty_map_key_path` aborts the whole
/// `save_dirty` batch.
fn lookup_table_in_doc<'a>(
    root: &'a toml_edit::Table,
    segs: &[&str],
) -> Option<&'a dyn toml_edit::TableLike> {
    let mut cursor: &dyn toml_edit::TableLike = root;
    for seg in segs {
        cursor = cursor.get(seg)?.as_table_like()?;
    }
    Some(cursor)
}

/// `TableLike` rather than `Table` for the traversal cursor — same reason
/// as `lookup_table_in_doc` above: a map-key section may live inside a
/// hand-edited inline table (`openai = { "gpt-4.1" = { ... } }`), which
/// parses as `Item::Value(Value::InlineTable)`. `as_table_mut()` returns
/// `None` for that shape, so a `Table`-only cursor would silently return
/// without removing anything — `save_dirty` reports success while the key
/// stays on disk. `as_table_like_mut()` descends into both `Table` and
/// `InlineTable`, and `TableLike::remove` deletes from either.
fn delete_path_in_doc(root: &mut toml_edit::Table, segs: &[&str]) {
    let Some((last, parents)) = segs.split_last() else {
        return;
    };
    let mut cursor: &mut dyn toml_edit::TableLike = root;
    for seg in parents {
        cursor = match cursor.get_mut(seg).and_then(|i| i.as_table_like_mut()) {
            Some(t) => t,
            None => return,
        };
    }
    cursor.remove(last);
}

/// Same `TableLike` traversal as `delete_path_in_doc`, for the same
/// reason: a write into a key that already lives inside a hand-edited
/// inline table must land in that inline table rather than silently
/// no-op-ing because the cursor only understood standard `Table` nodes.
fn set_path_in_doc(root: &mut toml_edit::Table, segs: &[&str], value: &toml::Value) {
    let Some((last, parents)) = segs.split_last() else {
        return;
    };
    let mut cursor: &mut dyn toml_edit::TableLike = root;
    for seg in parents {
        if !cursor.contains_key(seg) {
            cursor.insert(seg, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        cursor = match cursor.get_mut(seg).and_then(|i| i.as_table_like_mut()) {
            Some(t) => t,
            None => return,
        };
    }
    let new_item = crate::migration::toml_value_to_edit_item(value);
    cursor.insert(last, new_item);
}

#[allow(clippy::unused_async)] // async needed on unix for tokio File I/O; no-op on other platforms
async fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let dir = File::open(path).await.with_context(|| {
            format!(
                "Failed to open directory for fsync: {}",
                path.display().to_string()
            )
        })?;
        dir.sync_all().await.with_context(|| {
            format!(
                "Failed to fsync directory metadata: {}",
                path.display().to_string()
            )
        })?;
        Ok(())
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x02000000;
        let dir = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .with_context(|| {
                format!(
                    "Failed to open directory for fsync: {}",
                    path.display().to_string()
                )
            })?;
        // FlushFileBuffers on directory handles returns ERROR_ACCESS_DENIED on
        // Windows (OS Error 5). This is expected — NTFS does not support
        // flushing directory metadata the same way Unix does. The individual
        // files have already been synced, so it is safe to ignore this error.
        if let Err(e) = dir.sync_all() {
            if e.raw_os_error() == Some(5) {
                ::zeroclaw_log::record!(
                    TRACE,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    &format!(
                        "Ignoring expected ACCESS_DENIED when fsyncing directory on Windows: {}",
                        path.display().to_string()
                    )
                );
            } else {
                return Err(e).with_context(|| {
                    format!(
                        "Failed to fsync directory metadata: {}",
                        path.display().to_string()
                    )
                });
            }
        }
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(())
    }
}

// ── SOP configuration ──────────────────────────────────────────

/// Standard Operating Procedures configuration (`[sop]`).
///
/// Definition-side keys (`sops_dir`, `default_execution_mode`) stay live for
/// SOP.toml loading and authoring. The run-side engine was removed (wall-5
/// slice 2); the remaining fields are inert retention knobs kept so legacy
/// `[sop]` tables still parse. SOP runs are Tachi-side ProcedureRuns through
/// the procedure_v1 seam.
///
/// The `default_execution_mode` field uses the `SopExecutionMode` type from
/// `sop::types` (re-exported via `sop::SopExecutionMode`). To avoid circular
/// module references, config stores it using the same enum definition.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "sop"]
pub struct SopConfig {
    /// Directory containing SOP definitions (subdirs with SOP.toml + SOP.md).
    /// Optional override. When omitted, the runtime and CLI both resolve the
    /// default `<workspace>/sops`; SOPs load from there whenever it exists.
    #[serde(default)]
    pub sops_dir: Option<String>,

    /// Default execution mode for SOPs that omit `execution_mode`.
    /// Values: `auto`, `supervised` (default), `step_by_step`,
    /// `priority_based`, `deterministic`.
    #[serde(default = "default_sop_execution_mode")]
    pub default_execution_mode: String,

    /// RETIRED with the SOP run side. No longer read; kept so existing
    /// `[sop]` tables still parse.
    #[serde(default = "default_sop_max_concurrent_total")]
    pub max_concurrent_total: usize,

    /// RETIRED with the SOP run side (approval broker removed). No longer
    /// read; kept so existing `[sop]` tables still parse.
    #[serde(default = "default_sop_approval_timeout_secs")]
    pub approval_timeout_secs: u64,

    /// RETIRED with the SOP run side. No longer read; kept so existing
    /// `[sop]` tables still parse.
    #[serde(default = "default_sop_max_finished_runs")]
    pub max_finished_runs: usize,

    /// RETIRED with the SOP run side (maintenance tick removed). No longer
    /// read; kept so existing `[sop]` tables still parse.
    #[serde(default = "default_sop_maintenance_interval_secs")]
    pub maintenance_interval_secs: u64,

    /// RETIRED with the SOP run side (run store removed; this key's old doc
    /// named the deleted `build_sop_engine`). No longer read; kept so
    /// existing `[sop]` tables still parse.
    #[serde(default = "default_sop_persist_runs")]
    pub persist_runs: bool,

    /// RETIRED with the SOP run side (run store removed). No longer read;
    /// kept so existing `[sop]` tables still parse.
    #[serde(default)]
    pub run_store_backend: SopRunStoreBackend,

    /// RETIRED with the SOP run side (run store removed). No longer read; a
    /// leftover `<data_dir>/sop/runs.db` is left in place and reported by a
    /// boot-time warning.
    #[serde(default)]
    pub run_state_dir: Option<String>,

    /// RETIRED with the SOP run side (approval broker removed). No longer
    /// read; kept so existing `[sop]` tables still parse.
    #[serde(default)]
    pub approval_mode: ApprovalMode,

    /// RETIRED with the SOP run side (approval broker removed). No longer
    /// read; kept so existing `[sop]` tables still parse.
    #[serde(default)]
    pub approval_timeout_action: ApprovalTimeoutAction,

    /// RETIRED with the SOP run side (approval broker removed). No longer
    /// read; kept so existing `[sop]` tables still parse.
    #[serde(default)]
    #[nested]
    pub approval: SopApprovalConfig,

    /// RETIRED with the SOP run side (per-step scope consumption removed
    /// from the agent turn). No longer read; kept so existing `[sop]`
    /// tables still parse.
    #[serde(default)]
    pub step_scope_enforce: bool,

    /// RETIRED with the SOP run side. No longer read; kept so existing
    /// `[sop]` tables still parse.
    #[serde(default = "default_sop_step_mandatory_tools")]
    pub step_mandatory_tools: Vec<String>,

    /// RETIRED with the SOP run side (engine step execution removed). No
    /// longer read; kept so existing `[sop]` tables still parse.
    #[serde(default = "default_sop_step_schema_enforce")]
    pub step_schema_enforce: bool,

    /// RETIRED with the SOP run side (engine step routing removed). No
    /// longer read; kept so existing `[sop]` tables still parse.
    #[serde(default = "default_sop_max_step_visits")]
    pub max_step_visits: u32,

    /// RETIRED with the SOP run side (engine step failure policy removed).
    /// No longer read; kept so existing `[sop]` tables still parse.
    #[serde(default = "default_sop_max_step_retries")]
    pub max_step_retries: u32,

    /// Maximum bytes accepted from untrusted SOP trigger payload/topic content
    /// before char-boundary truncation. 0 disables the cap.
    #[serde(default = "default_sop_untrusted_payload_max_bytes")]
    pub untrusted_payload_max_bytes: usize,

    /// Prompt-guard action for untrusted SOP trigger input: warn, block, or sanitize.
    #[serde(default = "default_sop_untrusted_input_guard")]
    pub untrusted_input_guard: String,

    /// Prompt-guard and outbound-redaction sensitivity for untrusted SOP content.
    #[serde(default = "default_sop_untrusted_guard_sensitivity")]
    pub untrusted_guard_sensitivity: f64,

    /// Include the explanatory warning text inside untrusted-content frames.
    /// Boundary framing itself is always on once wired.
    #[serde(default = "default_sop_untrusted_frame_warning")]
    pub untrusted_frame_warning: bool,

    /// Redact outbound SOP content before persistence/audit consumers write it.
    /// Intended to converge with the shared B/F redaction switch once those consumers land.
    #[serde(default = "default_sop_untrusted_outbound_redact")]
    pub untrusted_outbound_redact: bool,

    /// RETIRED with the SOP run side (procedural-memory tooling removed). No
    /// longer read; kept so existing `[sop]` tables still parse.
    #[serde(default)]
    pub procedural_memory_enabled: bool,
}

fn default_sop_execution_mode() -> String {
    "supervised".to_string()
}

/// RETIRED with the SOP run side (run store removed): kept only so existing
/// `[sop]` tables still parse. A closed, compile-time-known set, so it is a
/// serde enum rather than free text (mirrors `SandboxBackend` /
/// `ObservabilityBackend` in this file); unknown values are rejected at parse
/// time.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum SopRunStoreBackend {
    /// RETIRED with the SOP run side; parse-compat default.
    #[default]
    Sqlite,
    /// RETIRED with the SOP run side; parse-compat value.
    Memory,
}

/// RETIRED with the SOP run side (approval broker removed): parse-compat
/// selector, no longer read. Closed set => serde enum.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// Agent `sop_approve` OR an out-of-band principal both clear. Backward-compat default.
    #[default]
    Both,
    /// Only out-of-band principals (CLI / WS / HTTP / timeout) clear; `sop_approve`
    /// becomes a no-op. Lets an operator require a separate approver without bricking runs.
    OutOfBandRequired,
    /// Only the agent tool clears; out-of-band surfaces are read-only pending lists. Niche.
    AgentTool,
}

/// RETIRED with the SOP run side (approval broker removed): parse-compat
/// selector, no longer read. Closed set => serde enum.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ApprovalTimeoutAction {
    /// Re-surface to the out-of-band approver and keep waiting; never self-approve.
    /// Default, fail-closed.
    #[default]
    Escalate,
    /// Terminate the run (fail-safe cancel). Strict mode.
    Cancel,
    /// LEGACY: auto-approve on timeout. The ONLY path to the old fail-open behavior;
    /// opt-in only.
    AutoApprove,
}

/// `[sop.approval]` - RETIRED with the SOP run side (approval broker removed):
/// kept only so existing tables still parse; values are no longer read.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "sop_approval"]
pub struct SopApprovalConfig {
    /// Named approver groups: `group name -> members`. A member is matched against
    /// the transport-derived (channel-authenticated) `ApprovalPrincipal` identity.
    /// A member may be source-qualified (`<source>:<identity>`, e.g. `http:ZeroClawOperator`,
    /// `ws:<subject>`, `agent:<alias>`) to grant rights on one transport only, or a
    /// bare identity (`alice`) to grant from any non-channel source. Channel members
    /// must include the channel namespace (`channel:<channel-key>:<sender>`) so sender
    /// ids from different channel aliases cannot collide. A future auth system adds a
    /// second resolver alongside this one; it does not replace channel identities.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    #[nested]
    pub groups: std::collections::HashMap<String, ApprovalGroupConfig>,
    /// Named approval policies a SOP step may reference by name.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    #[nested]
    pub policies: std::collections::HashMap<String, ApprovalPolicyConfig>,
}

/// A named approver group (`[sop.approval.groups.<name>]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "sop_approval_group"]
pub struct ApprovalGroupConfig {
    /// Identity labels that belong to this group.
    #[serde(default)]
    pub members: Vec<String>,
}

/// A named approval policy (`[sop.approval.policies.<name>]`) the broker enforces.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "sop_approval_policy"]
pub struct ApprovalPolicyConfig {
    /// Group whose members may satisfy this policy's gate. `None`/empty = any
    /// principal permitted by `approval_mode` (back-compat, no membership gate).
    #[serde(default)]
    pub required_group: Option<String>,
    /// Distinct approvers required before the gate clears. `0`/`1` both mean a
    /// single approval; `>= 2` requires a quorum of distinct approver identities.
    #[serde(default)]
    pub quorum: u32,
    /// Channel to deliver the INITIAL approval request to when a run parks at a
    /// gate this policy governs, formatted `channel:recipient` (e.g.
    /// `discord.ops:123456789012345678`). The `channel` names a configured channel
    /// (the `<channel>.<alias>` / bare-`<channel>` key from the channel map); the
    /// `recipient` is that channel's addressee (a Discord channel id, a chat id,
    /// etc.). Delivery is best-effort - the gate is the source of truth and a
    /// delivery failure never blocks or clears it; approvals still come back
    /// through the normal HTTP/WS/tool surfaces. `None`/empty = no out-of-band
    /// request notice (today's behavior: only the originating surface is notified).
    /// This is a DISTINCT lifecycle event from `escalation_route`: the request goes
    /// out when the run parks; the escalation goes out only if it later times out.
    /// When configured, the route must use the `channel:recipient` format.
    #[serde(default)]
    pub request_route: Option<String>,
    /// Route to escalate to on timeout (the distinct "second route"). `None`/empty
    /// re-surfaces to the same route (today's `Escalate` behavior). When configured,
    /// the route must use the `channel:recipient` format.
    #[serde(default)]
    pub escalation_route: Option<String>,
}

fn default_sop_max_concurrent_total() -> usize {
    4
}

fn default_sop_approval_timeout_secs() -> u64 {
    300
}

fn default_sop_max_finished_runs() -> usize {
    100
}

fn default_sop_persist_runs() -> bool {
    true
}

fn default_sop_step_mandatory_tools() -> Vec<String> {
    // The sop_* run tools are retired; nothing is mandatory by
    // default. The key itself is removed with the run-side config sweep.
    Vec::new()
}

fn default_sop_step_schema_enforce() -> bool {
    true
}

fn default_sop_max_step_visits() -> u32 {
    256
}

fn default_sop_max_step_retries() -> u32 {
    2
}

fn default_sop_untrusted_payload_max_bytes() -> usize {
    8192
}

fn default_sop_untrusted_input_guard() -> String {
    "warn".to_string()
}

fn default_sop_untrusted_guard_sensitivity() -> f64 {
    0.7
}

fn default_sop_untrusted_frame_warning() -> bool {
    true
}

fn default_sop_untrusted_outbound_redact() -> bool {
    true
}

fn default_sop_maintenance_interval_secs() -> u64 {
    60
}

impl Default for SopConfig {
    fn default() -> Self {
        Self {
            sops_dir: None,
            default_execution_mode: default_sop_execution_mode(),
            max_concurrent_total: default_sop_max_concurrent_total(),
            approval_timeout_secs: default_sop_approval_timeout_secs(),
            max_finished_runs: default_sop_max_finished_runs(),
            maintenance_interval_secs: default_sop_maintenance_interval_secs(),
            persist_runs: default_sop_persist_runs(),
            run_store_backend: SopRunStoreBackend::Sqlite,
            run_state_dir: None,
            approval_mode: ApprovalMode::Both,
            approval_timeout_action: ApprovalTimeoutAction::Escalate,
            approval: SopApprovalConfig::default(),
            step_scope_enforce: false,
            step_mandatory_tools: default_sop_step_mandatory_tools(),
            step_schema_enforce: default_sop_step_schema_enforce(),
            max_step_visits: default_sop_max_step_visits(),
            max_step_retries: default_sop_max_step_retries(),
            untrusted_payload_max_bytes: default_sop_untrusted_payload_max_bytes(),
            untrusted_input_guard: default_sop_untrusted_input_guard(),
            untrusted_guard_sensitivity: default_sop_untrusted_guard_sensitivity(),
            untrusted_frame_warning: default_sop_untrusted_frame_warning(),
            untrusted_outbound_redact: default_sop_untrusted_outbound_redact(),
            procedural_memory_enabled: false,
        }
    }
}

impl HasPropKind for serde_json::Value {
    // `serde_json::Value` is an arbitrary JSON document, not an enum.
    // Classifying it as `Enum` previously made `enum_variants_for::<Value>()`
    // hand back the literal placeholder `"(unknown variants)"`, and the
    // dashboard form rendered fields like `model_providers.<key>.provider_extra`
    // as a single-option dropdown. `String` is the closest scalar kind —
    // the form renders a text input where the user pastes raw JSON.
    // Round-trip via `set_prop` stays correct: serde deserializes the TOML
    // string back into `Value::String(...)`. Power users editing complex
    // objects still use `zeroclaw config set --json` or hand-edit the
    // `config.toml`.
    const PROP_KIND: PropKind = PropKind::String;
}

#[cfg(test)]
mod tests;

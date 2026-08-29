//! Channel subsystem for messaging platform integrations.

#[cfg(feature = "channel-acp-server")]
pub mod acp_server;
pub mod media_pipeline;

mod channel_system_prompt;
pub(crate) use channel_system_prompt::{
    build_channel_system_prompt_for_message_with_signal, build_channel_turn_context_preamble,
    compose_outgoing_user_turn_with_context,
};

mod reply_intent;
#[cfg(test)]
pub(crate) use reply_intent::NoReplyKind;
pub(crate) use reply_intent::{AssistantChannelOutcome, parse_reply_intent};
// Test suites under `orchestrator::tests` pull these through `use super::*`.
#[cfg(test)]
pub(crate) use channel_system_prompt::{
    build_channel_system_prompt, build_channel_system_prompt_for_message,
    channel_delivery_instructions,
};

mod outbound_sanitize;
#[cfg(test)]
pub(crate) use outbound_sanitize::strip_think_tags_inline;
#[cfg(feature = "channel-telegram")]
pub(crate) use outbound_sanitize::strip_tool_call_tags;
#[cfg(test)]
pub(crate) use outbound_sanitize::{
    EMPTY_CHANNEL_REPLY_FALLBACK, OutboundContentFormat, channel_outbound_protected_spans,
    sanitize_channel_response, sanitize_channel_response_with_leak_detection,
    strip_isolated_tool_json_artifacts,
};
pub(crate) use outbound_sanitize::{
    ensure_nonempty_channel_reply, outbound_content_format_for_channel,
    redact_channel_outbound_leaks, sanitize_channel_response_for_format_with_leak_detection,
    sanitize_streaming_draft_text, strip_tool_result_content, strip_tool_summary_prefix,
};

mod runtime_commands;
pub(crate) use runtime_commands::{
    ChannelRuntimeCommand, ModelsCommandResolution, OverrideScope, build_config_block_kit,
    build_config_text_response, build_models_help_response, build_providers_help_response,
    channel_runtime_cli_string, channel_runtime_cli_string_with_args, channel_runtime_scope_label,
    parse_runtime_command, resolve_models_command, resolve_provider_ref_for_runtime_switch,
};

mod channel_factories;
#[cfg(feature = "channel-nostr")]
pub(crate) use channel_factories::ActiveChannelAliases;
#[cfg(feature = "channel-matrix")]
pub(crate) use channel_factories::matrix_state_dir;
pub(crate) use channel_factories::{
    ConfiguredChannel, collect_configured_channels, composite_channel_key, configured_channel_map,
};
pub use channel_factories::{build_channel_map, register_channels_for_tools};

mod process_message;
use process_message::process_channel_message;

mod channel_build;
use channel_build::build_channel_by_id;
#[cfg(test)]
use channel_build::one_shot_channel_workspace_dir;

mod start_channels;
pub use start_channels::start_channels;

mod deliver_announcement;
pub use deliver_announcement::deliver_announcement;

mod inbox;
pub(crate) use inbox::{Admission, MessageInbox};

mod task_prefs;
pub(crate) use task_prefs::TaskPreferenceOverlay;

// Channel types imported directly from source crates (no shim files)
#[cfg(feature = "channel-amqp")]
pub use crate::amqp::AmqpChannel;
#[cfg(feature = "channel-bluesky")]
pub use crate::bluesky::BlueskyChannel;
#[cfg(feature = "channel-clawdtalk")]
pub use crate::clawdtalk::ClawdTalkChannel;
#[cfg(feature = "channel-dingtalk")]
pub use crate::dingtalk::DingTalkChannel;
#[cfg(feature = "channel-discord")]
pub use crate::discord::DiscordChannel;
#[cfg(feature = "channel-email")]
pub use crate::email_channel::EmailChannel;
#[cfg(feature = "channel-git")]
pub use crate::git::GitChannel;
#[cfg(feature = "channel-email")]
pub use crate::gmail_push::GmailPushChannel;
#[cfg(feature = "channel-imessage")]
pub use crate::imessage::IMessageChannel;
#[cfg(feature = "channel-irc")]
pub use crate::irc::IrcChannel;
#[cfg(feature = "channel-lark")]
pub use crate::lark::LarkChannel;
#[cfg(feature = "channel-line")]
pub use crate::line::LineChannel;
#[cfg(feature = "channel-linq")]
pub use crate::linq::LinqChannel;
#[cfg(feature = "channel-mattermost")]
pub use crate::mattermost::MattermostChannel;
#[cfg(feature = "channel-mochat")]
pub use crate::mochat::MochatChannel;
#[cfg(feature = "channel-nextcloud")]
pub use crate::nextcloud_talk::NextcloudTalkChannel;
#[cfg(feature = "channel-nostr")]
pub use crate::nostr::NostrChannel;
#[cfg(feature = "channel-notion")]
pub use crate::notion::NotionChannel;
#[cfg(feature = "channel-qq")]
pub use crate::qq::QQChannel;
#[cfg(feature = "channel-reddit")]
pub use crate::reddit::RedditChannel;
#[cfg(feature = "channel-signal")]
pub use crate::signal::SignalChannel;
#[cfg(feature = "channel-slack")]
pub use crate::slack::SlackChannel;
pub use crate::transcription;
pub use crate::tts::{TtsManager, TtsProvider};
#[cfg(feature = "channel-twitch")]
pub use crate::twitch::TwitchChannel;
#[cfg(feature = "channel-twitter")]
pub use crate::twitter::TwitterChannel;
#[cfg(feature = "channel-voice-call")]
pub use crate::voice_call::VoiceCallChannel;
#[cfg(feature = "voice-wake")]
pub use crate::voice_wake::VoiceWakeChannel;
#[cfg(feature = "channel-wati")]
pub use crate::wati::WatiChannel;
#[cfg(feature = "channel-webhook")]
pub use crate::webhook::WebhookChannel;
#[cfg(feature = "channel-wechat")]
pub use crate::wechat::WeChatChannel;
#[cfg(feature = "channel-wecom")]
pub use crate::wecom::WeComChannel;
#[cfg(feature = "channel-wecom-ws")]
pub use crate::wecom_ws::WeComWsChannel;
#[cfg(feature = "channel-wecom-ws")]
use crate::wecom_ws::WeComWsRuntimePolicy;
#[cfg(feature = "channel-whatsapp-cloud")]
pub use crate::whatsapp::WhatsAppChannel;
pub use zeroclaw_api::channel::{Channel, ChannelMessage, SendMessage};
// Local channel types (in misc, not zeroclaw-channels)
pub use crate::cli::CliChannel;
pub use crate::link_enricher;
#[cfg(feature = "channel-matrix")]
pub use crate::matrix::MatrixChannel;
#[cfg(feature = "channel-telegram")]
pub use crate::telegram::TelegramChannel;
#[cfg(feature = "whatsapp-web")]
pub use crate::whatsapp_web::WhatsAppWebChannel;
pub use zeroclaw_infra::debounce::MessageDebouncer;
pub use zeroclaw_infra::session_backend::SessionBackend;
pub use zeroclaw_infra::session_sqlite::SqliteSessionBackend;
pub use zeroclaw_infra::stall_watchdog::StallWatchdog;

use anyhow::{Context, Result};
use parking_lot::RwLock;
use portable_atomic::{AtomicU64, Ordering};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::time::Instant;
use std::time::{Duration, SystemTime};
use tokio_util::sync::CancellationToken;

use zeroclaw_api::memory_traits::MemoryStrategy;
use zeroclaw_api::session_keys::sanitize_session_key;
use zeroclaw_config::scattered_types::{ThinkingConfig, ThinkingLevel};
use zeroclaw_config::schema::Config;
#[cfg(test)]
use zeroclaw_memory::MEMORY_CONTEXT_OPEN;
use zeroclaw_memory::{self, Memory};
use zeroclaw_providers::{self, ChatMessage, ModelProvider, ProviderDispatch};
#[cfg(test)]
use zeroclaw_runtime::agent::loop_::build_tool_instructions_for_names;
use zeroclaw_runtime::agent::loop_::{
    TurnOutcome, append_pinned_mcp_section, apply_text_tool_prompt_policy,
    settle_announcement_guards,
};
use zeroclaw_runtime::approval::ApprovalManager;
use zeroclaw_runtime::observability::Observer;
use zeroclaw_runtime::observability::traits::{ObserverEvent, ObserverMetric};
use zeroclaw_runtime::platform;
use zeroclaw_runtime::security::{AutonomyLevel, SecurityPolicy};
use zeroclaw_runtime::tools::{self, Tool};
use zeroclaw_runtime::util::truncate_with_ellipsis;

type CronChannelRegistry = Arc<HashMap<String, Arc<dyn Channel>>>;

/// Live channel registry consulted by `deliver_announcement` so cron sends reuse the
/// authenticated channel instance (Matrix E2EE can't tolerate per-send session restore).
/// Replaced wholesale by each `start_channels` call.
static CRON_CHANNEL_REGISTRY: std::sync::RwLock<Option<CronChannelRegistry>> =
    std::sync::RwLock::new(None);

/// Observer wrapper that forwards tool-call events to a channel sender
/// for real-time threaded notifications.
struct ChannelNotifyObserver {
    inner: Arc<dyn Observer>,
    tx: tokio::sync::mpsc::Sender<String>,
    tools_used: AtomicBool,
}

const NOTIFY_DETAIL_MAX_CHARS: usize = 4096;

impl Observer for ChannelNotifyObserver {
    fn record_event(&self, event: &ObserverEvent) {
        if let ObserverEvent::ToolCallStart {
            tool, arguments, ..
        } = event
        {
            self.tools_used.store(true, Ordering::Relaxed);
            let detail = match arguments {
                Some(args) if !args.is_empty() => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(args) {
                        if let Some(cmd) = v.get("command").and_then(|c| c.as_str()) {
                            format!(": `{}`", truncate_with_ellipsis(cmd, 200))
                        } else if let Some(q) = v.get("query").and_then(|c| c.as_str()) {
                            format!(": {}", truncate_with_ellipsis(q, 200))
                        } else if let Some(p) = v.get("path").and_then(|c| c.as_str()) {
                            format!(": {}", truncate_with_ellipsis(p, NOTIFY_DETAIL_MAX_CHARS))
                        } else if let Some(u) = v.get("url").and_then(|c| c.as_str()) {
                            format!(": {}", truncate_with_ellipsis(u, NOTIFY_DETAIL_MAX_CHARS))
                        } else {
                            let s = args.to_string();
                            format!(": {}", truncate_with_ellipsis(&s, 120))
                        }
                    } else {
                        let s = args.to_string();
                        format!(": {}", truncate_with_ellipsis(&s, 120))
                    }
                }
                _ => String::new(),
            };
            let _ = self.tx.try_send(format!("\u{1F527} `{tool}`{detail}"));
        }
        self.inner.record_event(event);
    }
    fn record_metric(&self, metric: &ObserverMetric) {
        self.inner.record_metric(metric);
    }
    fn flush(&self) {
        self.inner.flush();
    }
    fn name(&self) -> &str {
        "channel-notify"
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Per-sender conversation history for channel messages.
/// Bounded by `MAX_CONVERSATION_SENDERS` — oldest-accessed senders are evicted.
type ConversationHistoryMap = Arc<Mutex<lru::LruCache<String, Vec<ChatMessage>>>>;
/// Senders that requested `/new` or `/clear` and must force a fresh prompt on their next message.
type PendingNewSessionSet = Arc<Mutex<HashSet<String>>>;
/// Maximum conversation senders kept in memory (LRU eviction beyond this).
const MAX_CONVERSATION_SENDERS: usize = 1000;
/// Maximum history messages to keep per sender.
const MAX_CHANNEL_HISTORY: usize = 50;
/// Minimum user-message length (in chars) for auto-save to memory.
/// Messages shorter than this (e.g. "ok", "thanks") are not stored,
/// reducing noise in memory recall.
const AUTOSAVE_MIN_MESSAGE_CHARS: usize = 20;
const WHATSAPP_OBSERVED_GROUP_MESSAGE_LABEL: &str = "Observed WhatsApp group message";
const WHATSAPP_CURRENT_GROUP_MESSAGE_LABEL: &str = "Current WhatsApp group message";

// System prompt functions live in `zeroclaw_runtime::agent::system_prompt`.
#[allow(unused_imports)]
pub use zeroclaw_runtime::agent::system_prompt::{
    BOOTSTRAP_MAX_CHARS, build_system_prompt, build_system_prompt_with_mode,
    build_system_prompt_with_mode_and_autonomy,
};

const DEFAULT_CHANNEL_INITIAL_BACKOFF_SECS: u64 = 2;
const DEFAULT_CHANNEL_MAX_BACKOFF_SECS: u64 = 60;
const MIN_CHANNEL_MESSAGE_TIMEOUT_SECS: u64 = 30;
#[cfg(test)]
const CHANNEL_MESSAGE_TIMEOUT_SECS: u64 = 300;
/// Cap timeout scaling so large max_tool_iterations values do not create unbounded waits.
const CHANNEL_MESSAGE_TIMEOUT_SCALE_CAP: u64 = 4;
const CHANNEL_MIN_IN_FLIGHT_MESSAGES: usize = 8;
const CHANNEL_MAX_IN_FLIGHT_MESSAGES: usize = 64;
const CHANNEL_TYPING_REFRESH_INTERVAL_SECS: u64 = 4;
const CHANNEL_HEALTH_HEARTBEAT_SECS: u64 = 30;
const CHANNEL_HISTORY_COMPACT_KEEP_MESSAGES: usize = 12;
const CHANNEL_HISTORY_COMPACT_CONTENT_CHARS: usize = 600;
/// Proactive context-window budget in estimated characters (~4 chars/token).
/// Guardrail for hook-modified outbound channel content.
const CHANNEL_HOOK_MAX_OUTBOUND_CHARS: usize = 20_000;

type ProviderCacheMap = Arc<Mutex<HashMap<String, Arc<dyn ModelProvider>>>>;
type RouteSelectionMap = Arc<Mutex<HashMap<String, ChannelRouteSelection>>>;
type ThinkingOverrideMap = Arc<Mutex<HashMap<String, ThinkingLevel>>>;
/// Session-only model overrides scoped above the per-sender [`RouteSelectionMap`].
/// Keyed by a `scope_override_key` (prefixed `user::`/`agent::`), so both
/// scopes share one in-memory map. Never persisted — lost on restart by design.
type ScopedRouteMap = Arc<Mutex<HashMap<String, ChannelRouteSelection>>>;

fn effective_channel_message_timeout_secs(configured: u64) -> u64 {
    configured.max(MIN_CHANNEL_MESSAGE_TIMEOUT_SECS)
}

#[cfg(test)]
fn channel_message_timeout_budget_secs(
    message_timeout_secs: u64,
    max_tool_iterations: usize,
) -> u64 {
    channel_message_timeout_budget_secs_with_cap(
        message_timeout_secs,
        max_tool_iterations,
        CHANNEL_MESSAGE_TIMEOUT_SCALE_CAP,
    )
}

fn channel_message_timeout_budget_secs_with_cap(
    message_timeout_secs: u64,
    max_tool_iterations: usize,
    scale_cap: u64,
) -> u64 {
    let iterations = max_tool_iterations.max(1) as u64;
    let scale = iterations.min(scale_cap);
    message_timeout_secs.saturating_mul(scale)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChannelRouteSelection {
    model_provider: String,
    model: String,
    /// Route-specific API key override. When set, this credential is passed
    /// directly to the requested provider instead of the alias entry's key.
    api_key: Option<String>,
}

#[derive(Debug, Clone)]
struct ChannelRuntimeDefaults {
    default_model_provider: String,
    model: String,
    temperature: Option<f64>,
    api_key: Option<String>,
    api_url: Option<String>,
    reliability: zeroclaw_config::schema::ReliabilityConfig,
}

#[derive(Debug, Clone)]
struct ChannelRuntimeDefaultsSnapshot {
    config: Arc<Config>,
    defaults: ChannelRuntimeDefaults,
    hot: bool,
    generation: u64,
}

#[derive(Debug, Clone)]
struct ChannelRuntimeOverride {
    config: Arc<Config>,
    defaults: ChannelRuntimeDefaults,
    generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ConfigFileStamp {
    modified: SystemTime,
    len: u64,
}

const SYSTEMD_STATUS_ARGS: [&str; 3] = ["--user", "is-active", "zeroclaw.service"];
const SYSTEMD_RESTART_ARGS: [&str; 3] = ["--user", "restart", "zeroclaw.service"];
const OPENRC_STATUS_ARGS: [&str; 2] = ["zeroclaw", "status"];
const OPENRC_RESTART_ARGS: [&str; 2] = ["zeroclaw", "restart"];

#[derive(Clone, Copy)]
#[allow(clippy::struct_excessive_bools)]
struct InterruptOnNewMessageConfig {
    telegram: bool,
    slack: bool,
    discord: bool,
    mattermost: bool,
    matrix: bool,
    whatsapp: bool,
}

impl InterruptOnNewMessageConfig {
    fn enabled_for_channel(self, channel: &str) -> bool {
        match channel {
            "telegram" => self.telegram,
            "slack" => self.slack,
            "discord" => self.discord,
            "mattermost" => self.mattermost,
            "matrix" => self.matrix,
            "whatsapp" => self.whatsapp,
            _ => false,
        }
    }
}

fn interrupt_on_new_message_config(
    channels: &zeroclaw_config::schema::ChannelsConfig,
) -> InterruptOnNewMessageConfig {
    InterruptOnNewMessageConfig {
        telegram: channels
            .telegram
            .get("default")
            .is_some_and(|tg| tg.interrupt_on_new_message),
        slack: channels
            .slack
            .get("default")
            .is_some_and(|sl| sl.interrupt_on_new_message),
        discord: channels
            .discord
            .get("default")
            .is_some_and(|dc| dc.interrupt_on_new_message),
        mattermost: channels
            .mattermost
            .get("default")
            .is_some_and(|mm| mm.interrupt_on_new_message),
        matrix: channels
            .matrix
            .get("default")
            .is_some_and(|mx| mx.interrupt_on_new_message),
        whatsapp: channels
            .whatsapp
            .get("default")
            .is_some_and(|wa| wa.interrupt_on_new_message),
    }
}

#[derive(Clone)]
struct ChannelCostTrackingState {
    tracker: Arc<zeroclaw_runtime::cost::CostTracker>,
    model_provider_pricing: Arc<zeroclaw_runtime::agent::cost::ModelProviderPricing>,
    agent_alias: Arc<String>,
}

#[derive(Clone)]
struct ChannelRuntimeContext {
    channels_by_name: Arc<HashMap<String, Arc<dyn Channel>>>,
    model_provider: Arc<dyn ModelProvider>,
    model_provider_ref: Arc<String>,
    /// Alias of the agent that owns this runtime context. Stamped onto
    /// every per-message tracing span so descendant events inherit the
    /// attribution without each call site re-passing it.
    agent_alias: Arc<String>,
    /// Resolved aliased-agent config for the agent owning this
    /// runtime context. Per-channel agent dispatch (one agent per
    /// channel.`<type>`.`<alias>`) is a follow-up.
    agent_cfg: Arc<zeroclaw_config::schema::AliasedAgentConfig>,
    prompt_config: Arc<zeroclaw_config::schema::Config>,
    memory: Arc<dyn Memory>,
    memory_strategy: Arc<dyn MemoryStrategy>,
    /// Companion PortableKernel store. Shared across agents; sibling of
    /// `memory_strategy`, not inside `TachiMemory`.
    companion_store: Option<Arc<zeroclaw_memory::CompanionStore>>,
    /// Local User Model authority store (#51): owner values/goals/
    /// preferences projected into turn prompts. `None` disables projection.
    user_model: Option<Arc<zeroclaw_memory::companion::UserModelStore>>,
    /// Session-scoped task preferences (#51 slice 4): overrides that
    /// expire with their session and never enter the durable store.
    task_prefs: Arc<TaskPreferenceOverlay>,
    tools_registry: Arc<Vec<Box<dyn Tool>>>,
    observer: Arc<dyn Observer>,
    system_prompt: Arc<String>,
    model: Arc<String>,
    temperature: Option<f64>,
    auto_save_memory: bool,
    max_tool_iterations: usize,
    min_relevance_score: f64,
    conversation_histories: ConversationHistoryMap,
    pending_new_sessions: PendingNewSessionSet,
    provider_cache: ProviderCacheMap,
    route_overrides: RouteSelectionMap,
    thinking_overrides: ThinkingOverrideMap,
    /// Session-only `/model` overrides scoped by user/agent (see
    /// [`ScopedRouteMap`]). Consulted above `route_overrides` in
    /// [`get_route_selection`]; never persisted.
    scope_overrides: ScopedRouteMap,
    reliability: Arc<zeroclaw_config::schema::ReliabilityConfig>,
    provider_runtime_options: zeroclaw_providers::ModelProviderRuntimeOptions,
    workspace_dir: Arc<PathBuf>,
    message_timeout_secs: u64,
    interrupt_on_new_message: InterruptOnNewMessageConfig,
    multimodal: zeroclaw_config::schema::MultimodalConfig,
    media_pipeline: zeroclaw_config::schema::MediaPipelineConfig,
    transcription_config: zeroclaw_config::schema::TranscriptionConfig,
    /// Resolved per-agent transcription provider alias (`<type>.<alias>`)
    /// for the runtime-active agent that owns this channel context.
    /// Empty when the agent has no transcription_provider set; downstream
    /// `TranscriptionManager.transcribe` calls then fail loud.
    agent_transcription_provider: String,
    hooks: Option<Arc<zeroclaw_runtime::hooks::HookRunner>>,
    non_cli_excluded_tools: Arc<Vec<String>>,
    autonomy_level: AutonomyLevel,
    tool_call_dedup_exempt: Arc<Vec<String>>,
    model_routes: Arc<Vec<zeroclaw_config::schema::ModelRouteConfig>>,
    query_classification: zeroclaw_config::schema::QueryClassificationConfig,
    ack_reactions: bool,
    show_tool_calls: bool,
    session_store: Option<Arc<dyn zeroclaw_infra::session_backend::SessionBackend>>,
    /// Non-interactive approval manager for channel-driven runs.
    /// Enforces `auto_approve` / `always_ask` / supervised policy from
    /// `[autonomy]` config; auto-denies tools that would need interactive
    /// approval since no operator is present on channel runs.
    approval_manager: Arc<ApprovalManager>,
    activated_tools:
        Option<std::sync::Arc<std::sync::Mutex<zeroclaw_runtime::tools::ActivatedToolSet>>>,
    cost_tracking: Option<ChannelCostTrackingState>,
    pacing: zeroclaw_config::schema::PacingConfig,
    max_tool_result_chars: usize,
    context_token_budget: usize,
    debouncer: Arc<zeroclaw_infra::debounce::MessageDebouncer>,
    /// HMAC receipt generator. `Some` when `[agent.resolved.tool_receipts] enabled = true`.
    /// Threaded into `run_tool_call_loop` so `tool_execution::execute_one_tool`
    /// can sign each result.
    receipt_generator: Option<zeroclaw_runtime::agent::tool_receipts::ReceiptGenerator>,
    /// Mirror of `[agent.resolved.tool_receipts] show_in_response`. When true,
    /// `process_channel_message` renders the per-turn collector as a trailing
    /// `Tool receipts:` block sent after the main reply.
    show_receipts_in_response: bool,
    last_applied_config_stamp: Arc<Mutex<Option<ConfigFileStamp>>>,
    runtime_defaults_override: Arc<Mutex<Option<Arc<ChannelRuntimeOverride>>>>,
    /// Per-conversation-history-key locks that serialize persistence mutations
    /// (append / remove_last / delete_session) for the same sender without
    /// serializing the full message-processing loop.
    persist_locks: Arc<std::sync::Mutex<HashMap<String, Arc<std::sync::Mutex<()>>>>>,
}

impl ChannelRuntimeContext {
    /// Companion PortableKernel handle injected from the composition root.
    pub(crate) fn companion_store(&self) -> Option<&Arc<zeroclaw_memory::CompanionStore>> {
        self.companion_store.as_ref()
    }

    pub(crate) fn user_model(&self) -> Option<&Arc<zeroclaw_memory::companion::UserModelStore>> {
        self.user_model.as_ref()
    }

    pub(crate) fn task_prefs(&self) -> &TaskPreferenceOverlay {
        &self.task_prefs
    }

    fn persist_companion_capture(&self, msg: &ChannelMessage, session_id: &str, turn_id: &str) {
        let Some(store) = self.companion_store.as_ref() else {
            return;
        };
        let owner = self.prompt_config.companion_memory.owner.gate();
        let _ = zeroclaw_memory::capture_channel_turn(
            Some(store.as_ref()),
            self.agent_alias.as_str(),
            session_id,
            turn_id,
            msg.channel.as_str(),
            msg.sender.as_str(),
            &owner,
        );
    }
}

/// Acquire the per-conversation-history-key persistence lock so that
/// append/remove_last/delete_session operations for the same sender are
/// serialized without blocking the full message-processing loop
fn acquire_persist_lock(ctx: &ChannelRuntimeContext, key: &str) -> Arc<std::sync::Mutex<()>> {
    let mut map = ctx.persist_locks.lock().unwrap_or_else(|e| e.into_inner());
    map.entry(key.to_string())
        .or_insert_with(|| Arc::new(std::sync::Mutex::new(())))
        .clone()
}

#[derive(Clone)]
struct InFlightSenderTaskState {
    task_id: u64,
    cancellation: CancellationToken,
    completion: Arc<InFlightTaskCompletion>,
}

struct InFlightTaskCompletion {
    done: AtomicBool,
    notify: tokio::sync::Notify,
}

impl InFlightTaskCompletion {
    fn new() -> Self {
        Self {
            done: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }

    fn mark_done(&self) {
        self.done.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    async fn wait(&self) {
        if self.done.load(Ordering::Acquire) {
            return;
        }
        self.notify.notified().await;
    }
}

fn conversation_memory_key(msg: &zeroclaw_api::channel::ChannelMessage) -> String {
    // Include thread_ts for per-topic memory isolation in forum groups
    let raw = match &msg.thread_ts {
        Some(tid) => format!("{}_{}_{}_{}", msg.channel, tid, msg.sender, msg.id),
        None => format!("{}_{}_{}", msg.channel, msg.sender, msg.id),
    };
    sanitize_session_key(&raw)
}

/// The channel prefix used in session/route keys: the channel type plus the
/// zeroclaw alias when present, so two bots on the same platform (e.g.
/// `discord.clamps` + `discord.glados`) never share a keyspace.
fn channel_scope(msg: &zeroclaw_api::channel::ChannelMessage) -> String {
    match &msg.channel_alias {
        Some(alias) => format!("{}.{}", msg.channel, alias),
        None => msg.channel.clone(),
    }
}

pub fn conversation_history_key(msg: &zeroclaw_api::channel::ChannelMessage) -> String {
    let channel_scope = channel_scope(msg);
    let thread_scope = match msg.thread_ts.as_deref() {
        // Matrix thread_ts is a delivery anchor, not a topic boundary: root
        // and follow-ups must share one sender+room session.
        Some(_) if is_matrix_channel_name(&msg.channel) => None,
        other => other,
    };
    let raw = match (msg.conversation_scope, thread_scope) {
        (zeroclaw_api::channel::ChannelConversationScope::ReplyTarget, _) => {
            format!("{channel_scope}_{}", msg.reply_target)
        }
        (zeroclaw_api::channel::ChannelConversationScope::Sender, Some(tid)) => {
            format!("{channel_scope}_{}_{tid}_{}", msg.reply_target, msg.sender)
        }
        (zeroclaw_api::channel::ChannelConversationScope::Sender, None) => {
            format!("{channel_scope}_{}_{}", msg.reply_target, msg.sender)
        }
    };
    sanitize_session_key(&raw)
}

fn scope_override_key(
    scope: OverrideScope,
    msg: &zeroclaw_api::channel::ChannelMessage,
    agent_alias: &str,
) -> String {
    let raw = match scope {
        OverrideScope::User => format!("user::{}::{}", channel_scope(msg), msg.sender),
        OverrideScope::Agent => format!("agent::{agent_alias}"),
    };
    sanitize_session_key(&raw)
}

fn followup_thread_id(msg: &zeroclaw_api::channel::ChannelMessage) -> Option<String> {
    if is_matrix_channel_name(&msg.channel) {
        msg.thread_ts.clone()
    } else {
        msg.thread_ts.clone().or_else(|| Some(msg.id.clone()))
    }
}

fn interruption_scope_key(msg: &zeroclaw_api::channel::ChannelMessage) -> String {
    match (msg.conversation_scope, msg.interruption_scope_id.as_deref()) {
        (zeroclaw_api::channel::ChannelConversationScope::ReplyTarget, Some(scope)) => {
            sanitize_session_key(&format!("{}_{}", channel_scope(msg), scope))
        }
        (zeroclaw_api::channel::ChannelConversationScope::ReplyTarget, None) => {
            sanitize_session_key(&format!("{}_{}", channel_scope(msg), msg.reply_target))
        }
        (zeroclaw_api::channel::ChannelConversationScope::Sender, Some(scope)) => format!(
            "{}_{}_{}_{}",
            msg.channel, msg.reply_target, msg.sender, scope
        ),
        (zeroclaw_api::channel::ChannelConversationScope::Sender, None) => {
            format!("{}_{}_{}", msg.channel, msg.reply_target, msg.sender)
        }
    }
}

/// Returns `true` when `content` is a `/stop` command (with optional `@botname` suffix).
/// Not gated on channel type — all non-CLI channels support `/stop`.
fn is_stop_command(content: &str) -> bool {
    let trimmed = content.trim();
    if !trimmed.starts_with('/') {
        return false;
    }
    let cmd = trimmed.split_whitespace().next().unwrap_or("");
    let base = cmd.split('@').next().unwrap_or(cmd);
    base.eq_ignore_ascii_case("/stop")
}

/// Splice a claimed background-announcement block above this turn's user
/// message, in place.
///
/// Shape mirrors the runtime's own claim sites (`loop_.rs`'s
/// `format!("{context}[{now}] {msg}")`): the block first, then the user's text,
/// with no separator of our own — the block carries its own trailing newline
/// from `claim_child_announcements_context`.
///
/// **Only the last message, and only when it is the user turn.** That is this
/// module's existing convention for "the message this turn is about": the
/// turn-context preamble is composed onto `history.last_mut()` under the same
/// `role == "user"` test, and the runtime's claim sites all splice into a user
/// message they build as the final one. Reaching further back would put the
/// block above text the model reads earlier, out of order with the news it
/// describes.
///
/// Returns whether the block landed. `false` means the model will never read
/// it, and the caller must let its `UnclaimOnDrop` guard drop armed so the
/// announcements go back to the store for a later turn. Takes a slice rather
/// than a `Vec` on purpose: there is no shape in which pushing a new message
/// here is right, so the signature refuses it.
fn prepend_context_to_last_user_turn(history: &mut [ChatMessage], block: &str) -> bool {
    if block.is_empty() {
        return false;
    }
    match history.last_mut() {
        Some(last) if last.role == "user" => {
            last.content = format!("{block}{}", last.content);
            true
        }
        _ => false,
    }
}

/// How a channel turn ended. Three levels because this turn shape separates
/// cancellation from timeout from tool-loop failure, and the three answer the
/// announcement question differently.
///
/// Module scope rather than a local inside `process_channel_message_body`
/// (where it used to live) because
/// [`run_channel_turn_with_background_announcements`] returns it and the tests
/// that pin the bracket's settle behaviour have to construct it — a
/// function-local type is reachable from neither.
enum LlmExecutionResult {
    Completed(Result<Result<String, anyhow::Error>, tokio::time::error::Elapsed>),
    Cancelled,
}

/// This turn shape's answer to the one question that decides whether its
/// claimed announcements stay delivered (`TurnOutcome`, `agent/loop_.rs`).
///
/// Only the fully nested `Completed(Ok(Ok(_)))` counts, and each layer it
/// rejects is a case where the model may never have seen the block:
/// `Cancelled` (the select fired before or during the call),
/// `Completed(Err(_))` (the whole tool loop timed out), and
/// `Completed(Ok(Err(_)))` (it failed — including failing before the
/// provider call). Flattening this to "is it ok" would keep announcements
/// nobody read flagged delivered-to-nobody.
impl TurnOutcome for LlmExecutionResult {
    fn turn_succeeded(&self) -> bool {
        matches!(self, LlmExecutionResult::Completed(Ok(Ok(_))))
    }
}

/// What [`run_channel_turn_with_background_announcements`] needs of a claim
/// guard: settle it exactly once, against this turn's outcome, and let it drop
/// still armed on every path that does not.
///
/// The bracket is generic over this rather than over `UnclaimOnDrop` for one
/// reason: `UnclaimOnDrop` can only be minted by a real claim, and a real claim
/// in this crate's tests yields nothing. `claim_announcements_for_scoped_turn`
/// resolves its store through `control_plane()`
/// (`zeroclaw-runtime/src/control_plane/global.rs`), a `OnceLock` only the
/// daemon boots, and the bypass hook for it (`CHILD_ANNOUNCEMENT_STORE_TEST_HOOK`,
/// `agent/loop_.rs`) is `#[cfg(test)]`-private to `zeroclaw-runtime`, so it does
/// not exist when that crate is compiled as this one's dependency. A test here
/// can therefore only ever observe an empty claim and no guard. Abstracting the
/// guard is what lets a stub claim hand the bracket something whose settling is
/// observable.
trait ChannelAnnouncementGuard {
    /// Settle against how the turn ended. The judgement is
    /// [`TurnOutcome::turn_succeeded`]'s, never this call's.
    fn settle_against(self, outcome: &LlmExecutionResult);
}

/// The production guard settles through the runtime's own function, so the
/// criterion stays the one spelled in `agent/loop_.rs` and is not restated here.
impl ChannelAnnouncementGuard for zeroclaw_runtime::agent::UnclaimOnDrop {
    fn settle_against(self, outcome: &LlmExecutionResult) {
        settle_announcement_guards(Some(self), outcome);
    }
}

/// The channel turn's background-announcement bracket: claim under the
/// conversation's history key, splice the block above the user message, run the
/// turn, settle the claim against how it ended.
///
/// This exists as a seam, not as decomposition.
/// `process_channel_message_body` needs a whole live orchestrator context
/// (providers, registries, channel handles, approval manager) that no test
/// constructs, so with the wiring inline the only thing that could pin it was a
/// test that read this file's own source text for literals — which cannot catch
/// a wrong key, a wrong history shape, or a splice that permanently returns
/// `false`. Taking the turn's execution body as a parameter moves all three
/// under behavioural test: production passes its model-switch retry loop
/// unchanged, a test passes a stub that returns a constructed
/// [`LlmExecutionResult`] and inspects, from inside the stub, exactly the
/// `history` the model would have been given.
///
/// **`history` is `&mut` and reaches the body only after the splice.** That
/// ordering is the contract — the body is handed the same vector the splice
/// wrote into, so there is no shape in which the model reads a history the
/// splice did not touch.
///
/// **A failed splice disarms before the body runs.** Nothing was put in front
/// of the model, so the rows go back to the store and a later turn announces
/// them again. This is reachable, not theoretical: a cache whose tail is a
/// `tool` message — an interrupted tool-calling turn, persisted before its
/// assistant reply — makes `normalize_cached_channel_turns` merge this turn's
/// user content *into* that tool message, so the last role is `tool` and both
/// this splice and the turn-context preamble no-op. It costs one turn, not the
/// announcement.
///
/// **Settling happens once, outside the body.** A model-switch retry loops with
/// the same history, which the model has still not read, so a body that retries
/// internally settles nothing per attempt; it yields one outcome and that is
/// what the claim is judged by.
async fn run_channel_turn_with_background_announcements<Guard, Claim, Body>(
    history_key: &str,
    history: &mut Vec<ChatMessage>,
    claim: Claim,
    turn_body: Body,
) -> LlmExecutionResult
where
    Guard: ChannelAnnouncementGuard,
    Claim: AsyncFnOnce(&str) -> (String, Option<Guard>),
    Body: AsyncFnOnce(&mut Vec<ChatMessage>) -> LlmExecutionResult,
{
    let (announcements, mut guard) = claim(history_key).await;
    if !prepend_context_to_last_user_turn(history, &announcements) {
        // Nothing was spliced, so the model will never read these. Drop the
        // guard armed right here, before the turn even starts.
        guard = None;
    }

    let outcome = turn_body(history).await;

    if let Some(guard) = guard {
        guard.settle_against(&outcome);
    }
    outcome
}

fn timestamp_channel_user_content(content: &str) -> String {
    let now = chrono::Local::now();
    format!("[{}] {}", now.format("%Y-%m-%d %H:%M:%S %Z"), content)
}

fn format_whatsapp_group_history_turn(label: &str, sender: &str, content: &str) -> String {
    let sender = sender.trim();
    if sender.is_empty() {
        format!("[{label}]\n{content}")
    } else {
        format!("[{label} from {sender}]\n{content}")
    }
}

fn attributed_whatsapp_group_user_turn(
    msg: &zeroclaw_api::channel::ChannelMessage,
    label: &str,
    content: &str,
) -> String {
    if msg.channel == "whatsapp" && is_group_reply_target(&msg.reply_target) {
        format_whatsapp_group_history_turn(label, &msg.sender, content)
    } else {
        content.to_string()
    }
}

fn timestamped_channel_user_history_content(
    msg: &zeroclaw_api::channel::ChannelMessage,
    label: &str,
) -> String {
    let timestamped_content = timestamp_channel_user_content(&msg.content);
    attributed_whatsapp_group_user_turn(msg, label, &timestamped_content)
}

/// Collapse only heavy inline `data:` image payloads in historical turns while
/// preserving re-loadable `[IMAGE:<path>]` file references, so a later turn can
/// re-inflate from disk without re-sending megabytes of base64 every request.
/// File-path and placeholder markers pass through untouched.
fn collapse_inline_image_payloads(turns: &mut [ChatMessage]) {
    if turns.len() <= 1 {
        return;
    }
    let last_idx = turns.len() - 1;
    for turn in &mut turns[..last_idx] {
        if turn.role != "user" || !turn.content.contains("[IMAGE:data:") {
            continue;
        }
        let (_, refs) = zeroclaw_providers::multimodal::parse_image_markers(&turn.content);
        if refs.iter().any(|r| r.starts_with("data:")) {
            turn.content = strip_inline_data_image_markers(&turn.content);
        }
    }
}

fn strip_inline_data_image_markers(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut cursor = 0usize;
    while let Some(rel) = content[cursor..].find("[IMAGE:data:") {
        let start = cursor + rel;
        out.push_str(&content[cursor..start]);
        match content[start..].find(']') {
            Some(rel_end) => {
                out.push_str("[Image attachment omitted from history]");
                cursor = start + rel_end + 1;
            }
            None => {
                out.push_str(&content[start..]);
                cursor = content.len();
                break;
            }
        }
    }
    if cursor < content.len() {
        out.push_str(&content[cursor..]);
    }
    out.trim().to_string()
}

fn normalize_cached_channel_turns(turns: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let mut normalized = Vec::with_capacity(turns.len());
    let mut expecting_user = true;

    for turn in turns {
        match (expecting_user, turn.role.as_str()) {
            // Pass through tool-role messages preserved by
            // keep_tool_context_turns.  After a tool result the
            // next expected message is an assistant response, same as
            // after a user message.
            (_, "tool") | (true, "user") => {
                normalized.push(turn);
                expecting_user = false;
            }
            (false, "assistant") => {
                normalized.push(turn);
                expecting_user = true;
            }
            // Interrupted channel turns can produce consecutive user messages
            // (no assistant persisted yet). Merge instead of dropping.
            (false, "user") | (true, "assistant") => {
                if let Some(last_turn) = normalized.last_mut()
                    && !turn.content.is_empty()
                {
                    if !last_turn.content.is_empty() {
                        last_turn.content.push_str("\n\n");
                    }
                    last_turn.content.push_str(&turn.content);
                }
            }
            _ => {}
        }
    }

    normalized
}

fn should_bypass_reply_intent_precheck(
    msg: &zeroclaw_api::channel::ChannelMessage,
    direct_message: bool,
) -> bool {
    msg.explicitly_addressed || direct_message
}

fn is_matrix_channel_name(channel_name: &str) -> bool {
    channel_name == "matrix" || channel_name.starts_with("matrix:")
}

struct ChannelThinkingResolution {
    effective_content: String,
    level: ThinkingLevel,
    params: zeroclaw_runtime::agent::thinking::ThinkingParams,
    effective_temperature: Option<f64>,
}

fn resolve_channel_thinking(
    content: &str,
    session_override: Option<ThinkingLevel>,
    config: &ThinkingConfig,
    base_temperature: Option<f64>,
) -> ChannelThinkingResolution {
    let (directive, effective_content) =
        match zeroclaw_runtime::agent::thinking::parse_thinking_directive(content) {
            Some((level, remaining)) => (Some(level), remaining),
            None => (None, content.to_string()),
        };
    let level = zeroclaw_runtime::agent::thinking::resolve_thinking_level(
        directive,
        session_override,
        config,
    );
    let params = zeroclaw_runtime::agent::thinking::apply_thinking_level_with_config(level, config);
    let effective_temperature = base_temperature.map(|temperature| {
        zeroclaw_runtime::agent::thinking::clamp_temperature(
            temperature + params.temperature_adjustment,
        )
    });

    ChannelThinkingResolution {
        effective_content,
        level,
        params,
        effective_temperature,
    }
}

fn resolved_runtime_model_provider_ref(
    config: &Config,
    agent_alias: &str,
) -> anyhow::Result<String> {
    let agent = config
        .agents
        .get(agent_alias)
        .with_context(|| format!("agents.{agent_alias} is not configured"))?;
    let configured = agent.model_provider.trim();
    if configured.is_empty() {
        anyhow::bail!(
            "agents.{agent_alias}.model_provider is empty; runtime reload requires a dotted `<type>.<alias>` provider reference"
        );
    }
    let (model_provider, _) = model_provider_entry_for_ref(config, configured)?;
    Ok(model_provider)
}

fn model_provider_entry_for_ref<'a>(
    config: &'a Config,
    model_provider: &str,
) -> anyhow::Result<(String, &'a zeroclaw_config::schema::ModelProviderConfig)> {
    let trimmed = model_provider.trim();
    if trimmed.is_empty() {
        anyhow::bail!("model_provider reference must not be empty");
    }

    let Some((provider_type, provider_alias)) = trimmed.split_once('.') else {
        anyhow::bail!("model_provider `{trimmed}` must use `<type>.<alias>` form");
    };
    let Some(entry) = config.providers.models.find(provider_type, provider_alias) else {
        anyhow::bail!("model_provider `{trimmed}` does not resolve to a configured provider");
    };
    Ok((trimmed.to_string(), entry))
}

/// Resolve runtime defaults from `config` against a specific dotted
/// `model_provider` reference (`"<type>.<alias>"`) — the per-agent
/// resolution path.
fn runtime_defaults_from_config(
    config: &Config,
    model_provider: &str,
) -> anyhow::Result<ChannelRuntimeDefaults> {
    let (default_model_provider, entry) = model_provider_entry_for_ref(config, model_provider)?;
    let model = entry
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "model_provider": model_provider,
                        "reason": "no_model_configured",
                    })),
                "orchestrator: model_provider has no resolvable model"
            );
            anyhow::Error::msg(format!(
                "no model configured: model_provider '{model_provider}' does not resolve to a \
                 ModelProviderConfig with a `model` field, and providers.models has no \
                 fallback entry."
            ))
        })?;
    Ok(ChannelRuntimeDefaults {
        default_model_provider,
        model,
        temperature: entry.temperature,
        api_key: entry.api_key.clone(),
        api_url: entry.uri.clone(),
        reliability: config.reliability.clone(),
    })
}

fn runtime_config_path(ctx: &ChannelRuntimeContext) -> Option<PathBuf> {
    ctx.provider_runtime_options
        .zeroclaw_dir
        .as_ref()
        .map(|dir| dir.join("config.toml"))
}

fn runtime_defaults_snapshot(ctx: &ChannelRuntimeContext) -> ChannelRuntimeDefaultsSnapshot {
    if let Some(runtime_override) = ctx
        .runtime_defaults_override
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        return ChannelRuntimeDefaultsSnapshot {
            config: Arc::clone(&runtime_override.config),
            defaults: runtime_override.defaults.clone(),
            hot: true,
            generation: runtime_override.generation,
        };
    }

    ChannelRuntimeDefaultsSnapshot {
        config: Arc::clone(&ctx.prompt_config),
        defaults: ChannelRuntimeDefaults {
            default_model_provider: ctx.model_provider_ref.as_str().to_string(),
            model: ctx.model.as_str().to_string(),
            temperature: ctx.temperature,
            api_key: None,
            api_url: None,
            reliability: (*ctx.reliability).clone(),
        },
        hot: false,
        generation: 0,
    }
}

async fn config_file_stamp(path: &Path) -> Option<ConfigFileStamp> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    let modified = metadata.modified().ok()?;
    Some(ConfigFileStamp {
        modified,
        len: metadata.len(),
    })
}

async fn load_runtime_config_and_defaults(
    path: &Path,
    agent_alias: &str,
) -> Result<(Config, ChannelRuntimeDefaults)> {
    let contents = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let mut parsed: Config = zeroclaw_config::migration::migrate_to_current(&contents)
        .with_context(|| format!("Failed to migrate {}", path.display()))?;
    parsed.config_path = path.to_path_buf();

    if let Some(zeroclaw_dir) = path.parent() {
        let store =
            zeroclaw_runtime::security::SecretStore::new(zeroclaw_dir, parsed.secrets.encrypt);
        parsed.decrypt_secrets(&store)?;
    }
    let applied = zeroclaw_config::env_overrides::apply_env_overrides(&mut parsed)?;
    parsed.env_overridden_paths = applied.paths;
    parsed.pre_override_snapshots = applied.snapshots;
    // Same retired-surface tombstones as `Config::load_or_init`: env-prefix
    // hits plus any retired section still present in the file being
    // (re)loaded, so the live per-message reload path surfaces the same
    // structured warnings the startup path does.
    parsed.retired_surface_warnings =
        zeroclaw_config::validation_warnings::retired_section_tombstones(&contents)
            .into_iter()
            .chain(applied.tombstone_warnings)
            .collect();

    let model_provider = resolved_runtime_model_provider_ref(&parsed, agent_alias)?;
    let defaults = runtime_defaults_from_config(&parsed, &model_provider)?;
    Ok((parsed, defaults))
}

async fn maybe_apply_runtime_config_update(ctx: &ChannelRuntimeContext) -> Result<()> {
    let Some(config_path) = runtime_config_path(ctx) else {
        return Ok(());
    };

    let Some(stamp) = config_file_stamp(&config_path).await else {
        return Ok(());
    };

    {
        let last = ctx
            .last_applied_config_stamp
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if *last == Some(stamp) {
            return Ok(());
        }
    }

    let (next_config, next_defaults) =
        load_runtime_config_and_defaults(&config_path, ctx.agent_alias.as_str()).await?;
    let next_config = Arc::new(next_config);
    let next_options = zeroclaw_providers::options_for_provider_ref(
        next_config.as_ref(),
        &next_defaults.default_model_provider,
        &ctx.provider_runtime_options,
    );
    let model_provider_instance = zeroclaw_providers::create_resilient_model_provider_from_ref(
        next_config.as_ref(),
        &next_defaults.default_model_provider,
        next_defaults.api_key.as_deref(),
        next_defaults.api_url.as_deref(),
        &next_defaults.reliability,
        &next_options,
    )?;
    let model_provider_instance: Arc<dyn ModelProvider> = Arc::from(model_provider_instance);

    if let Err(err) = ProviderDispatch::from_ref(&*model_provider_instance)
        .warmup()
        .await
    {
        if zeroclaw_providers::reliable::is_non_retryable(&err) {
            ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"model_provider": next_defaults.default_model_provider, "model": next_defaults.model, "err": err.to_string()})), "Rejecting config reload: model not available (non-retryable)");
            return Ok(());
        }
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(
                    ::serde_json::json!({"model_provider": next_defaults.default_model_provider, "err": err.to_string()})
                ),
            "ModelProvider warmup failed after config reload (retryable, applying anyway)"
        );
    }

    {
        let mut override_guard = ctx
            .runtime_defaults_override
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let next_generation = override_guard.as_ref().map_or(1, |runtime_override| {
            runtime_override.generation.saturating_add(1)
        });
        let next_override = Arc::new(ChannelRuntimeOverride {
            config: Arc::clone(&next_config),
            defaults: next_defaults.clone(),
            generation: next_generation,
        });
        let cache_key =
            provider_cache_key(&next_defaults.default_model_provider, None, next_generation);

        let mut cache = ctx.provider_cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.clear();
        cache.insert(cache_key, Arc::clone(&model_provider_instance));
        *override_guard = Some(next_override);
    }

    *ctx.last_applied_config_stamp
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(stamp);

    ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"path": config_path.display().to_string(), "model_provider": next_defaults.default_model_provider, "model": next_defaults.model, "temperature": next_defaults.temperature, "agent_model_provider": next_defaults.default_model_provider})), "Applied updated channel runtime config from disk");

    Ok(())
}

fn default_route_selection_from_snapshot(
    defaults_snapshot: &ChannelRuntimeDefaultsSnapshot,
) -> ChannelRouteSelection {
    let defaults = defaults_snapshot.defaults.clone();
    ChannelRouteSelection {
        model_provider: defaults.default_model_provider,
        model: defaults.model,
        api_key: None,
    }
}

/// First scope override that matches `msg`, in precedence order
/// `User > Agent`. Session-only — never consults disk.
fn scope_override_lookup(
    ctx: &ChannelRuntimeContext,
    msg: &zeroclaw_api::channel::ChannelMessage,
) -> Option<ChannelRouteSelection> {
    let overrides = ctx
        .scope_overrides
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Hot path: nearly all deployments never set a scoped override, so avoid
    // building (and sanitizing) the per-scope keys on every message.
    if overrides.is_empty() {
        return None;
    }
    [OverrideScope::User, OverrideScope::Agent]
        .into_iter()
        .find_map(|scope| {
            overrides
                .get(&scope_override_key(scope, msg, ctx.agent_alias.as_str()))
                .cloned()
        })
}

fn get_route_selection(
    ctx: &ChannelRuntimeContext,
    msg: &zeroclaw_api::channel::ChannelMessage,
    sender_key: &str,
    defaults_snapshot: &ChannelRuntimeDefaultsSnapshot,
) -> ChannelRouteSelection {
    // Precedence (most specific wins): user > agent scope override,
    // then the per-sender route override, then the config default.
    scope_override_lookup(ctx, msg).unwrap_or_else(|| {
        ctx.route_overrides
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(sender_key)
            .cloned()
            .unwrap_or_else(|| default_route_selection_from_snapshot(defaults_snapshot))
    })
}

fn set_route_selection(
    ctx: &ChannelRuntimeContext,
    sender_key: &str,
    next: ChannelRouteSelection,
    defaults_snapshot: &ChannelRuntimeDefaultsSnapshot,
) {
    let default_route = default_route_selection_from_snapshot(defaults_snapshot);
    let mut routes = ctx
        .route_overrides
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if next == default_route {
        routes.remove(sender_key);
    } else {
        routes.insert(sender_key.to_string(), next);
    }
}

fn apply_model_ref(
    sel: &mut ChannelRouteSelection,
    model_routes: &[zeroclaw_config::schema::ModelRouteConfig],
    model: &str,
) {
    if let Some(route) = model_routes
        .iter()
        .find(|r| r.model.eq_ignore_ascii_case(model) || r.hint.eq_ignore_ascii_case(model))
    {
        sel.model_provider = route.model_provider.clone();
        sel.model = route.model.clone();
        sel.api_key = route.api_key.clone();
    } else {
        sel.model = model.to_string();
    }
}

fn shadow_note(
    ctx: &ChannelRuntimeContext,
    msg: &zeroclaw_api::channel::ChannelMessage,
    sender_key: &str,
    defaults_snapshot: &ChannelRuntimeDefaultsSnapshot,
    wrote: &ChannelRouteSelection,
) -> String {
    let effective = get_route_selection(ctx, msg, sender_key, defaults_snapshot);
    if effective.model == wrote.model && effective.model_provider == wrote.model_provider {
        String::new()
    } else {
        format!(
            "\n{}",
            channel_runtime_cli_string_with_args(
                "channel-runtime-shadow-note",
                &[
                    ("model", effective.model.as_str()),
                    ("provider", effective.model_provider.as_str()),
                ],
            )
        )
    }
}

/// Write (or clear) a session-only scope override. Returns `false` without
/// Write (or clear) a session-only scope override. Setting a value equal to the
/// config default clears the override (mirrors [`set_route_selection`]).
fn set_scope_override(
    ctx: &ChannelRuntimeContext,
    scope: OverrideScope,
    msg: &zeroclaw_api::channel::ChannelMessage,
    next: ChannelRouteSelection,
    defaults_snapshot: &ChannelRuntimeDefaultsSnapshot,
) {
    let key = scope_override_key(scope, msg, ctx.agent_alias.as_str());
    let default_route = default_route_selection_from_snapshot(defaults_snapshot);
    let mut overrides = ctx
        .scope_overrides
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if next == default_route {
        overrides.remove(&key);
    } else {
        overrides.insert(key, next);
    }
}

/// Per-sender authorization for `/model --agent <model>`. Resolves live
/// from `Config::peer_groups` via `Config::channel_agent_scope_admins`;
/// no cache, no per-channel duplicate sender list (consistent with
/// `AGENTS.md` SINGLE SOURCE OF TRUTH). Default deny
/// (`RequireExplicit`); operators who want the prior behavior opt in
/// by marking one or more peer groups `admin_for_agent_scope = true`.
///
/// **Effective-on-restart semantics:** this gate reads
/// `ctx.prompt_config`, an `Arc<Config>` snapshot captured when the
/// runtime context was built. A `peer_groups` edit in `config.toml`
/// therefore takes effect on context rebuild / daemon restart, not on
/// the next command — same lifetime as the other `prompt_config`-backed
/// orchestrator helpers. (The `channel_external_peers` sibling reads a
/// live `RwLock` for inbound dispatch because the gateway constructs
/// fresh `peer_resolver` closures per alias; the orchestrator's runtime
/// context is built once at startup and uses the snapshot path.)
///
/// Matching routes through `crate::allowlist::is_user_allowed` so the
/// gate honors the same wildcard (`["*"]` admits anyone) and per-channel
/// peer-identity semantics every inbound channel uses, instead of a raw
/// `==` that ignores wildcard, case, and the leading `@` Telegram strips
/// before comparison. Both the configured peer list and the incoming
/// sender are normalized through [`normalize_peer_username`] (strip a
/// leading `@`, ASCII-lowercase) so an operator who writes
/// `external_peers = ["@user_1"]` is matched by an inbound `user_1`
/// sender — matching what every channel's inbound path does before
/// calling `is_user_allowed`.
fn is_agent_scope_authorized(
    ctx: &ChannelRuntimeContext,
    msg: &zeroclaw_api::channel::ChannelMessage,
) -> bool {
    let channel_type = msg.channel.as_str();
    let channel_alias = msg.channel_alias.as_deref().unwrap_or(msg.channel.as_str());
    let agent_alias = ctx.agent_alias.as_str();
    let admins: Vec<String> = ctx
        .prompt_config
        .channel_agent_scope_admins(channel_type, channel_alias, agent_alias)
        .into_iter()
        .map(|p| normalize_peer_username(&p))
        .collect();
    let sender = normalize_peer_username(msg.sender.as_str());
    crate::allowlist::is_user_allowed(&admins, &sender, crate::allowlist::Match::Sensitive)
}

/// Canonical peer-username form used by the agent-scope gate. Inbound
/// channels (Telegram: `Self::normalize_identity`; IRC: `Match::CaseInsensitive`;
/// Matrix: same) already collapse the inbound sender into a stripped /
/// case-folded identity before calling `allowlist::is_user_allowed`. The
/// gate must apply the same shape to the configured `external_peers`
/// list so an operator's `"@user_1"` / `"user_1"` / `"@Alice"` entries
/// all match the same channel-normalized sender identity.
///
/// Kept local to this module so any future per-channel nuance (E.164
/// phone, email domain) can be plumbed explicitly through
/// `allowlist::is_user_allowed_by` rather than overloading this helper.
fn normalize_peer_username(raw: &str) -> String {
    raw.trim_start_matches('@').to_ascii_lowercase()
}

fn clear_sender_history(ctx: &ChannelRuntimeContext, sender_key: &str) {
    ctx.conversation_histories
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .pop(sender_key);
}

fn mark_sender_for_new_session(ctx: &ChannelRuntimeContext, sender_key: &str) {
    ctx.pending_new_sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(sender_key.to_string());
}

fn take_pending_new_session(ctx: &ChannelRuntimeContext, sender_key: &str) -> bool {
    ctx.pending_new_sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(sender_key)
}

fn replace_available_skills_section(base_prompt: &str, refreshed_skills: &str) -> String {
    const SKILLS_HEADER: &str = "## Available Skills\n\n";
    const SKILLS_END: &str = "</available_skills>";
    const WORKSPACE_HEADER: &str = "## Workspace\n\n";

    if let Some(start) = base_prompt.find(SKILLS_HEADER)
        && let Some(rel_end) = base_prompt[start..].find(SKILLS_END)
    {
        let end = start + rel_end + SKILLS_END.len();
        let tail = base_prompt[end..]
            .strip_prefix("\n\n")
            .unwrap_or(&base_prompt[end..]);

        let mut refreshed = String::with_capacity(
            base_prompt.len().saturating_sub(end.saturating_sub(start))
                + refreshed_skills.len()
                + 2,
        );
        refreshed.push_str(&base_prompt[..start]);
        if !refreshed_skills.is_empty() {
            refreshed.push_str(refreshed_skills);
            refreshed.push_str("\n\n");
        }
        refreshed.push_str(tail);
        return refreshed;
    }

    if refreshed_skills.is_empty() {
        return base_prompt.to_string();
    }

    if let Some(workspace_start) = base_prompt.find(WORKSPACE_HEADER) {
        let mut refreshed = String::with_capacity(base_prompt.len() + refreshed_skills.len() + 2);
        refreshed.push_str(&base_prompt[..workspace_start]);
        refreshed.push_str(refreshed_skills);
        refreshed.push_str("\n\n");
        refreshed.push_str(&base_prompt[workspace_start..]);
        return refreshed;
    }

    format!("{base_prompt}\n\n{refreshed_skills}")
}

fn refreshed_new_session_system_prompt(ctx: &ChannelRuntimeContext) -> String {
    let refreshed_skills = zeroclaw_runtime::skills::skills_to_prompt_with_mode(
        &zeroclaw_runtime::skills::load_skills_for_agent(
            ctx.workspace_dir.as_ref(),
            ctx.prompt_config.as_ref(),
            ctx.agent_alias.as_ref(),
        ),
        ctx.workspace_dir.as_ref(),
        ctx.prompt_config
            .effective_skills_prompt_mode(ctx.agent_alias.as_str()),
    );
    replace_available_skills_section(ctx.system_prompt.as_str(), &refreshed_skills)
}

fn compact_sender_history(ctx: &ChannelRuntimeContext, sender_key: &str) -> bool {
    let mut histories = ctx
        .conversation_histories
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let Some(turns) = histories.get_mut(sender_key) else {
        return false;
    };

    if turns.is_empty() {
        return false;
    }

    let keep_from = turns
        .len()
        .saturating_sub(CHANNEL_HISTORY_COMPACT_KEEP_MESSAGES);
    let mut compacted = normalize_cached_channel_turns(turns[keep_from..].to_vec());

    for turn in &mut compacted {
        if turn.content.chars().count() > CHANNEL_HISTORY_COMPACT_CONTENT_CHARS {
            turn.content =
                truncate_with_ellipsis(&turn.content, CHANNEL_HISTORY_COMPACT_CONTENT_CHARS);
        }
    }

    if compacted.is_empty() {
        turns.clear();
        return false;
    }

    *turns = compacted;
    true
}

/// Number of most-recent turns whose tool-result payloads are kept at full size
/// when proactively trimming. The active exchange stays intact; only older
/// tool results are shrunk to a bounded extract.
fn append_sender_turn(ctx: &ChannelRuntimeContext, sender_key: &str, turn: ChatMessage) {
    // Serialize per-sender persistence to prevent interleaving across concurrent
    // workers that share the same conversation_history_key
    let persist_lock = acquire_persist_lock(ctx, sender_key);
    let _lock = persist_lock.lock().unwrap_or_else(|e| e.into_inner());

    // Persist to JSONL before adding to in-memory history.
    if let Some(ref store) = ctx.session_store
        && let Err(e) = store.append(sender_key, &turn)
    {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
            "Failed to persist session turn"
        );
    }

    // Use the user-configured max_history_messages (fall back to
    // MAX_CHANNEL_HISTORY when the config value is 0 or absent).
    let max_history = {
        let configured = ctx.agent_cfg.resolved.max_history_messages;
        if configured > 0 {
            configured
        } else {
            MAX_CHANNEL_HISTORY
        }
    };

    let mut histories = ctx
        .conversation_histories
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let turns = histories.get_or_insert_mut(sender_key.to_string(), Vec::new);
    turns.push(turn);
    while turns.len() > max_history {
        turns.remove(0);
    }
}

/// Extract tool-call (assistant with tool_call content) and tool-result
/// messages from the current turn in the LLM history, excluding the final
/// assistant text response.  "Current turn" = everything after the last
/// user-role message.
fn extract_current_turn_tool_messages(history: &[ChatMessage]) -> Vec<ChatMessage> {
    // Find the index of the last user message — tool messages for the
    // current turn come after it.
    let last_user_idx = history.iter().rposition(|m| m.role == "user").unwrap_or(0);

    let tail = &history[last_user_idx + 1..];
    if tail.is_empty() {
        return Vec::new();
    }

    // Everything except the very last assistant message (which is the
    // final text response that gets stored separately).
    let end = if tail.last().is_some_and(|m| m.role == "assistant") {
        tail.len() - 1
    } else {
        tail.len()
    };

    tail[..end]
        .iter()
        .filter(|m| m.role == "assistant" || m.role == "tool")
        .cloned()
        .collect()
}

fn rollback_orphan_user_turn(
    ctx: &ChannelRuntimeContext,
    sender_key: &str,
    expected_content: &str,
) -> bool {
    // Serialize per-sender persistence to prevent interleaving across concurrent
    // workers that share the same conversation_history_key
    let persist_lock = acquire_persist_lock(ctx, sender_key);
    let _lock = persist_lock.lock().unwrap_or_else(|e| e.into_inner());

    let mut histories = ctx
        .conversation_histories
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(turns) = histories.get_mut(sender_key) else {
        return false;
    };

    let should_pop = turns
        .last()
        .is_some_and(|turn| turn.role == "user" && turn.content == expected_content);
    if !should_pop {
        return false;
    }

    turns.pop();
    if turns.is_empty() {
        histories.pop(sender_key);
    }

    // Also remove the orphan turn from the persisted JSONL session store so
    // it doesn't resurface after a daemon restart
    if let Some(ref store) = ctx.session_store
        && let Err(e) = store.remove_last(sender_key)
    {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
            "Failed to rollback session store entry"
        );
    }

    true
}

fn should_rollback_failed_user_turn(error: &anyhow::Error) -> bool {
    if error
        .downcast_ref::<zeroclaw_providers::ProviderCapabilityError>()
        .is_some_and(|capability| capability.capability.eq_ignore_ascii_case("vision"))
    {
        return true;
    }

    zeroclaw_providers::reliable::is_non_retryable(error)
}

fn is_context_window_overflow_error(err: &anyhow::Error) -> bool {
    let lower = err.to_string().to_lowercase();
    [
        "exceeds the context window",
        "context window of this model",
        "maximum context length",
        "context length exceeded",
        "too many tokens",
        "token limit exceeded",
        "prompt is too long",
        "input is too long",
    ]
    .iter()
    .any(|hint| lower.contains(hint))
}

/// Build a cache key that includes the runtime-defaults generation, the
/// model_provider name, and, when a route-specific API key is supplied, a hash
/// of that key. Generation `0` is the immutable startup config, so its key shape
/// stays unchanged; hot-reload generations get isolated cache entries.
fn provider_cache_key(provider_name: &str, route_api_key: Option<&str>, generation: u64) -> String {
    let base = match route_api_key {
        Some(key) => {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            key.hash(&mut hasher);
            format!("{provider_name}@{:x}", hasher.finish())
        }
        None => provider_name.to_string(),
    };
    if generation == 0 {
        base
    } else {
        format!("g{generation}:{base}")
    }
}

fn provider_credentials_for_ref(
    config: &zeroclaw_config::schema::Config,
    provider_ref: &str,
) -> (Option<String>, Option<String>) {
    let Some((type_key, alias_key)) = provider_ref.trim().split_once('.') else {
        return (None, None);
    };
    config
        .providers
        .models
        .find(type_key, alias_key)
        .map_or((None, None), |entry| {
            (entry.api_key.clone(), entry.uri.clone())
        })
}

async fn get_or_create_provider(
    ctx: &ChannelRuntimeContext,
    provider_name: &str,
    route_api_key: Option<&str>,
    defaults_snapshot: &ChannelRuntimeDefaultsSnapshot,
) -> anyhow::Result<Arc<dyn ModelProvider>> {
    let cache_key = provider_cache_key(provider_name, route_api_key, defaults_snapshot.generation);

    if let Some(existing) = ctx
        .provider_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&cache_key)
        .cloned()
    {
        return Ok(existing);
    }

    let config = Arc::clone(&defaults_snapshot.config);
    let defaults = defaults_snapshot.defaults.clone();

    // Only return the pre-built startup default model_provider while the
    // current runtime defaults still match startup and there is no
    // route-specific credential override. Once config reload changes defaults,
    // the cache/store path above owns the live default provider.
    if route_api_key.is_none()
        && provider_name == defaults.default_model_provider.as_str()
        && provider_name == ctx.model_provider_ref.as_str()
        && !defaults_snapshot.hot
    {
        return Ok(Arc::clone(&ctx.model_provider));
    }
    let (entry_api_key, entry_api_url) =
        provider_credentials_for_ref(config.as_ref(), provider_name);
    let effective_api_key = route_api_key.map(ToString::to_string).or(entry_api_key);

    let model_provider = create_resilient_model_provider_nonblocking(
        config,
        provider_name,
        effective_api_key,
        entry_api_url,
        defaults.reliability,
        ctx.provider_runtime_options.clone(),
    )
    .await?;
    let model_provider: Arc<dyn ModelProvider> = Arc::from(model_provider);

    if let Err(err) = ProviderDispatch::from_ref(&*model_provider).warmup().await {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(
                    ::serde_json::json!({"model_provider": provider_name, "err": err.to_string()})
                ),
            "ModelProvider warmup failed"
        );
    }

    let mut cache = ctx.provider_cache.lock().unwrap_or_else(|e| e.into_inner());
    let cached = cache
        .entry(cache_key)
        .or_insert_with(|| Arc::clone(&model_provider));
    Ok(Arc::clone(cached))
}

async fn create_resilient_model_provider_nonblocking(
    config: Arc<zeroclaw_config::schema::Config>,
    provider_name: &str,
    api_key: Option<String>,
    api_url: Option<String>,
    reliability: zeroclaw_config::schema::ReliabilityConfig,
    provider_runtime_options: zeroclaw_providers::ModelProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn ModelProvider>> {
    let provider_name = provider_name.to_string();
    tokio::task::spawn_blocking(move || {
        let options = zeroclaw_providers::options_for_provider_ref(
            &config,
            &provider_name,
            &provider_runtime_options,
        );
        zeroclaw_providers::create_resilient_model_provider_from_ref(
            &config,
            &provider_name,
            api_key.as_deref(),
            api_url.as_deref(),
            &reliability,
            &options,
        )
    })
    .await
    .context("failed to join model_provider initialization task")?
}

/// Render the per-scope override ladder appended to `/model` (no args), so a
/// user can see what is set at each tier and the resolution precedence.
fn build_scope_override_summary(
    ctx: &ChannelRuntimeContext,
    msg: &zeroclaw_api::channel::ChannelMessage,
    defaults_snapshot: &ChannelRuntimeDefaultsSnapshot,
) -> String {
    let fmt_sel =
        |sel: &ChannelRouteSelection| format!("`{}` / `{}`", sel.model_provider, sel.model);
    let (user, agent) = {
        let overrides = ctx
            .scope_overrides
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let scope_line = |scope: OverrideScope| -> String {
            overrides
                .get(&scope_override_key(scope, msg, ctx.agent_alias.as_str()))
                .map(&fmt_sel)
                .unwrap_or_else(|| "—".to_string())
        };
        (
            scope_line(OverrideScope::User),
            scope_line(OverrideScope::Agent),
        )
    };
    let sender_key = conversation_history_key(msg);
    let session = ctx
        .route_overrides
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&sender_key)
        .map(fmt_sel)
        .unwrap_or_else(|| "—".to_string());
    let default = default_route_selection_from_snapshot(defaults_snapshot);
    let default = fmt_sel(&default);
    format!(
        "\n\n{}",
        channel_runtime_cli_string_with_args(
            "channel-runtime-scope-overrides-summary",
            &[
                ("user", user.as_str()),
                ("agent", agent.as_str()),
                ("session", session.as_str()),
                ("default", default.as_str()),
            ],
        )
    )
}

async fn handle_runtime_command_if_needed(
    ctx: &ChannelRuntimeContext,
    msg: &zeroclaw_api::channel::ChannelMessage,
    target_channel: Option<&Arc<dyn Channel>>,
) -> bool {
    let Some(command) = parse_runtime_command(&msg.channel, &msg.content) else {
        return false;
    };

    let Some(channel) = target_channel else {
        return true;
    };

    let sender_key = conversation_history_key(msg);
    let defaults_snapshot = runtime_defaults_snapshot(ctx);
    let mut current = get_route_selection(ctx, msg, &sender_key, &defaults_snapshot);

    let response = match command {
        ChannelRuntimeCommand::ShowProviders => build_providers_help_response(&current),
        ChannelRuntimeCommand::SetTaskPref(kind, semantic_key, statement) => {
            ctx.task_prefs()
                .set(&sender_key, kind, &semantic_key, &statement);
            channel_runtime_cli_string_with_args(
                "channel-runtime-task-pref-set",
                &[("kind", kind), ("statement", statement.as_str())],
            )
        }
        ChannelRuntimeCommand::InvalidTaskPref(_raw) => {
            channel_runtime_cli_string("channel-runtime-task-pref-invalid")
        }
        ChannelRuntimeCommand::SetProvider(raw_model_provider) => {
            match resolve_models_command(defaults_snapshot.config.as_ref(), &raw_model_provider) {
                ModelsCommandResolution::Resolved(provider_ref) => {
                    match get_or_create_provider(ctx, &provider_ref, None, &defaults_snapshot).await
                    {
                        Ok(_) => {
                            if provider_ref != current.model_provider {
                                current.model_provider = provider_ref.clone();
                                set_route_selection(
                                    ctx,
                                    &sender_key,
                                    current.clone(),
                                    &defaults_snapshot,
                                );
                            }

                            channel_runtime_cli_string_with_args(
                                "channel-runtime-set-provider-switched",
                                &[
                                    ("provider", provider_ref.as_str()),
                                    ("model", current.model.as_str()),
                                ],
                            )
                        }
                        Err(err) => {
                            let safe_err = zeroclaw_providers::sanitize_api_error(&err.to_string());
                            channel_runtime_cli_string_with_args(
                                "channel-runtime-set-provider-init-failed",
                                &[
                                    ("provider", provider_ref.as_str()),
                                    ("error", safe_err.as_str()),
                                ],
                            )
                        }
                    }
                }
                ModelsCommandResolution::Ambiguous { family, aliases } => {
                    let list = aliases
                        .iter()
                        .map(|a| format!("`{family}.{a}`"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    channel_runtime_cli_string_with_args(
                        "channel-runtime-provider-ambiguous",
                        &[("family", family.as_str()), ("list", list.as_str())],
                    )
                }
                ModelsCommandResolution::NoAlias(ref_or_family) => {
                    channel_runtime_cli_string_with_args(
                        "channel-runtime-provider-no-alias",
                        &[("provider", ref_or_family.as_str())],
                    )
                }
                ModelsCommandResolution::Unknown => channel_runtime_cli_string_with_args(
                    "channel-runtime-provider-unknown",
                    &[("provider", raw_model_provider.as_str())],
                ),
            }
        }
        ChannelRuntimeCommand::ShowModel => {
            let mut resp = build_models_help_response(
                &current,
                ctx.workspace_dir.as_path(),
                &ctx.model_routes,
            );
            resp.push_str(&build_scope_override_summary(ctx, msg, &defaults_snapshot));
            resp
        }
        ChannelRuntimeCommand::SetModelScoped(scope, raw_model) => {
            let model = raw_model.trim().trim_matches('`').to_string();
            if model.is_empty() {
                channel_runtime_cli_string("channel-runtime-scoped-model-empty")
            } else if scope == OverrideScope::Agent && !is_agent_scope_authorized(ctx, msg) {
                // Per-sender authorization gate for the `--agent` scope only.
                // `/model --user` is unaffected.
                let channel_alias = msg.channel_alias.as_deref().unwrap_or(msg.channel.as_str());
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "sender": msg.sender.as_str(),
                            "agent": ctx.agent_alias.as_str(),
                            "channel": msg.channel.as_str(),
                            "channel_alias": channel_alias,
                            "model_requested": model.as_str(),
                            "command": "/model --agent",
                        })),
                    "agent-scope /model override rejected"
                );
                zeroclaw_runtime::i18n::get_required_cli_string_with_args(
                    "channel-runtime-agent-scope-rejected",
                    &[
                        ("sender", msg.sender.as_str()),
                        ("agent", ctx.agent_alias.as_str()),
                        ("model", model.as_str()),
                    ],
                )
            } else {
                // Resolve provider+model the same way bare `/model` does, then
                // write it at the requested scope instead of the per-sender route.
                let mut next = current.clone();
                apply_model_ref(&mut next, &ctx.model_routes, &model);
                set_scope_override(ctx, scope, msg, next.clone(), &defaults_snapshot);
                if scope == OverrideScope::Agent {
                    let channel_alias =
                        msg.channel_alias.as_deref().unwrap_or(msg.channel.as_str());
                    ::zeroclaw_log::record!(
                        INFO,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Approve)
                            .with_outcome(::zeroclaw_log::EventOutcome::Success)
                            .with_attrs(::serde_json::json!({
                                "sender": msg.sender.as_str(),
                                "agent": ctx.agent_alias.as_str(),
                                "channel": msg.channel.as_str(),
                                "channel_alias": channel_alias,
                                "model_provider": next.model_provider.as_str(),
                                "model": next.model.as_str(),
                                "command": "/model --agent",
                            })),
                        "agent-scope /model override accepted"
                    );
                }
                let scope_label = channel_runtime_scope_label(scope);
                let mut resp = channel_runtime_cli_string_with_args(
                    "channel-runtime-scoped-model-switched",
                    &[
                        ("model", next.model.as_str()),
                        ("provider", next.model_provider.as_str()),
                        ("scope", scope_label.as_str()),
                    ],
                );
                resp.push_str(&shadow_note(
                    ctx,
                    msg,
                    &sender_key,
                    &defaults_snapshot,
                    &next,
                ));
                resp
            }
        }
        ChannelRuntimeCommand::SetModel(raw_model) => {
            let model = raw_model.trim().trim_matches('`').to_string();
            if model.is_empty() {
                channel_runtime_cli_string("channel-runtime-model-empty")
            } else {
                apply_model_ref(&mut current, &ctx.model_routes, &model);
                set_route_selection(ctx, &sender_key, current.clone(), &defaults_snapshot);

                let mut resp = channel_runtime_cli_string_with_args(
                    "channel-runtime-model-switched",
                    &[
                        ("model", current.model.as_str()),
                        ("provider", current.model_provider.as_str()),
                    ],
                );
                resp.push_str(&shadow_note(
                    ctx,
                    msg,
                    &sender_key,
                    &defaults_snapshot,
                    &current,
                ));
                resp
            }
        }
        ChannelRuntimeCommand::ShowConfig => {
            if msg.channel == "slack" {
                let blocks_json = build_config_block_kit(
                    &current,
                    ctx.workspace_dir.as_path(),
                    &ctx.model_routes,
                );
                // Use a magic prefix so SlackChannel::send() can detect Block Kit JSON.
                format!("__ZEROCLAW_BLOCK_KIT__{blocks_json}")
            } else {
                build_config_text_response(&current, ctx.workspace_dir.as_path(), &ctx.model_routes)
            }
        }
        ChannelRuntimeCommand::NewSession => {
            // Serialize per-sender persistence to prevent interleaving
            let persist_lock = acquire_persist_lock(ctx, &sender_key);
            let _lock = persist_lock.lock().unwrap_or_else(|e| e.into_inner());
            clear_sender_history(ctx, &sender_key);
            ctx.thinking_overrides
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&sender_key);
            if let Some(ref store) = ctx.session_store
                && let Err(e) = store.delete_session(&sender_key)
            {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(
                            ::serde_json::json!({"error": format!("{}", e), "sender_key": sender_key})
                        ),
                    "Failed to delete persisted session for"
                );
            }
            mark_sender_for_new_session(ctx, &sender_key);
            channel_runtime_cli_string("channel-runtime-new-session")
        }
        ChannelRuntimeCommand::SetThinking(level) => match level {
            Some(level) => {
                ctx.thinking_overrides
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(sender_key.clone(), level);
                channel_runtime_cli_string_with_args(
                    "channel-runtime-thinking-set",
                    &[("level", level.as_str())],
                )
            }
            None => {
                let removed = ctx
                    .thinking_overrides
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&sender_key)
                    .is_some();
                let default = ctx.agent_cfg.resolved.thinking.default_level.as_str();
                if removed {
                    channel_runtime_cli_string_with_args(
                        "channel-runtime-thinking-cleared",
                        &[("default", default)],
                    )
                } else {
                    channel_runtime_cli_string_with_args(
                        "channel-runtime-thinking-default",
                        &[("default", default)],
                    )
                }
            }
        },
        ChannelRuntimeCommand::InvalidThinking(raw) => channel_runtime_cli_string_with_args(
            "channel-runtime-thinking-invalid",
            &[("raw", raw.as_str())],
        ),
    };

    if let Err(err) = channel
        .send(&{
            let mut sm = SendMessage::new(response, &msg.reply_target)
                .in_thread(msg.thread_ts.clone())
                .in_reply_to(Some(msg.id.clone()));
            if let Some(ref subj) = msg.subject {
                let reply_subject = if subj.to_lowercase().starts_with("re:") {
                    subj.clone()
                } else {
                    format!("Re: {}", subj)
                };
                sm = sm.subject(reply_subject);
            }
            sm
        })
        .await
    {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            &format!(
                "Failed to send runtime command response on {}: {err}",
                channel.name()
            )
        );
    }

    true
}

fn is_group_reply_target(reply_target: &str) -> bool {
    reply_target.contains("@g.us") || reply_target.starts_with("group:")
}

fn sender_memory_session_ids(
    msg: &zeroclaw_api::channel::ChannelMessage,
    history_key: &str,
) -> Vec<String> {
    // Match the sanitized form persisted by memory backend migrations.
    let sanitized_sender = sanitize_session_key(&msg.sender);
    if is_group_reply_target(&msg.reply_target) {
        vec![sanitized_sender]
    } else {
        vec![history_key.to_string(), sanitized_sender]
    }
}

#[cfg(test)]
fn extract_tool_context_summary(history: &[ChatMessage], start_index: usize) -> String {
    fn push_unique_tool_name(tool_names: &mut Vec<String>, name: &str) {
        let candidate = name.trim();
        if candidate.is_empty() {
            return;
        }
        if !tool_names.iter().any(|existing| existing == candidate) {
            tool_names.push(candidate.to_string());
        }
    }

    fn collect_tool_names_from_tool_call_tags(content: &str, tool_names: &mut Vec<String>) {
        const TAG_PAIRS: [(&str, &str); 4] = [
            ("<tool_call>", "</tool_call>"),
            ("<toolcall>", "</toolcall>"),
            ("<tool-call>", "</tool-call>"),
            ("<invoke>", "</invoke>"),
        ];

        for (open_tag, close_tag) in TAG_PAIRS {
            for segment in content.split(open_tag) {
                if let Some(json_end) = segment.find(close_tag) {
                    let json_str = segment[..json_end].trim();
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str)
                        && let Some(name) = val.get("name").and_then(|n| n.as_str())
                    {
                        push_unique_tool_name(tool_names, name);
                    }
                }
            }
        }
    }

    fn collect_tool_names_from_native_json(content: &str, tool_names: &mut Vec<String>) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(content)
            && let Some(calls) = val.get("tool_calls").and_then(|c| c.as_array())
        {
            for call in calls {
                let name = call
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .or_else(|| call.get("name").and_then(|n| n.as_str()));
                if let Some(name) = name {
                    push_unique_tool_name(tool_names, name);
                }
            }
        }
    }

    fn collect_tool_names_from_tool_results(content: &str, tool_names: &mut Vec<String>) {
        let marker = "<tool_result name=\"";
        let mut remaining = content;
        while let Some(start) = remaining.find(marker) {
            let name_start = start + marker.len();
            let after_name_start = &remaining[name_start..];
            if let Some(name_end) = after_name_start.find('"') {
                let name = &after_name_start[..name_end];
                push_unique_tool_name(tool_names, name);
                remaining = &after_name_start[name_end + 1..];
            } else {
                break;
            }
        }
    }

    let mut tool_names: Vec<String> = Vec::new();

    for msg in history.iter().skip(start_index) {
        match msg.role.as_str() {
            "assistant" => {
                collect_tool_names_from_tool_call_tags(&msg.content, &mut tool_names);
                collect_tool_names_from_native_json(&msg.content, &mut tool_names);
            }
            "user" => {
                // Prompt-mode tool calls are always followed by [Tool results] entries
                // containing `<tool_result name="...">` tags with canonical tool names.
                collect_tool_names_from_tool_results(&msg.content, &mut tool_names);
            }
            _ => {}
        }
    }

    if tool_names.is_empty() {
        return String::new();
    }

    format!("[Used tools: {}]", tool_names.join(", "))
}

async fn classify_channel_reply_intent(
    model_provider: &dyn ModelProvider,
    system_prompt: &str,
    history: &[ChatMessage],
    model: &str,
    temperature: Option<f64>,
) -> anyhow::Result<AssistantChannelOutcome> {
    let mut convo = String::from(
        "Decide whether the assistant should send any visible reply to the latest inbound \
         channel message, and if not, which kind of non-reply it is.\n\nReturn exactly one of:\n\
         - `REPLY`\n\
         - `NO_REPLY[INFO]: <short reason>`   (informational/social, no action needed)\n\
         - `NO_REPLY[REFUSE]: <short reason>` (refused for safety, policy, or prompt injection)\n\
         - `NO_REPLY[FAIL]: <short reason>`   (tried but couldn't fulfil — bad URL, missing file, timeout)\n\
         - `NO_REPLY: <short reason>`         (legacy form; treated as INFO)\n\n\
         Rules:\n\
         - Any call to action from the user MUST be actioned — return `REPLY`. A call to action \
         is a question, request, command, or ask: a message that requires the assistant to do \
         or say something. Being merely named, addressed, or referenced is NOT a call to action \
         on its own (e.g. \"stand by\", \"hold on\", \"thanks bot\" — those are not asks). \
         There is no exception when a real ask is present: memory or prior history showing a \
         similar earlier exchange is NOT grounds to skip the response — the user asked now and \
         is owed a reply now.\n\
         - For everything that is not a call to action, default to `REPLY`. Only emit \
         `NO_REPLY[*]` when one of the categories below clearly applies; when in doubt, `REPLY`.\n\
         - `NO_REPLY[INFO]` is reserved for messages plainly not for the assistant: chatter \
         between other humans in a group channel, system broadcasts, or content the embedded \
         system prompt explicitly tells the assistant to ignore.\n\
         - Output exactly one of the tokens above; emit no other text. The `<short reason>` \
         describes the inbound message — it MUST NOT restate or paraphrase these classifier \
         instructions.\n\nConversation:\n",
    );

    for msg in history.iter().filter(|m| m.role != "system") {
        let role = match msg.role.as_str() {
            "assistant" => "assistant",
            _ => "user",
        };
        // Strip media markers — auxiliary classifier does not need image
        // content, and forwarding `[IMAGE:/local/path]` would reach the
        // provider as a malformed `image_url.url` and trigger 400 errors.
        let safe_content = zeroclaw_providers::multimodal::strip_media_markers(&msg.content);
        let _ = writeln!(convo, "[{role}] {safe_content}");
    }

    let response = ProviderDispatch::from_ref(model_provider)
        .chat_with_system(Some(system_prompt), &convo, model, temperature)
        .await?;
    Ok(parse_reply_intent(&response))
}

async fn resolve_classifier_route(
    ctx: &ChannelRuntimeContext,
    provider_ref: &zeroclaw_config::providers::ModelProviderRef,
    defaults_snapshot: &ChannelRuntimeDefaultsSnapshot,
) -> Option<(Arc<dyn ModelProvider>, String, Option<f64>)> {
    let provider_str = provider_ref.as_str().trim();
    if provider_str.is_empty() {
        return None;
    }

    let (type_key, alias_key) = match provider_str.split_once('.') {
        Some(parts) => parts,
        None => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"provider": provider_str})),
                "classifier_provider must be dotted `<type>.<alias>`; falling back to main agent"
            );
            return None;
        }
    };

    let model_cfg = match defaults_snapshot
        .config
        .providers
        .models
        .find(type_key, alias_key)
    {
        Some(cfg) => cfg,
        None => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"provider": provider_str})),
                "classifier_provider references an unknown [providers.models.<type>.<alias>] entry; falling back to main agent"
            );
            return None;
        }
    };

    let model = model_cfg.model.clone().unwrap_or_default();
    let temperature = model_cfg.temperature;
    if model.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"provider": provider_str})),
            "classifier_provider points to a [providers.models] entry without a `model` field; falling back to main agent"
        );
        return None;
    }

    let provider = match get_or_create_provider(
        ctx,
        provider_str,
        model_cfg.api_key.as_deref(),
        defaults_snapshot,
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            let safe_err = zeroclaw_providers::sanitize_api_error(&e.to_string());
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"provider": provider_str, "error": safe_err})),
                "Failed to initialize classifier_provider; falling back to main agent provider"
            );
            return None;
        }
    };

    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_attrs(::serde_json::json!({"provider": provider_str, "model": model.as_str()})),
        "classifier_provider override active"
    );

    Some((provider, model, temperature))
}

fn spawn_supervised_listener(
    ch: Arc<dyn Channel>,
    alias: Option<String>,
    tx: tokio::sync::mpsc::Sender<zeroclaw_api::channel::ChannelMessage>,
    initial_backoff_secs: u64,
    max_backoff_secs: u64,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    spawn_supervised_listener_with_health_interval(
        ch,
        alias,
        tx,
        initial_backoff_secs,
        max_backoff_secs,
        Duration::from_secs(CHANNEL_HEALTH_HEARTBEAT_SECS),
        cancel,
    )
}

fn spawn_supervised_listener_with_health_interval(
    ch: Arc<dyn Channel>,
    alias: Option<String>,
    tx: tokio::sync::mpsc::Sender<zeroclaw_api::channel::ChannelMessage>,
    initial_backoff_secs: u64,
    max_backoff_secs: u64,
    health_interval: Duration,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let health_interval = if health_interval.is_zero() {
        Duration::from_secs(1)
    } else {
        health_interval
    };

    let composite = match alias.as_deref() {
        Some(a) if !a.is_empty() => format!("{}.{}", ch.name(), a),
        _ => ch.name().to_string(),
    };
    let span = zeroclaw_log::attribution_span!(&*ch);
    zeroclaw_spawn::spawn!(
        async move {
            let component = format!("channel:{composite}");
            let mut backoff = initial_backoff_secs.max(1);
            let max_backoff = max_backoff_secs.max(backoff);

            loop {
                zeroclaw_runtime::health::mark_component_ok(&component);
                let mut health = tokio::time::interval(health_interval);
                health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let result = {
                    let listen_future = ch.listen(tx.clone());
                    tokio::pin!(listen_future);

                    loop {
                        tokio::select! {
                            () = cancel.cancelled() => return,
                            _ = health.tick() => {
                                zeroclaw_runtime::health::mark_component_ok(&component);
                            }
                            result = &mut listen_future => break result,
                        }
                    }
                };

                match result {
                    Ok(()) => {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                            &format!("Channel {} exited unexpectedly; restarting", ch.name())
                        );
                        zeroclaw_runtime::health::mark_component_error(
                            &component,
                            "listener exited unexpectedly",
                        );
                        backoff = initial_backoff_secs.max(1);
                    }
                    Err(e) => {
                        if is_non_retryable_channel_listener_error(ch.name(), &e) {
                            ::zeroclaw_log::record!(
                                ERROR,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Reject
                                )
                                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                                .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                                "channel listener hit non-retryable error; waiting for config change or shutdown"
                            );
                            zeroclaw_runtime::health::mark_component_error(&component, e.to_string());
                            tokio::select! {
                                () = cancel.cancelled() => return,
                                () = std::future::pending::<()>() => unreachable!(),
                            }
                        }
                        ::zeroclaw_log::record!(
                            ERROR,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Fail
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                            "channel listener error; restarting"
                        );
                        zeroclaw_runtime::health::mark_component_error(&component, e.to_string());
                    }
                }

                zeroclaw_runtime::health::bump_component_restart(&component);
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(Duration::from_secs(backoff)) => {}
                }
                backoff = backoff.saturating_mul(2).min(max_backoff);
            }
        }
        .instrument(span)
    )
}

fn is_non_retryable_channel_listener_error(channel_name: &str, error: &anyhow::Error) -> bool {
    match channel_name {
        name if name == "discord" || name.starts_with("discord-") => {
            #[cfg(feature = "channel-discord")]
            if error
                .downcast_ref::<crate::discord::DiscordListenerFatalError>()
                .is_some()
            {
                return true;
            }
            zeroclaw_providers::reliable::is_non_retryable(error)
        }
        _ => false,
    }
}

fn compute_max_in_flight_messages(
    channel_count: usize,
    max_concurrent_per_channel: usize,
) -> usize {
    channel_count
        .saturating_mul(max_concurrent_per_channel)
        .clamp(
            CHANNEL_MIN_IN_FLIGHT_MESSAGES,
            CHANNEL_MAX_IN_FLIGHT_MESSAGES,
        )
}

fn max_in_flight_messages_for_config(
    channel_count: usize,
    config: &zeroclaw_config::schema::ChannelsConfig,
) -> usize {
    compute_max_in_flight_messages(channel_count, config.max_concurrent_per_channel)
}

fn log_worker_join_result(result: Result<(), tokio::task::JoinError>) {
    if let Err(error) = result {
        ::zeroclaw_log::record!(
            ERROR,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({"error": format!("{}", error)})),
            "Channel message worker crashed"
        );
    }
}

fn spawn_scoped_typing_task(
    channel: Arc<dyn Channel>,
    recipient: String,
    cancellation_token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let stop_signal = cancellation_token;
    let refresh_interval = Duration::from_secs(CHANNEL_TYPING_REFRESH_INTERVAL_SECS);
    zeroclaw_spawn::spawn!(async move {
        let mut interval = tokio::time::interval(refresh_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                () = stop_signal.cancelled() => break,
                _ = interval.tick() => {
                    if let Err(e) = channel.start_typing(&recipient).await {
                        ::zeroclaw_log::record!(DEBUG, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"error": format!("{}", e)})), "failed to start typing");
                    }
                }
            }
        }

        if let Err(e) = channel.stop_typing(&recipient).await {
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "failed to stop typing"
            );
        }
    })
}

struct ScopedTypingTask {
    cancellation_token: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
}

struct ScopedTypingController {
    channel: Arc<dyn Channel>,
    recipient: String,
    task: tokio::sync::Mutex<Option<ScopedTypingTask>>,
}

impl ScopedTypingController {
    fn new(channel: Arc<dyn Channel>, recipient: String) -> Self {
        Self {
            channel,
            recipient,
            task: tokio::sync::Mutex::new(None),
        }
    }

    async fn resume(&self) {
        let mut task = self.task.lock().await;
        if task.is_some() {
            return;
        }

        let cancellation_token = CancellationToken::new();
        let handle = spawn_scoped_typing_task(
            Arc::clone(&self.channel),
            self.recipient.clone(),
            cancellation_token.clone(),
        );
        *task = Some(ScopedTypingTask {
            cancellation_token,
            handle,
        });
    }

    async fn pause(&self) {
        let task = self.task.lock().await.take();
        if let Some(task) = task {
            task.cancellation_token.cancel();
            log_worker_join_result(task.handle.await);
        }
    }
}

struct ApprovalTypingChannel {
    inner: Arc<dyn Channel>,
    typing: Arc<ScopedTypingController>,
}

impl ApprovalTypingChannel {
    fn new(inner: Arc<dyn Channel>, typing: Arc<ScopedTypingController>) -> Self {
        Self { inner, typing }
    }
}

impl ::zeroclaw_api::attribution::Attributable for ApprovalTypingChannel {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        self.inner.role()
    }

    fn alias(&self) -> &str {
        self.inner.alias()
    }
}

// `ToolLoop::channel` is consumed only by the approval gate. Approval-gated
// calls are forced sequential by `should_execute_tools_in_parallel`, so this
// deliberately narrow wrapper forwards the required Channel methods plus the
// approval boundary instead of acting as a general channel facade.
#[async_trait::async_trait]
impl Channel for ApprovalTypingChannel {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        self.inner.send(message).await
    }

    async fn listen(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        self.inner.listen(tx).await
    }

    async fn request_approval(
        &self,
        recipient: &str,
        request: &zeroclaw_api::channel::ChannelApprovalRequest,
    ) -> anyhow::Result<Option<zeroclaw_api::channel::ChannelApprovalResponse>> {
        Ok(self
            .request_approval_attributed(recipient, request)
            .await?
            .map(|response| response.response))
    }

    async fn request_approval_attributed(
        &self,
        recipient: &str,
        request: &zeroclaw_api::channel::ChannelApprovalRequest,
    ) -> anyhow::Result<Option<zeroclaw_api::channel::AttributedApprovalResponse>> {
        self.typing.pause().await;
        let response = self
            .inner
            .request_approval_attributed(recipient, request)
            .await;
        if response.as_ref().is_ok_and(|response| {
            response.as_ref().is_some_and(|response| {
                matches!(
                    response.response,
                    zeroclaw_api::channel::ChannelApprovalResponse::Approve
                        | zeroclaw_api::channel::ChannelApprovalResponse::AlwaysApprove
                )
            })
        }) {
            self.typing.resume().await;
        }
        response
    }
}

/// Pump draft deltas to the channel transport, sanitizing every partial on the
/// way out.
///
/// Extracted from the streaming spawn so the boundary can be exercised through
/// the values actually handed to `update_draft` and `update_draft_progress`. A
/// test that calls [`sanitize_streaming_draft_text`] directly proves only that
/// the helper is correct, and would stay green if this wiring were removed;
/// the leak this guards against is a transport call carrying raw text, so that
/// is what the regression needs to observe.
///
/// Status deltas are sanitized per delta because they replace the progress
/// line outright, whereas text deltas are accumulated first: the sanitizer
/// needs the whole partial to tell a closed envelope from one still arriving.
///
/// `known_tool_names` comes from the same registry the final sanitizer reads,
/// so both boundaries judge a protocol payload by the same tool inventory.
async fn run_draft_updater(
    channel: Arc<dyn Channel>,
    reply_target: String,
    draft_id: String,
    known_tool_names: HashSet<String>,
    mut rx: tokio::sync::mpsc::Receiver<zeroclaw_runtime::agent::loop_::DraftEvent>,
) {
    use zeroclaw_runtime::agent::loop_::StreamDelta;
    let mut accumulated = String::new();
    while let Some(event) = rx.recv().await {
        match event {
            StreamDelta::Status(text) => {
                let visible = sanitize_streaming_draft_text(&text, &known_tool_names);
                if let Err(e) = channel
                    .update_draft_progress(&reply_target, &draft_id, &visible)
                    .await
                {
                    ::zeroclaw_log::record!(
                        DEBUG,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                        "Draft progress update failed"
                    );
                }
            }
            StreamDelta::Text(text) => {
                accumulated.push_str(&text);
                let visible = sanitize_streaming_draft_text(&accumulated, &known_tool_names);
                if let Err(e) = channel
                    .update_draft(&reply_target, &draft_id, &visible)
                    .await
                {
                    ::zeroclaw_log::record!(
                        DEBUG,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                        "Draft update failed"
                    );
                }
            }
        }
    }
}

fn resolve_channel_ack_reactions(
    ctx: &ChannelRuntimeContext,
    msg: &zeroclaw_api::channel::ChannelMessage,
) -> bool {
    let Some(ref alias) = msg.channel_alias else {
        return ctx.ack_reactions;
    };
    match msg.channel.as_str() {
        "lark" | "feishu" => ctx
            .prompt_config
            .channels
            .lark
            .get(alias)
            .and_then(|c| c.ack_reactions)
            .unwrap_or(ctx.ack_reactions),
        "telegram" => ctx
            .prompt_config
            .channels
            .telegram
            .get(alias)
            .and_then(|c| c.ack_reactions)
            .unwrap_or(ctx.ack_reactions),
        "matrix" => ctx
            .prompt_config
            .channels
            .matrix
            .get(alias)
            .and_then(|c| c.ack_reactions)
            .unwrap_or(ctx.ack_reactions),
        _ => ctx.ack_reactions,
    }
}

async fn reconcile_early_ack(
    ctx: &ChannelRuntimeContext,
    msg: &ChannelMessage,
    target_channel: Option<&Arc<dyn Channel>>,
    early_ack_task: Option<tokio::task::JoinHandle<()>>,
    done_emoji: Option<&str>,
) {
    if !resolve_channel_ack_reactions(ctx, msg) {
        return;
    }
    let Some(channel) = target_channel else {
        return;
    };
    // Wait for the spawned 👀 add to land first; otherwise a fast early-return
    // path could remove before the add runs and strand the ack.
    if let Some(task) = early_ack_task {
        let _ = task.await;
    }
    let _ = channel
        .remove_reaction(&msg.reply_target, &msg.id, "\u{1F440}")
        .await;
    if let Some(emoji) = done_emoji {
        let _ = channel
            .add_reaction(&msg.reply_target, &msg.id, emoji)
            .await;
    }
}

fn stamp_session_routing_context(
    ctx: &ChannelRuntimeContext,
    msg: &ChannelMessage,
    history_key: &str,
) {
    let Some(ref store) = ctx.session_store else {
        return;
    };

    let channel_id = msg
        .channel_alias
        .as_deref()
        .map(|alias| format!("{}.{alias}", msg.channel));
    let room_id = msg
        .thread_ts
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let target = msg.reply_target.trim();
            if target.is_empty() {
                None
            } else {
                Some(target)
            }
        });
    let context = zeroclaw_infra::session_backend::SessionContext {
        channel_id: channel_id.as_deref(),
        room_id,
        sender_id: Some(msg.sender.as_str()).filter(|s| !s.is_empty()),
    };
    if let Err(e) = store.set_session_context(history_key, context) {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"history_key": history_key, "e": e.to_string()})),
            "Failed to stamp session routing context"
        );
    }
}

fn record_passive_context(ctx: &ChannelRuntimeContext, msg: &ChannelMessage, history_key: &str) {
    let timestamped_content =
        timestamped_channel_user_history_content(msg, WHATSAPP_OBSERVED_GROUP_MESSAGE_LABEL);
    append_sender_turn(ctx, history_key, ChatMessage::user(&timestamped_content));
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
            ::serde_json::json!({
                "message_id": msg.id,
                "history_key": history_key,
            })
        ),
        "recorded passive channel context"
    );
}

async fn dispatch_worker(
    ctx: Arc<ChannelRuntimeContext>,
    msg: zeroclaw_api::channel::ChannelMessage,
    in_flight: Arc<tokio::sync::Mutex<HashMap<String, InFlightSenderTaskState>>>,
    task_sequence: Arc<AtomicU64>,
    permit: tokio::sync::OwnedSemaphorePermit,
    inbox: Option<Arc<MessageInbox>>,
) {
    let _permit = permit;
    let inbox_account = channel_key_for_message(&msg);
    let inbox_message_id = msg.id.clone();
    let interrupt_enabled = ctx
        .interrupt_on_new_message
        .enabled_for_channel(msg.channel.as_str());
    let sender_scope_key = interruption_scope_key(&msg);
    let cancellation_token = CancellationToken::new();
    let completion = Arc::new(InFlightTaskCompletion::new());
    let task_id = task_sequence.fetch_add(1, Ordering::Relaxed);

    let register_in_flight = msg.channel != "cli" && !msg.passive_context;

    if register_in_flight {
        let previous = {
            let mut active = in_flight.lock().await;
            active.insert(
                sender_scope_key.clone(),
                InFlightSenderTaskState {
                    task_id,
                    cancellation: cancellation_token.clone(),
                    completion: Arc::clone(&completion),
                },
            )
        };

        if interrupt_enabled && let Some(previous) = previous {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({"sender": msg.sender})),
                "interrupting previous in-flight request for sender"
            );
            previous.cancellation.cancel();
            previous.completion.wait().await;
        }
    }

    process_channel_message(ctx, msg, cancellation_token).await;

    // The turn is finished: only now is a future redelivery safely
    // suppressible. Failure here means at-least-once re-processing, never
    // a silent drop.
    if let Some(inbox) = inbox {
        let _ = tokio::task::spawn_blocking(move || {
            inbox.mark_completed(&inbox_account, &inbox_message_id)
        })
        .await;
    }

    if register_in_flight {
        let mut active = in_flight.lock().await;
        if active
            .get(&sender_scope_key)
            .is_some_and(|state| state.task_id == task_id)
        {
            active.remove(&sender_scope_key);
        }
    }

    completion.mark_done();
}

#[derive(Clone)]
pub(crate) struct AgentRouter {
    by_agent: Arc<HashMap<String, Arc<ChannelRuntimeContext>>>,
    owner_by_channel_key: Arc<HashMap<String, String>>,
    single_ctx: Option<Arc<ChannelRuntimeContext>>,
}

impl AgentRouter {
    #[cfg(test)]
    fn single(ctx: Arc<ChannelRuntimeContext>) -> Self {
        Self {
            by_agent: Arc::new(HashMap::new()),
            owner_by_channel_key: Arc::new(HashMap::new()),
            single_ctx: Some(ctx),
        }
    }

    fn multi(
        by_agent: HashMap<String, Arc<ChannelRuntimeContext>>,
        owner_by_channel_key: HashMap<String, String>,
    ) -> Self {
        Self {
            by_agent: Arc::new(by_agent),
            owner_by_channel_key: Arc::new(owner_by_channel_key),
            single_ctx: None,
        }
    }

    fn resolve(
        &self,
        msg: &zeroclaw_api::channel::ChannelMessage,
    ) -> Option<Arc<ChannelRuntimeContext>> {
        if let Some(ctx) = &self.single_ctx {
            return Some(Arc::clone(ctx));
        }
        if let Some(alias) = msg.channel_alias.as_deref().filter(|s| !s.is_empty()) {
            let composite = format!("{}.{alias}", msg.channel);
            // An explicit alias identifies a distinct configured channel. It
            // must not fall back to another alias's bare platform owner.
            return self
                .owner_by_channel_key
                .get(&composite)
                .and_then(|agent| self.by_agent.get(agent))
                .cloned();
        }
        if let Some(agent) = self.owner_by_channel_key.get(&msg.channel)
            && let Some(ctx) = self.by_agent.get(agent)
        {
            return Some(Arc::clone(ctx));
        }
        None
    }
}

fn channel_key_for_message(msg: &zeroclaw_api::channel::ChannelMessage) -> String {
    match msg.channel_alias.as_deref() {
        Some(alias) => format!("{}.{alias}", msg.channel),
        None => msg.channel.clone(),
    }
}

/// Resolve effective debounce window: a per-channel override with a positive
/// value wins, otherwise falls back to the global default from `ChannelsConfig`.
/// A per-channel value of `0` is treated as unset (falls back to global).
fn resolve_effective_debounce_window(
    global_ms: u64,
    channel: &str,
    channel_alias: Option<&str>,
    telegram_configs: &std::collections::HashMap<String, zeroclaw_config::schema::TelegramConfig>,
) -> std::time::Duration {
    let per_channel_ms = if channel == "telegram" {
        channel_alias
            .and_then(|alias| telegram_configs.get(alias))
            .and_then(|cfg| cfg.debounce_ms)
            .filter(|ms| *ms > 0)
    } else {
        None
    };
    std::time::Duration::from_millis(per_channel_ms.unwrap_or(global_ms))
}

/// Channels exempt from ingress dedup: their inbound ids are per-listen
/// session counters (or interactive input), not durable platform message
/// ids, so checking them against the durable seen-set would drop fresh
/// traffic after a listener restart (the counter resets, the store does
/// not). None of them redelivers via a persisted cursor, which is the
/// window dedup exists to close.
const NON_DEDUP_CHANNELS: &[&str] = &["cli", "webhook", "voice_wake"];

async fn run_message_dispatch_loop(
    mut rx: tokio::sync::mpsc::Receiver<zeroclaw_api::channel::ChannelMessage>,
    router: AgentRouter,
    max_in_flight_messages: usize,
    inbox: Option<std::sync::Arc<MessageInbox>>,
) {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_in_flight_messages));
    let mut workers = tokio::task::JoinSet::new();
    let in_flight_by_sender = Arc::new(tokio::sync::Mutex::new(HashMap::<
        String,
        InFlightSenderTaskState,
    >::new()));
    let task_sequence = Arc::new(AtomicU64::new(1));

    while let Some(msg) = rx.recv().await {
        if let Some(seen_ids) = &inbox
            && !NON_DEDUP_CHANNELS.contains(&msg.channel.as_str())
            && !msg.id.is_empty()
        {
            let account = channel_key_for_message(&msg);
            let message_id = msg.id.clone();
            let store = Arc::clone(seen_ids);
            let recorded =
                tokio::task::spawn_blocking(move || store.admit(&account, &message_id)).await;
            match recorded {
                Ok(Ok(Admission::Fresh)) => {}
                Ok(Ok(Admission::DuplicateCompleted)) => {
                    ::zeroclaw_log::record!(
                        INFO,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_attrs(::serde_json::json!({
                                "channel": msg.channel,
                                "message_id": msg.id,
                                "sender": msg.sender,
                            })),
                        "dropping redelivered inbound message (turn already completed)"
                    );
                    continue;
                }
                Ok(Ok(Admission::DuplicateInFlight)) => {
                    ::zeroclaw_log::record!(
                        INFO,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_attrs(::serde_json::json!({
                                "channel": msg.channel,
                                "message_id": msg.id,
                                "sender": msg.sender,
                            })),
                        "dropping concurrent duplicate of an in-flight message"
                    );
                    continue;
                }
                Ok(Err(err)) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "channel": msg.channel,
                                "message_id": msg.id,
                                "err": err.to_string(),
                            })),
                        "inbox store failed; processing without dedup"
                    );
                }
                Err(_) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "channel": msg.channel,
                                "message_id": msg.id,
                                "err": "spawn_blocking join error",
                            })),
                        "seen-id store failed; processing without dedup"
                    );
                }
            }
        }
        let Some(ctx) = router.resolve(&msg) else {
            ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"channel_alias": msg.channel_alias, "sender": msg.sender})), "dropping inbound message: no agent owns this channel");
            continue;
        };

        // Fast path: /stop cancels the in-flight task for this sender scope without
        // spawning a worker or registering a new task. Handled here — before semaphore
        // acquisition — so the target task is still in the store and is never replaced.
        if msg.channel != "cli" && is_stop_command(&msg.content) {
            let scope_key = interruption_scope_key(&msg);
            let previous = {
                let mut active = in_flight_by_sender.lock().await;
                active.remove(&scope_key)
            };
            let reply = if let Some(state) = previous {
                state.cancellation.cancel();
                zeroclaw_runtime::i18n::get_required_cli_string("channel-runtime-stop-sent")
            } else {
                zeroclaw_runtime::i18n::get_required_cli_string("channel-runtime-stop-no-task")
            };
            let channel = find_channel_for_message(&ctx.channels_by_name, &msg).cloned();
            if let Some(channel) = channel {
                let reply_target = msg.reply_target.clone();
                let thread_ts = msg.thread_ts.clone();
                zeroclaw_spawn::spawn!(async move {
                    let _ = channel
                        .send(&SendMessage::new(reply, &reply_target).in_thread(thread_ts))
                        .await;
                });
            } else {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                    "stop command: no registered channel found for reply"
                );
            }
            continue;
        }

        // ── Debounce: accumulate rapid messages per sender ──────────
        // CLI messages bypass debouncing so the interactive loop stays responsive.
        let msg = if msg.channel != "cli" {
            let debounce_key = conversation_history_key(&msg);

            // Resolve effective debounce window: per-channel override wins,
            // otherwise falls back to the global default from ChannelsConfig.
            // A per-channel value of 0 is treated as unset (falls back to global).
            let debounce_window = resolve_effective_debounce_window(
                ctx.prompt_config.channels.debounce_ms,
                &msg.channel,
                msg.channel_alias.as_deref(),
                &ctx.prompt_config.channels.telegram,
            );

            match ctx
                .debouncer
                .debounce_with_window(&debounce_key, &msg.content, debounce_window)
                .await
            {
                zeroclaw_infra::debounce::DebounceResult::Pending(rx) => {
                    // Spawn a lightweight task that waits for the debounce window
                    // to expire, then feeds the combined message through the normal
                    // worker path below.
                    let debounce_ctx = Arc::clone(&ctx);
                    let debounce_in_flight = Arc::clone(&in_flight_by_sender);
                    let debounce_semaphore = Arc::clone(&semaphore);
                    let debounce_task_seq = Arc::clone(&task_sequence);
                    let debounce_inbox = inbox.clone();
                    let mut debounce_msg = msg;
                    workers.spawn(async move {
                        let combined = match rx.await {
                            Ok(combined) => combined,
                            Err(_) => {
                                // Receiver dropped — a newer message superseded this one.
                                return;
                            }
                        };
                        debounce_msg.content = combined;
                        ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"channel": debounce_msg.channel, "sender": debounce_msg.sender})), "Debounced message ready — dispatching combined message");

                        let permit = match debounce_semaphore.acquire_owned().await {
                            Ok(permit) => permit,
                            Err(_) => return,
                        };

                        dispatch_worker(
                            debounce_ctx,
                            debounce_msg,
                            debounce_in_flight,
                            debounce_task_seq,
                            permit,
                            debounce_inbox,
                        )
                        .await;
                    });
                    continue;
                }
                zeroclaw_infra::debounce::DebounceResult::Passthrough(content) => {
                    let mut m = msg;
                    m.content = content;
                    m
                }
            }
        } else {
            msg
        };

        let permit = match Arc::clone(&semaphore).acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => break,
        };

        let worker_ctx = Arc::clone(&ctx);
        let in_flight = Arc::clone(&in_flight_by_sender);
        let task_sequence = Arc::clone(&task_sequence);
        let worker_inbox = inbox.clone();
        workers.spawn(async move {
            dispatch_worker(
                worker_ctx,
                msg,
                in_flight,
                task_sequence,
                permit,
                worker_inbox,
            )
            .await;
        });

        while let Some(result) = workers.try_join_next() {
            log_worker_join_result(result);
        }
    }

    while let Some(result) = workers.join_next().await {
        log_worker_join_result(result);
    }
}

fn normalize_telegram_identity(value: &str) -> String {
    value.trim().trim_start_matches('@').to_string()
}

/// Trim-only identity normalizer for channels whose native id has no
/// `@`-style prefix to strip (WeChat openid, LINE user id).
fn normalize_trim_identity(value: &str) -> String {
    value.trim().to_string()
}

/// Per-channel-type identity normalizer. The operator-bind op is otherwise
/// identical across the pairing-capable channels; the only variance is how a
/// raw identity is canonicalized before it is stored in the allowlist.
pub type ChannelIdentityNormalizer = fn(&str) -> String;

/// Resolve the identity normalizer for a pairing-capable channel type, or
/// `None` for a type with no operator-bind surface. `None` is the closed-set
/// gate: only `telegram` / `wechat` / `line` can be bound this way.
#[must_use]
pub fn channel_identity_normalizer(channel_type: &str) -> Option<ChannelIdentityNormalizer> {
    match channel_type {
        "telegram" => Some(normalize_telegram_identity),
        "wechat" | "line" => Some(normalize_trim_identity),
        _ => None,
    }
}

/// Whether a `[channels.<type>.<alias>]` section exists. Rust has no
/// reflection over the typed channel maps, so this stays an explicit per-type
/// match; only this arm grows when a new pairing channel lands.
#[must_use]
pub fn channel_alias_configured(config: &Config, channel_type: &str, alias: &str) -> bool {
    match channel_type {
        "telegram" => config.channels.telegram.contains_key(alias),
        "wechat" => config.channels.wechat.contains_key(alias),
        "line" => config.channels.line.contains_key(alias),
        _ => false,
    }
}

/// Add `identity` to the peer group bound to `<type>.<alias>` in-place.
///
/// Returns `Ok(true)` when the identity was newly added, `Ok(false)` when it
/// was already present. Pure config mutation — no disk write, no daemon
/// restart — so it is the single core shared by the CLI
/// (`bind_telegram_identity`) and the gateway bind endpoint. The `channel`
/// field is the dotted `<type>.<alias>` ref so authorization stays scoped to
/// the bound alias; a bare type would broaden the peer across every alias of
/// that type.
pub fn bind_channel_identity_into(
    config: &mut Config,
    channel_type: &str,
    alias: &str,
    identity: &str,
) -> Result<bool> {
    use zeroclaw_config::multi_agent::{PeerGroupConfig, PeerUsername};
    use zeroclaw_config::providers::ChannelRef;

    let Some(normalize) = channel_identity_normalizer(channel_type) else {
        anyhow::bail!(
            "Channel type `{channel_type}` does not support identity binding \
             (supported: telegram, wechat, line)."
        );
    };

    let normalized = normalize(identity);
    if normalized.is_empty() {
        anyhow::bail!("{channel_type} identity cannot be empty");
    }

    // The alias must name an existing `[channels.<type>.<alias>]` section.
    // Binding into a phantom alias would mint a peer group the runtime never
    // reads (it resolves authorization per the alias the channel actually
    // runs under), so fail loudly instead of silently authorizing nobody.
    if !channel_alias_configured(config, channel_type, alias) {
        anyhow::bail!(
            "{channel_type} channel alias `{alias}` is not configured. Run \
             `zeroclaw config set channels.{channel_type}.{alias}.bot_token <token>` \
             (see docs/book/src/channels/overview.md for the full field list)."
        );
    }

    let group_name = format!("{channel_type}_{alias}");
    let channel_ref = format!("{channel_type}.{alias}");
    let group = config
        .peer_groups
        .entry(group_name)
        .or_insert_with(|| PeerGroupConfig {
            channel: ChannelRef::new(channel_ref),
            ..PeerGroupConfig::default()
        });

    if group
        .external_peers
        .iter()
        .any(|p| normalize(p.as_str()) == normalized)
    {
        return Ok(false);
    }

    group.external_peers.push(PeerUsername::new(normalized));
    Ok(true)
}

/// Telegram-specific thin wrapper over [`bind_channel_identity_into`], kept
/// for the CLI entry point and its unit tests.
fn bind_telegram_identity_into(config: &mut Config, identity: &str, alias: &str) -> Result<bool> {
    bind_channel_identity_into(config, "telegram", alias, identity)
}

pub async fn bind_telegram_identity(config: &Config, identity: &str, alias: &str) -> Result<()> {
    let normalized = normalize_telegram_identity(identity);
    let mut updated = config.clone();

    if !bind_telegram_identity_into(&mut updated, identity, alias)? {
        println!("✅ Telegram identity already bound to telegram.{alias}: {normalized}");
        return Ok(());
    }

    updated.save().await?;
    println!("✅ Bound Telegram identity {normalized} to telegram.{alias}");
    println!("   Saved to {}", updated.config_path.display());
    match maybe_restart_managed_daemon_service() {
        Ok(true) => {
            println!("🔄 Detected running managed daemon service; reloaded automatically.");
        }
        Ok(false) => {
            println!(
                "ℹ️ No managed daemon service detected. If `zeroclaw daemon`/`channel start` is already running, restart it to load the updated allowlist."
            );
        }
        Err(e) => {
            eprintln!(
                "⚠️ Allowlist saved, but failed to reload daemon service automatically: {e}\n\
                 Restart service manually with `zeroclaw service stop && zeroclaw service start`."
            );
        }
    }
    Ok(())
}

fn maybe_restart_managed_daemon_service() -> Result<bool> {
    if cfg!(target_os = "macos") {
        let home = directories::UserDirs::new()
            .map(|u| u.home_dir().to_path_buf())
            .context("Could not find home directory")?;
        let plist = home
            .join("Library")
            .join("LaunchAgents")
            .join("com.zeroclaw.daemon.plist");
        if !plist.exists() {
            return Ok(false);
        }

        let list_output = Command::new("launchctl")
            .arg("list")
            .output()
            .context("Failed to query launchctl list")?;
        let listed = String::from_utf8_lossy(&list_output.stdout);
        if !listed.contains("com.zeroclaw.daemon") {
            return Ok(false);
        }

        let _ = Command::new("launchctl")
            .args(["stop", "com.zeroclaw.daemon"])
            .output();
        let start_output = Command::new("launchctl")
            .args(["start", "com.zeroclaw.daemon"])
            .output()
            .context("Failed to start launchd daemon service")?;
        if !start_output.status.success() {
            let stderr = String::from_utf8_lossy(&start_output.stderr);
            anyhow::bail!("launchctl start failed: {}", stderr.trim());
        }

        return Ok(true);
    }

    if cfg!(target_os = "linux") {
        // OpenRC (system-wide) takes precedence over systemd (user-level)
        let openrc_init_script = PathBuf::from("/etc/init.d/zeroclaw");
        if openrc_init_script.exists()
            && let Ok(status_output) = Command::new("rc-service").args(OPENRC_STATUS_ARGS).output()
        {
            // rc-service exits 0 if running, non-zero otherwise
            if status_output.status.success() {
                let restart_output = Command::new("rc-service")
                    .args(OPENRC_RESTART_ARGS)
                    .output()
                    .context("Failed to restart OpenRC daemon service")?;
                if !restart_output.status.success() {
                    let stderr = String::from_utf8_lossy(&restart_output.stderr);
                    anyhow::bail!("rc-service restart failed: {}", stderr.trim());
                }
                return Ok(true);
            }
        }

        // Systemd (user-level)
        let home = directories::UserDirs::new()
            .map(|u| u.home_dir().to_path_buf())
            .context("Could not find home directory")?;
        let unit_path: PathBuf = home
            .join(".config")
            .join("systemd")
            .join("user")
            .join("zeroclaw.service");
        if !unit_path.exists() {
            return Ok(false);
        }

        let active_output = Command::new("systemctl")
            .args(SYSTEMD_STATUS_ARGS)
            .output()
            .context("Failed to query systemd service state")?;
        let state = String::from_utf8_lossy(&active_output.stdout);
        if !state.trim().eq_ignore_ascii_case("active") {
            return Ok(false);
        }

        let restart_output = Command::new("systemctl")
            .args(SYSTEMD_RESTART_ARGS)
            .output()
            .context("Failed to restart systemd daemon service")?;
        if !restart_output.status.success() {
            let stderr = String::from_utf8_lossy(&restart_output.stderr);
            anyhow::bail!("systemctl restart failed: {}", stderr.trim());
        }

        return Ok(true);
    }

    Ok(false)
}

pub async fn send_channel_message(
    config: &Config,
    channel_id: &str,
    recipient: &str,
    message: &str,
) -> Result<()> {
    // Wrap into the canonical shared handle for the builder; this is a
    // one-shot path so the snapshot is dropped immediately after send.
    let config_arc = Arc::new(RwLock::new(config.clone()));
    let channel = build_channel_by_id(&config_arc, channel_id)?;
    let msg = SendMessage::new(message, recipient);
    channel
        .send(&msg)
        .await
        .with_context(|| format!("Failed to send message via {channel_id}"))?;
    println!("Message sent via {channel_id}.");
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelHealthState {
    Healthy,
    Unhealthy,
    Timeout,
}

fn classify_health_result(
    result: &std::result::Result<bool, tokio::time::error::Elapsed>,
) -> ChannelHealthState {
    match result {
        Ok(true) => ChannelHealthState::Healthy,
        Ok(false) => ChannelHealthState::Unhealthy,
        Err(_) => ChannelHealthState::Timeout,
    }
}

fn find_channel_for_message<'a>(
    channels: &'a HashMap<String, Arc<dyn Channel>>,
    msg: &zeroclaw_api::channel::ChannelMessage,
) -> Option<&'a Arc<dyn Channel>> {
    if let Some(alias) = msg.channel_alias.as_deref().filter(|s| !s.is_empty()) {
        let composite = format!("{}.{alias}", msg.channel);
        if let Some(ch) = channels.get(&composite) {
            return Some(ch);
        }
    }
    if let Some(ch) = channels.get(&msg.channel) {
        return Some(ch);
    }
    msg.channel
        .split_once(':')
        .and_then(|(base, _)| channels.get(base))
}

fn send_message_to_peer_tool_available(
    ctx: &ChannelRuntimeContext,
    msg: &zeroclaw_api::channel::ChannelMessage,
) -> bool {
    let excluded_for_turn = msg.channel != "cli" && ctx.autonomy_level != AutonomyLevel::Full;
    if excluded_for_turn
        && ctx
            .non_cli_excluded_tools
            .iter()
            .any(|tool_name| tool_name == "send_message_to_peer")
    {
        return false;
    }

    ctx.tools_registry
        .iter()
        .any(|tool| tool.name() == "send_message_to_peer")
}

fn peer_prompt_channel_ref(
    ctx: &ChannelRuntimeContext,
    msg: &zeroclaw_api::channel::ChannelMessage,
) -> Option<String> {
    let composite = composite_channel_key(&msg.channel, msg.channel_alias.as_deref());
    if msg
        .channel_alias
        .as_deref()
        .is_some_and(|alias| !alias.is_empty())
    {
        return Some(composite);
    }

    let Some(agent) = ctx.prompt_config.agents.get(ctx.agent_alias.as_str()) else {
        return Some(composite);
    };

    if agent.channels.iter().any(|channel| channel == &composite) {
        return Some(composite);
    }

    let matches: Vec<&str> = agent
        .channels
        .iter()
        .map(|channel| channel.as_str())
        .filter(|channel| channel_ref_matches_message_channel(channel, &msg.channel))
        .collect();
    if matches.len() == 1 {
        Some(matches[0].to_string())
    } else {
        None
    }
}

fn channel_ref_matches_message_channel(channel_ref: &str, message_channel: &str) -> bool {
    if channel_ref == message_channel {
        return true;
    }

    let message_base = message_channel
        .split_once(':')
        .map(|(base, _)| base)
        .unwrap_or(message_channel);
    channel_ref == message_base
        || channel_ref
            .split_once('.')
            .is_some_and(|(channel_type, _)| channel_type == message_base)
}

fn no_real_time_channels_message() -> &'static str {
    "No real-time channels configured. Run `zeroclaw quickstart` to set one up."
}

/// Run health checks for configured channels.
pub async fn doctor_channels(config: Config) -> Result<()> {
    let config_arc = Arc::new(RwLock::new(config));
    #[allow(unused_mut)]
    let mut channels = collect_configured_channels(&config_arc, "health check", &[]);

    #[cfg(feature = "channel-nostr")]
    {
        // Materialize the work list into owned values BEFORE any `.await`
        // so the RwLockReadGuard is dropped before the async constructor
        // runs (parking_lot guards are not Send).
        let nostr_jobs: Vec<(String, String, Vec<String>)> = {
            let config = config_arc.read();
            // Share the same gate as the Discord/shared-collector path so
            // theinvariant ("a disabled agent must not bring its
            // bound channel online") is enforced uniformly — see the
            // `ActiveChannelAliases::compute` constructor for details.
            let active = ActiveChannelAliases::compute(&config);
            config
                .channels
                .nostr
                .iter()
                .filter(|(alias, _)| active.contains(&format!("nostr.{alias}")))
                .filter(|(_, ns)| ns.enabled)
                .map(|(alias, ns)| (alias.clone(), ns.private_key.clone(), ns.relays.clone()))
                .collect()
        };
        for (alias, private_key, relays) in nostr_jobs {
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.clone();
                Arc::new(move || cfg_arc.read().channel_external_peers("nostr", &alias))
            };
            channels.push(ConfiguredChannel {
                display_name: "Nostr",
                alias: Some(alias.clone()),
                channel: Arc::new(
                    NostrChannel::new(&private_key, relays, alias, peer_resolver).await?,
                ),
            });
        }
    }

    #[cfg(not(feature = "channel-nostr"))]
    {
        let config = config_arc.read();
        if !config.channels.nostr.is_empty() {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "Nostr channel is configured but this build was compiled without \
                 `channel-nostr`; skipping Nostr health check."
            );
        }
    }

    if channels.is_empty() {
        println!("{}", no_real_time_channels_message());
        return Ok(());
    }

    println!("🩺 ZeroClaw Channel Doctor");
    println!();

    let mut healthy = 0_u32;
    let mut unhealthy = 0_u32;
    let mut timeout = 0_u32;

    for configured in channels {
        let result =
            tokio::time::timeout(Duration::from_secs(10), configured.channel.health_check()).await;
        let state = classify_health_result(&result);

        match state {
            ChannelHealthState::Healthy => {
                healthy += 1;
                println!("  ✅ {:<9} healthy", configured.display_name);
            }
            ChannelHealthState::Unhealthy => {
                unhealthy += 1;
                println!(
                    "  ❌ {:<9} unhealthy (auth/config/network)",
                    configured.display_name
                );
            }
            ChannelHealthState::Timeout => {
                timeout += 1;
                println!("  ⏱️  {:<9} timed out (>10s)", configured.display_name);
            }
        }
    }

    if !config_arc.read().channels.webhook.is_empty() {
        println!("  ℹ️  Webhook   check via `zeroclaw gateway` then GET /health");
    }

    println!();
    println!("Summary: {healthy} healthy, {unhealthy} unhealthy, {timeout} timed out");
    Ok(())
}

fn build_owner_by_channel_key(
    config: &Config,
    enabled_agents: &[String],
    collected_channel_keys: &[String],
) -> HashMap<String, String> {
    // Owner map: `<channel_type>.<alias>` (and bare `<channel_type>` for
    // backward-compat with cron callers / singleton channels) → agent_alias.
    // Built from each enabled agent's `agents.<alias>.channels` list — the
    // schema treats this as the source of truth for channel ownership.
    let mut owner_by_channel_key: HashMap<String, String> = HashMap::new();
    for alias_str in enabled_agents {
        let Some(agent_cfg) = config.agents.get(alias_str) else {
            debug_assert!(
                false,
                "enabled agent alias missing from config.agents: {}",
                alias_str
            );
            continue;
        };
        for ch in &agent_cfg.channels {
            let ch_str: &str = ch.as_ref();
            owner_by_channel_key.insert(ch_str.to_string(), alias_str.clone());
            if let Some((bare, _)) = ch_str.split_once('.') {
                owner_by_channel_key
                    .entry(bare.to_string())
                    .or_insert_with(|| alias_str.clone());
            }
        }
    }

    let any_binding_declared_anywhere = config.agents.values().any(|a| !a.channels.is_empty());

    if any_binding_declared_anywhere {
        if owner_by_channel_key.is_empty() && !collected_channel_keys.is_empty() {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "channel bindings exist but no owning agent is enabled; \
                 affected channels will be unbound and inbound messages dropped (#8013)"
            );
        }
        return owner_by_channel_key;
    }

    // True legacy mode: no agent anywhere declares a binding. Preserve the
    // existing deterministic fallback so on-disk session hydration and the
    // pre-existing `build_owner_by_channel_key_legacy_fallback_*` tests
    // continue to work.
    if !collected_channel_keys.is_empty() {
        let fallback_owner = config
            .resolved_runtime_agent_alias()
            .filter(|alias| enabled_agents.iter().any(|enabled| enabled == *alias))
            .map(ToString::to_string)
            .or_else(|| enabled_agents.first().cloned());

        if let Some(owner_alias) = fallback_owner {
            for channel_key in collected_channel_keys {
                owner_by_channel_key.insert(channel_key.clone(), owner_alias.clone());
                if let Some((bare, _)) = channel_key.split_once('.') {
                    owner_by_channel_key
                        .entry(bare.to_string())
                        .or_insert_with(|| owner_alias.clone());
                }
            }
        }
    }

    owner_by_channel_key
}

/// The per-agent tool registry, prompt sections, and channel/deferred-MCP handles
/// `start_channels` needs from [`assemble_channel_agent_tools`].
struct ChannelAssembledTools {
    tools: Vec<Box<dyn Tool>>,
    deferred_section: String,
    pinned_section: String,
    ask_user_handle: Option<tools::PerToolChannelHandle>,
    reaction_handle: tools::PerToolChannelHandle,
    poll_handle: Option<tools::PerToolChannelHandle>,
    escalate_handle: Option<tools::PerToolChannelHandle>,
    channel_room_handle: Option<tools::PerToolChannelHandle>,
    activated_handle: Option<Arc<std::sync::Mutex<tools::ActivatedToolSet>>>,
}

/// Route a channel agent's tool registry through the one gated seam
/// (`ScopedToolRegistry::assemble`) - the same seam `run()`/`process_message()`/
/// `Agent::from_config` use. Extracted from `start_channels` so the channel path's
/// specific assembly knobs (below) are exercised directly by a unit test instead of
/// only indirectly through `start_channels`'s much larger, harder-to-isolate flow.
///
/// Replaces the channel path's former hand-rolled peripheral wiring, built-in
/// filter, MCP scoping, and skill registration - which had silently diverged from
/// every other construction path in two ways this cutover closes: MCP
/// resource/prompt capability tools and pinned MCP resources
/// (`docs/book/src/tools/mcp.md` "Pinning resources into context", a documented
/// general agent capability with no channel-specific exception) were never wired
/// into the channel path at all.
///
/// - `connect_peripherals: true` - channel-driven sessions actuate hardware,
///   mirroring the old unconditional `load_peripheral_tools` call.
/// - `runtime` - the orchestrator's REAL configured `RuntimeAdapter`, threaded
///   through skill execution. The old `register_skill_tools_with_context` call
///   defaulted to `NativeRuntime` regardless of `[platform]`.
/// - `connect_mcp: true`, `exclude_memory: false`, `caller_allowed: None` - match
///   the channel path's pre-cutover behavior exactly (no allowlist narrowing beyond
///   the agent's own policy; memory tools kept; MCP connected whenever
///   `config.mcp.enabled`).
///
/// Test coverage: the `assemble_channel_agent_tools_*` tests below drive this
/// function directly. They pin `exclude_memory: false` (memory tools survive),
/// the built-in allow/deny and runtime-threading behavior, and -- via a mock MCP
/// server granting a pinned resource -- that `connect_mcp: true` resolves MCP
/// content into a `pinned_section` kept separate from the deferred tool-search
/// listing. `connect_peripherals: true` is still only exercised as a literal
/// value: `load_peripheral_tools` reads a process-global `OnceLock` that stays
/// empty outside the real daemon binary, so peripheral-tool inclusion cannot be
/// unit-tested here and a regression flipping that knob to `false` would still
/// pass. Closing it needs a daemon-level peripheral harness; tracked as a
/// residual, not silently skipped.
async fn assemble_channel_agent_tools(
    config: &Config,
    agent_alias: &str,
    model_provider: &str,
    model: &str,
    security: &Arc<SecurityPolicy>,
    built: tools::AllToolsResult,
    skills: &[zeroclaw_runtime::skills::Skill],
    runtime: Arc<dyn platform::RuntimeAdapter>,
) -> ChannelAssembledTools {
    use zeroclaw_log::Instrument as _;

    let agent_attribution = zeroclaw_runtime::agent::AgentAttribution(agent_alias);
    let assembled = async {
        zeroclaw_log::scope!(
            model_provider: model_provider,
            model: model,
            => async {
                zeroclaw_runtime::tools::scoped::ScopedToolRegistry::assemble(
                    zeroclaw_runtime::tools::scoped::ScopedAssembly {
                        config,
                        agent_alias,
                        security,
                        built,
                        skills,
                        runtime,
                        caller_allowed: None,
                        connect_mcp: true,
                        connect_peripherals: true,
                        exclude_memory: false,
                        // Channel startup is an execution surface (the agent actually runs),
                        // so deferral behaves as normal; the dashboard-only per-spec listing
                        // is off, matching `run`/`process_message`.
                        list_deferred_mcp_specs: false,
                        emit_assembly_logs: true,
                        // Channel tools are assembled once at daemon startup and
                        // retain their registry-backed wrappers for the listener
                        // lifetime, so there is no per-turn reconnect to avoid here.
                        // The heartbeat worker remains the only caller that supplies
                        // a pre-built registry for reuse across repeated assemblies.
                        mcp_registry: None,
                    },
                )
                .await
            }
        )
        .await
    }
    .instrument(zeroclaw_log::attribution_span!(&agent_attribution))
    .await;
    let deferred_section = assembled.deferred_section().to_string();
    let pinned_section = assembled.pinned_section().to_string();
    let zeroclaw_runtime::tools::scoped::ScopedAssembled {
        registry,
        ask_user_handle,
        reaction_handle,
        poll_handle,
        escalate_handle,
        channel_room_handle,
        activated_handle,
        ..
    } = assembled;
    ChannelAssembledTools {
        tools: registry.into_inner(),
        deferred_section,
        pinned_section,
        ask_user_handle,
        reaction_handle,
        poll_handle,
        escalate_handle,
        channel_room_handle,
        activated_handle,
    }
}

/// Compose a channel agent's post-assembly MCP prompt sections in the order the
/// system prompt requires: apply the strict text-tool suppression policy to ONLY
/// the deferred/tool-search section, then append the pinned MCP resource section
/// afterward. This keeps the two concerns separate so that a strict, non-native
/// target (which clears the deferred tool-search listing) still starts with its
/// granted pinned MCP resources intact. Returns whether the text-tool protocol
/// should be exposed.
///
/// Single-sourced on purpose: `start_channels` and its regression test both call
/// this exact step, so a future edit that reorders the policy/append pair (or
/// applies suppression to a combined section) fails the test instead of silently
/// dropping pinned resources.
fn compose_channel_mcp_prompt_sections(
    native_tools: bool,
    strict_tool_parsing: bool,
    tool_descs: &mut Vec<(&str, &str)>,
    deferred_section: &mut String,
    pinned_section: &str,
) -> bool {
    let expose_text_tool_protocol = apply_text_tool_prompt_policy(
        native_tools,
        strict_tool_parsing,
        tool_descs,
        deferred_section,
    );
    append_pinned_mcp_section(deferred_section, pinned_section);
    expose_text_tool_protocol
}

// ── Concurrent persist lock test ─────────────────────────
// Lives outside `mod tests` so it has direct access to private parent items.

#[cfg(test)]
#[test]
fn concurrent_persist_lock_serialization() {
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use zeroclaw_infra::session_backend::SessionBackend;
    use zeroclaw_providers::ChatMessage;
    use zeroclaw_runtime::approval::ApprovalManager;
    use zeroclaw_runtime::observability::NoopObserver;

    struct OrderBackend {
        sequence: Arc<Mutex<Vec<String>>>,
        call_n: Arc<AtomicUsize>,
    }
    impl SessionBackend for OrderBackend {
        fn load(&self, _key: &str) -> Vec<ChatMessage> {
            vec![]
        }
        fn append(&self, _key: &str, msg: &ChatMessage) -> std::io::Result<()> {
            let content = msg.content.clone();
            let n = self.call_n.fetch_add(1, Ordering::SeqCst);
            self.sequence
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(content);
            // Delay outside the sequence lock: later callers get
            // shorter delays → they exit earlier and can win the
            // history-push race.
            std::thread::sleep(Duration::from_millis(8_u64.saturating_sub(n as u64 * 2)));
            Ok(())
        }
        fn remove_last(&self, _key: &str) -> std::io::Result<bool> {
            Ok(true)
        }
        fn list_sessions(&self) -> Vec<String> {
            vec![]
        }
    }

    let sender = "concurrent_test_key".to_string();
    let sequence = Arc::new(Mutex::new(Vec::new()));
    let backend = OrderBackend {
        sequence: sequence.clone(),
        call_n: Arc::new(AtomicUsize::new(0)),
    };

    let ctx = Arc::new(ChannelRuntimeContext {
        channels_by_name: Arc::new(HashMap::new()),
        model_provider: Arc::new(test_fixtures::DummyModelProvider),
        model_provider_ref: Arc::new("test".into()),
        agent_alias: Arc::new("test".into()),
        agent_cfg: Arc::new(zeroclaw_config::schema::AliasedAgentConfig::default()),
        memory: Arc::new(test_fixtures::NoopMemory),
        memory_strategy: Arc::new(
            zeroclaw_runtime::agent::memory_strategy::DefaultMemoryStrategy::with_config(
                Arc::new(test_fixtures::NoopMemory),
                zeroclaw_config::schema::MemoryConfig::default(),
                std::path::PathBuf::new(),
            ),
        ),
        companion_store: None,
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new(String::new()),
        model: Arc::new("test".into()),
        temperature: Some(0.0),
        auto_save_memory: false,
        max_tool_iterations: 5,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(lru::LruCache::new(
            std::num::NonZeroUsize::new(MAX_CONVERSATION_SENDERS).unwrap(),
        ))),
        pending_new_sessions: Arc::new(Mutex::new(HashSet::new())),
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        thinking_overrides: Arc::new(Mutex::new(HashMap::new())),
        scope_overrides: Arc::new(Mutex::new(HashMap::new())),
        reliability: Arc::new(zeroclaw_config::schema::ReliabilityConfig::default()),
        interrupt_on_new_message: InterruptOnNewMessageConfig {
            telegram: false,
            slack: false,
            discord: false,
            mattermost: false,
            matrix: false,
            whatsapp: false,
        },
        multimodal: zeroclaw_config::schema::MultimodalConfig::default(),
        media_pipeline: zeroclaw_config::schema::MediaPipelineConfig::default(),
        transcription_config: zeroclaw_config::schema::TranscriptionConfig::default(),
        agent_transcription_provider: String::new(),
        hooks: None,
        provider_runtime_options: zeroclaw_providers::ModelProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(std::env::temp_dir()),
        prompt_config: Arc::new(zeroclaw_config::schema::Config::default()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
        non_cli_excluded_tools: Arc::new(Vec::new()),
        autonomy_level: AutonomyLevel::default(),
        tool_call_dedup_exempt: Arc::new(Vec::new()),
        model_routes: Arc::new(Vec::new()),
        query_classification: zeroclaw_config::schema::QueryClassificationConfig::default(),
        ack_reactions: true,
        show_tool_calls: true,
        session_store: Some(Arc::new(backend) as Arc<dyn SessionBackend>),
        approval_manager: Arc::new(ApprovalManager::for_non_interactive(
            &zeroclaw_config::schema::RiskProfileConfig::default(),
        )),
        activated_tools: None,
        cost_tracking: None,
        pacing: zeroclaw_config::schema::PacingConfig::default(),
        max_tool_result_chars: 0,
        context_token_budget: 0,
        debouncer: Arc::new(zeroclaw_infra::debounce::MessageDebouncer::new(
            Duration::ZERO,
        )),
        receipt_generator: None,
        show_receipts_in_response: false,
        last_applied_config_stamp: Arc::new(Mutex::new(None)),
        runtime_defaults_override: Arc::new(Mutex::new(None)),
        persist_locks: Arc::new(Mutex::new(HashMap::new())),
        user_model: None,
        task_prefs: std::sync::Arc::new(TaskPreferenceOverlay::new()),
    });
    ctx.conversation_histories
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(sender.clone(), vec![ChatMessage::user("start")]);

    let barrier = Arc::new(Barrier::new(4));
    let mut handles = vec![];
    for i in 0..4 {
        let ctx = ctx.clone();
        let key = sender.clone();
        let b = barrier.clone();
        handles.push(std::thread::spawn(move || {
            b.wait();
            append_sender_turn(&ctx, &key, ChatMessage::user(format!("msg-{i}")));
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    // ── Assertion ────────────────────────────────────────────────
    // Under the per-sender persist lock every (append, history-push)
    // pair is atomic, so the backend sequence must equal the
    // in-memory history for this sender (minus the initial "start").
    let backend_order: Vec<String> = sequence.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let history: Vec<String> = {
        let histories = ctx
            .conversation_histories
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let turns = histories
            .peek(&sender)
            .expect("history must exist for sender");
        turns
            .iter()
            .filter(|m| m.content != "start")
            .map(|m| m.content.clone())
            .collect()
    };
    assert_eq!(
        backend_order, history,
        "backend append order must equal in-memory history order;\
         a mismatch means the per-sender persist lock is not serializing\
         store.append + history.push atomically"
    );
    assert_eq!(
        backend_order.len(),
        4,
        "all 4 concurrent appends must be recorded"
    );
}

#[cfg(test)]
mod debounce_resolution_tests;
#[cfg(test)]
mod omitted_feature_tests;
#[cfg(test)]
mod test_fixtures;
// Heavy suite gated so lib-test iteration does not pay 17.8k lines; CI channels leg enables it.
#[cfg(all(test, feature = "heavy-tests"))]
mod tests;

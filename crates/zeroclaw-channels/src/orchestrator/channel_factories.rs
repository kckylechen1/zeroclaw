//! Channel factory seam: configured-channel collection, registry maps, and tool registration.
//!
//! Extracted from `orchestrator/mod.rs` so per-channel construction and agent-binding
//! gating can evolve independently of message processing and runtime dispatch.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::RwLock;
use zeroclaw_api::channel::Channel;
use zeroclaw_config::schema::Config;
use zeroclaw_runtime::tools;

#[cfg(feature = "channel-wecom-ws")]
use crate::wecom_ws::WeComWsRuntimePolicy;

#[cfg(feature = "channel-amqp")]
use super::AmqpChannel;
#[cfg(feature = "channel-bluesky")]
use super::BlueskyChannel;
#[cfg(feature = "channel-clawdtalk")]
use super::ClawdTalkChannel;
#[cfg(feature = "channel-dingtalk")]
use super::DingTalkChannel;
#[cfg(feature = "channel-discord")]
use super::DiscordChannel;
#[cfg(feature = "channel-email")]
use super::EmailChannel;
#[cfg(feature = "channel-git")]
use super::GitChannel;
#[cfg(feature = "channel-email")]
use super::GmailPushChannel;
#[cfg(feature = "channel-imessage")]
use super::IMessageChannel;
#[cfg(feature = "channel-irc")]
use super::IrcChannel;
#[cfg(feature = "channel-lark")]
use super::LarkChannel;
#[cfg(feature = "channel-line")]
use super::LineChannel;
#[cfg(feature = "channel-linq")]
use super::LinqChannel;
#[cfg(feature = "channel-matrix")]
use super::MatrixChannel;
#[cfg(feature = "channel-mattermost")]
use super::MattermostChannel;
#[cfg(feature = "channel-mochat")]
use super::MochatChannel;
#[cfg(feature = "channel-nextcloud")]
use super::NextcloudTalkChannel;
#[cfg(feature = "channel-notion")]
use super::NotionChannel;
#[cfg(feature = "channel-qq")]
use super::QQChannel;
#[cfg(feature = "channel-reddit")]
use super::RedditChannel;
#[cfg(feature = "channel-signal")]
use super::SignalChannel;
#[cfg(feature = "channel-slack")]
use super::SlackChannel;
#[cfg(feature = "channel-telegram")]
use super::TelegramChannel;
#[cfg(feature = "channel-twitch")]
use super::TwitchChannel;
#[cfg(feature = "channel-twitter")]
use super::TwitterChannel;
#[cfg(feature = "channel-voice-call")]
use super::VoiceCallChannel;
#[cfg(feature = "voice-wake")]
use super::VoiceWakeChannel;
#[cfg(feature = "channel-wati")]
use super::WatiChannel;
#[cfg(feature = "channel-wechat")]
use super::WeChatChannel;
#[cfg(feature = "channel-wecom")]
use super::WeComChannel;
#[cfg(feature = "channel-wecom-ws")]
use super::WeComWsChannel;
#[cfg(feature = "channel-webhook")]
use super::WebhookChannel;
#[cfg(feature = "channel-whatsapp-cloud")]
use super::WhatsAppChannel;
#[cfg(feature = "whatsapp-web")]
use super::WhatsAppWebChannel;

pub(crate) struct ConfiguredChannel {
    pub(crate) display_name: &'static str,
    pub(crate) alias: Option<String>,
    pub(crate) channel: Arc<dyn Channel>,
}

/// Compose the registry key for a channel given its `name()` and configured alias.
/// Aliased channels live at `<name>.<alias>`; un-aliased singletons keep the bare name.
pub(crate) fn composite_channel_key(name: &str, alias: Option<&str>) -> String {
    match alias.filter(|s| !s.is_empty()) {
        Some(alias) => format!("{name}.{alias}"),
        None => name.to_string(),
    }
}

pub(crate) fn configured_channel_map(
    configured: &[ConfiguredChannel],
) -> HashMap<String, Arc<dyn Channel>> {
    let mut map: HashMap<String, Arc<dyn Channel>> = HashMap::new();
    let mut name_counts: HashMap<&str, usize> = HashMap::new();
    for cc in configured {
        *name_counts.entry(cc.channel.name()).or_insert(0) += 1;
    }
    for cc in configured {
        let name = cc.channel.name();
        let composite = composite_channel_key(name, cc.alias.as_deref());
        map.insert(composite, Arc::clone(&cc.channel));
        if name_counts.get(name).copied().unwrap_or(0) == 1 {
            map.entry(name.to_string())
                .or_insert_with(|| Arc::clone(&cc.channel));
        }
    }
    map
}
/// Active `<type>.<alias>` channel references from enabled agents and SOP
/// approval routes.
///
/// When no agent declares channel bindings, collection falls back to legacy
/// behavior and accepts all enabled channels.
pub(crate) struct ActiveChannelAliases {
    /// `<type>.<alias>` declared by ENABLED agents. Drives `contains` in
    /// explicit-binding mode: only enabled owners' bindings count.
    enabled_bindings: HashSet<String>,
    /// Bindings declared by all agents, including disabled owners. Their
    /// presence prevents legacy fallback from activating disabled channels.
    all_known_bindings: HashSet<String>,
}

impl ActiveChannelAliases {
    /// Returns true when `channel_ref` is agent-bound, or when no explicit
    /// agent bindings exist and legacy "accept all enabled channels" mode
    /// applies.
    pub(crate) fn contains(&self, channel_ref: &str) -> bool {
        self.all_known_bindings.is_empty() || self.enabled_bindings.contains(channel_ref)
    }

    /// True when bindings exist somewhere in the config but every owner is
    /// `enabled = false`.
    fn disabled_owners_exist(&self) -> bool {
        !self.all_known_bindings.is_empty() && self.enabled_bindings.is_empty()
    }

    /// Computes the canonical channel-binding view used by collection and
    /// startup checks. Disabled owners never activate channels.
    pub(crate) fn compute(config: &Config) -> Self {
        Self {
            enabled_bindings: config
                .agents
                .values()
                .filter(|a| a.enabled)
                .flat_map(|a| a.channels.iter().map(|c| c.as_str().to_string()))
                .collect(),
            all_known_bindings: config
                .agents
                .values()
                .flat_map(|a| a.channels.iter().map(|c| c.as_str().to_string()))
                .collect(),
        }
    }
}

pub fn build_channel_map(
    config: &Config,
) -> HashMap<String, Arc<dyn zeroclaw_api::channel::Channel>> {
    let config_arc = Arc::new(RwLock::new(config.clone()));
    let configured = collect_configured_channels(&config_arc, "", &[]);
    configured_channel_map(&configured)
}

pub fn register_channels_for_tools(
    config: &Config,
    ask_user_handle: &Option<tools::PerToolChannelHandle>,
    channel_room_handle: &Option<tools::PerToolChannelHandle>,
    reaction_handle: &Option<tools::PerToolChannelHandle>,
    poll_handle: &Option<tools::PerToolChannelHandle>,
    escalate_handle: &Option<tools::PerToolChannelHandle>,
) -> Vec<String> {
    let config_arc = Arc::new(RwLock::new(config.clone()));
    let configured = collect_configured_channels(&config_arc, "", &[]);

    let handles = [
        ask_user_handle.as_ref(),
        channel_room_handle.as_ref(),
        reaction_handle.as_ref(),
        poll_handle.as_ref(),
        escalate_handle.as_ref(),
    ];

    let map = configured_channel_map(&configured);
    for (key, channel) in &map {
        for handle in handles.iter().flatten() {
            handle.write().insert(key.clone(), Arc::clone(channel));
        }
    }
    let mut names: Vec<String> = map.keys().cloned().collect();
    names.sort();
    names
}

/// Per-alias Matrix state directory. Each `[channels.matrix.<alias>]` block
/// must own its own session/crypto store so two bots under one daemon don't
/// restore each other's `session.json` and run as the wrong account. The
/// alias component is what keeps them distinct.
#[cfg(feature = "channel-matrix")]
pub(crate) fn matrix_state_dir(config_path: &std::path::Path, alias: &str) -> std::path::PathBuf {
    config_path
        .parent()
        .map(|p| p.join("state").join("matrix").join(alias))
        .unwrap_or_else(|| std::path::PathBuf::from(".zeroclaw/state/matrix").join(alias))
}

pub(crate) fn collect_configured_channels(
    config_arc: &Arc<RwLock<Config>>,
    matrix_skip_context: &str,
    tool_specs: &[(String, String)],
) -> Vec<ConfiguredChannel> {
    let _ = matrix_skip_context;
    let _ = tool_specs;
    #[allow(unused_mut)]
    let mut channels = Vec::new();

    // Shadow `config` with a read guard so the existing body keeps
    // working via `Deref<Target = Config>`. Resolver closures that
    // outlive the function capture `config_arc.clone()`.
    let config = config_arc.read();

    let active_channel_aliases = ActiveChannelAliases::compute(&config);

    if active_channel_aliases.disabled_owners_exist() {
        let skipped: Vec<&String> = active_channel_aliases.all_known_bindings.iter().collect();
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({
                    "skipped_bindings": skipped.len(),
                    "bindings": skipped,
                })),
            "channel binding(s) skipped: all owning agent(s) are disabled (#8013)"
        );
    }

    #[cfg(feature = "channel-telegram")]
    for (alias, tg) in &config.channels.telegram {
        if !active_channel_aliases.contains(&format!("telegram.{alias}")) {
            continue;
        }
        if !tg.enabled {
            continue;
        }
        let ack = tg.ack_reactions.unwrap_or(config.channels.ack_reactions);
        let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || cfg_arc.read().channel_external_peers("telegram", &alias))
        };
        let voice_peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || cfg_arc.read().channel_voice_peers("telegram", &alias))
        };
        let channel_key = format!("telegram.{alias}");
        let agent_transcription_provider = config
            .agents
            .values()
            .filter(|a| a.enabled && a.channels.iter().any(|c| c.as_str() == channel_key))
            .find_map(|a| {
                let s = a.transcription_provider.as_str();
                if s.is_empty() {
                    None
                } else {
                    Some(s.to_string())
                }
            })
            .unwrap_or_default();
        channels.push(ConfiguredChannel {
            display_name: "Telegram",
            alias: Some(alias.clone()),
            channel: crate::paced_channel::PacedChannel::wrap(
                Arc::new(
                    TelegramChannel::new(
                        tg.bot_token.clone(),
                        alias.clone(),
                        peer_resolver,
                        tg.mention_only,
                    )
                    .with_voice_peer_resolver(voice_peer_resolver)
                    .with_persistence(config_arc.clone())
                    .with_api_base(tg.api_base_url.clone())
                    .with_ack_reactions(ack)
                    .with_streaming(tg.stream_mode, tg.draft_update_interval_ms)
                    .with_transcription(config.transcription.clone())
                    .with_agent_transcription_provider(agent_transcription_provider.clone())
                    .with_typed_transcription_providers(
                        &config.providers.transcription,
                        &agent_transcription_provider,
                    )
                    .with_tts(&config)
                    .with_workspace_dir(config.channel_workspace_dir(&format!("telegram.{alias}")))
                    .with_proxy_url(tg.proxy_url.clone())
                    .with_tool_command_specs(tool_specs.to_vec())
                    .with_approval_timeout_secs(tg.approval_timeout_secs),
                ),
                tg,
            ),
        });
    }

    #[cfg(not(feature = "channel-telegram"))]
    if !config.channels.telegram.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "Telegram channel is configured but this build was compiled without \
             `channel-telegram`; skipping Telegram."
        );
    }

    #[cfg(feature = "channel-discord")]
    for (alias, dc) in &config.channels.discord {
        if !active_channel_aliases.contains(&format!("discord.{alias}")) {
            continue;
        }
        if !dc.enabled {
            continue;
        }
        let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || cfg_arc.read().channel_external_peers("discord", &alias))
        };
        let mut discord_ch = DiscordChannel::new(
            dc.bot_token.clone(),
            dc.guild_ids.clone(),
            alias.clone(),
            peer_resolver,
            dc.listen_to_bots,
            dc.mention_only,
        )
        .with_channel_ids(dc.channel_ids.clone())
        .with_workspace_dir(config.channel_workspace_dir(&format!("discord.{alias}")))
        .with_streaming(
            dc.stream_mode,
            dc.draft_update_interval_ms,
            dc.multi_message_delay_ms,
        )
        .with_proxy_url(dc.proxy_url.clone())
        .with_transcription(config.transcription.clone())
        .with_stall_timeout(dc.stall_timeout_secs)
        .with_approval_timeout_secs(dc.approval_timeout_secs)
        .with_slash_commands(dc.slash_commands)
        .with_slash_command_scope(dc.slash_command_scope)
        .with_intents_mask(dc.intents_mask)
        .with_reaction_notifications(dc.reaction_notifications);
        if dc.slash_commands {
            let cfg_arc_for_slash = config_arc.clone();
            let channel_ref = format!("discord.{alias}");
            discord_ch = discord_ch.with_slash_command_resolver(std::sync::Arc::new(move || {
                let config = { cfg_arc_for_slash.read().clone() };
                let Some(agent_alias) = config
                    .agent_for_channel(&channel_ref)
                    .map(ToString::to_string)
                else {
                    return Vec::new();
                };
                let workspace = config.agent_workspace_dir(&agent_alias);
                let skills = zeroclaw_runtime::skills::load_skills_for_agent(
                    &workspace,
                    &config,
                    &agent_alias,
                );
                crate::discord::discord_slash_specs_from_skills(&skills)
            }));
        }
        if dc.archive {
            match zeroclaw_memory::SqliteMemory::new_named("sqlite", &config.data_dir, "discord") {
                Ok(mem) => {
                    discord_ch = discord_ch.with_archive_memory(std::sync::Arc::new(mem));
                }
                Err(e) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                        "discord: archive enabled but failed to open discord.db"
                    );
                }
            }
        }
        channels.push(ConfiguredChannel {
            display_name: "Discord",
            alias: Some(alias.clone()),
            channel: crate::paced_channel::PacedChannel::wrap(Arc::new(discord_ch), dc),
        });
    }

    #[cfg(not(feature = "channel-discord"))]
    if !config.channels.discord.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "Discord channel is configured but this build was compiled without \
             `channel-discord`; skipping Discord."
        );
    }

    #[cfg(feature = "channel-slack")]
    for (alias, sl) in &config.channels.slack {
        if !active_channel_aliases.contains(&format!("slack.{alias}")) {
            continue;
        }
        if !sl.enabled {
            continue;
        }
        let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || cfg_arc.read().channel_external_peers("slack", &alias))
        };
        let Some(bot_token) = sl.resolved_bot_token() else {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({ "alias": alias.clone() })),
                "Slack channel skipped: bot_token not set in config or via \
                 ZEROCLAW_SLACK_BOT_TOKEN / SLACK_BOT_TOKEN env"
            );
            continue;
        };
        channels.push(ConfiguredChannel {
            display_name: "Slack",
            alias: Some(alias.clone()),
            channel: crate::paced_channel::PacedChannel::wrap(
                Arc::new(
                    SlackChannel::new(
                        bot_token,
                        sl.resolved_app_token(),
                        sl.channel_ids.clone(),
                        alias.clone(),
                        peer_resolver,
                    )
                    .with_thread_replies(sl.thread_replies.unwrap_or(true))
                    .with_group_reply_policy(sl.mention_only, Vec::new())
                    .with_strict_mention_in_thread(sl.strict_mention_in_thread)
                    .with_workspace_dir(config.channel_workspace_dir(&format!("slack.{alias}")))
                    .with_markdown_blocks(sl.use_markdown_blocks)
                    .with_proxy_url(sl.proxy_url.clone())
                    .with_transcription(config.transcription.clone())
                    .with_streaming(sl.stream_drafts, sl.draft_update_interval_ms)
                    .with_cancel_reaction(sl.cancel_reaction.clone())
                    .with_approval_timeout_secs(sl.approval_timeout_secs),
                ),
                sl,
            ),
        });
    }

    #[cfg(not(feature = "channel-slack"))]
    if !config.channels.slack.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "Slack channel is configured but this build was compiled without \
             `channel-slack`; skipping Slack."
        );
    }

    #[cfg(feature = "channel-mattermost")]
    for (alias, mm) in &config.channels.mattermost {
        if !active_channel_aliases.contains(&format!("mattermost.{alias}")) {
            continue;
        }
        if !mm.enabled {
            continue;
        }
        let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || cfg_arc.read().channel_external_peers("mattermost", &alias))
        };
        channels.push(ConfiguredChannel {
            display_name: "Mattermost",
            alias: Some(alias.clone()),
            channel: crate::paced_channel::PacedChannel::wrap(
                Arc::new(
                    MattermostChannel::new(
                        mm.url.clone(),
                        mm.bot_token.clone(),
                        mm.login_id.clone(),
                        mm.password.clone(),
                        mm.channel_ids.clone(),
                        alias.clone(),
                        peer_resolver,
                        mm.thread_replies.unwrap_or(true),
                        mm.mention_only.unwrap_or(false),
                    )
                    .with_team_ids(mm.team_ids.clone())
                    .with_discover_dms(mm.discover_dms.unwrap_or(true))
                    .with_proxy_url(mm.proxy_url.clone())
                    .with_transcription(config.transcription.clone())
                    .with_listen_mode(mm.listen_mode),
                ),
                mm,
            ),
        });
    }

    #[cfg(not(feature = "channel-mattermost"))]
    if !config.channels.mattermost.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "Mattermost channel is configured but this build was compiled without \
             `channel-mattermost`; skipping Mattermost."
        );
    }

    #[cfg(feature = "channel-imessage")]
    for (alias, im) in &config.channels.imessage {
        if !active_channel_aliases.contains(&format!("imessage.{alias}")) {
            continue;
        }
        if !im.enabled {
            continue;
        }
        let _ = im;
        let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || cfg_arc.read().channel_external_peers("imessage", &alias))
        };
        channels.push(ConfiguredChannel {
            display_name: "iMessage",
            alias: Some(alias.clone()),
            channel: crate::paced_channel::PacedChannel::wrap(
                Arc::new(IMessageChannel::new(alias.clone(), peer_resolver)),
                im,
            ),
        });
    }

    #[cfg(not(feature = "channel-imessage"))]
    if !config.channels.imessage.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "iMessage channel is configured but this build was compiled without \
             `channel-imessage`; skipping iMessage."
        );
    }

    #[cfg(feature = "channel-matrix")]
    for (alias, mx) in &config.channels.matrix {
        if !active_channel_aliases.contains(&format!("matrix.{alias}")) {
            continue;
        }
        if !mx.enabled {
            continue;
        }
        let state_dir = matrix_state_dir(&config.config_path, alias);
        let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || cfg_arc.read().channel_external_peers("matrix", &alias))
        };
        let ack = mx.ack_reactions.unwrap_or(config.channels.ack_reactions);
        match MatrixChannel::new(mx.clone(), alias.clone(), peer_resolver, state_dir) {
            Ok(channel) => {
                let channel = channel
                    .with_transcription(config.transcription.clone())
                    .with_workspace_dir(config.channel_workspace_dir(&format!("matrix.{alias}")))
                    .with_ack_reactions(ack);
                channels.push(ConfiguredChannel {
                    display_name: "Matrix",
                    alias: Some(alias.clone()),
                    channel: crate::paced_channel::PacedChannel::wrap(Arc::new(channel), mx),
                });
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "Matrix channel construction failed"
                );
            }
        }
    }

    #[cfg(not(feature = "channel-matrix"))]
    if !config.channels.matrix.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            &format!(
                "Matrix channel is configured but this build was compiled without `channel-matrix`; skipping Matrix {}.",
                matrix_skip_context
            )
        );
    }

    #[cfg(feature = "channel-signal")]
    for (alias, sig) in &config.channels.signal {
        if !active_channel_aliases.contains(&format!("signal.{alias}")) {
            continue;
        }
        if !sig.enabled {
            continue;
        }
        let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || cfg_arc.read().channel_external_peers("signal", &alias))
        };
        channels.push(ConfiguredChannel {
            display_name: "Signal",
            alias: Some(alias.clone()),
            channel: crate::paced_channel::PacedChannel::wrap(
                Arc::new(
                    SignalChannel::new(
                        sig.http_url.clone(),
                        sig.account.clone(),
                        sig.group_ids.clone(),
                        sig.dm_only,
                        alias.clone(),
                        peer_resolver,
                        sig.ignore_attachments,
                        sig.ignore_stories,
                    )
                    .with_proxy_url(sig.proxy_url.clone())
                    .with_approval_timeout_secs(sig.approval_timeout_secs),
                ),
                sig,
            ),
        });
    }

    #[cfg(not(feature = "channel-signal"))]
    if !config.channels.signal.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "Signal channel is configured but this build was compiled without \
             `channel-signal`; skipping Signal."
        );
    }

    #[cfg(any(feature = "channel-whatsapp-cloud", feature = "whatsapp-web"))]
    for (alias, wa) in &config.channels.whatsapp {
        if !active_channel_aliases.contains(&format!("whatsapp.{alias}")) {
            continue;
        }
        if !wa.enabled {
            continue;
        }
        if wa.is_ambiguous_config() {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "WhatsApp config has both phone_number_id (Cloud) and a Web selector (session_path/pair_phone/pair_code/ws_url/mode=personal) set; preferring Cloud API mode. Remove one selector to avoid ambiguity."
            );
        }
        // Runtime negotiation: detect backend type from config
        match wa.backend_type() {
            #[cfg(feature = "channel-whatsapp-cloud")]
            "cloud" => {
                // Cloud API mode: requires phone_number_id, access_token, verify_token
                if wa.is_cloud_config() {
                    let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                        let cfg_arc = config_arc.clone();
                        let alias = alias.clone();
                        Arc::new(move || cfg_arc.read().channel_external_peers("whatsapp", &alias))
                    };
                    channels.push(ConfiguredChannel {
                        display_name: "WhatsApp",
                        alias: Some(alias.clone()),
                        channel: crate::paced_channel::PacedChannel::wrap(
                            Arc::new(
                                WhatsAppChannel::new(
                                    wa.access_token.clone().unwrap_or_default(),
                                    wa.phone_number_id.clone().unwrap_or_default(),
                                    wa.verify_token.clone().unwrap_or_default(),
                                    alias.clone(),
                                    peer_resolver,
                                )
                                .with_proxy_url(wa.proxy_url.clone())
                                .with_dm_mention_patterns(wa.dm_mention_patterns.clone())
                                .with_group_mention_patterns(wa.group_mention_patterns.clone())
                                .with_approval_timeout_secs(wa.approval_timeout_secs),
                            ),
                            wa,
                        ),
                    });
                } else {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                        "WhatsApp Cloud API configured but missing required fields (phone_number_id, access_token, verify_token)"
                    );
                }
                #[cfg(not(feature = "channel-whatsapp-cloud"))]
                {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                        "WhatsApp Cloud API backend requires 'channel-whatsapp-cloud' feature. Build/run with --features channel-whatsapp-cloud"
                    );
                }
            }
            #[cfg(not(feature = "channel-whatsapp-cloud"))]
            "cloud" => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                    "WhatsApp Cloud API is configured but this build was compiled without `channel-whatsapp-cloud`; skipping WhatsApp Cloud."
                );
            }
            "web" => {
                // Web mode: requires session_path
                #[cfg(feature = "whatsapp-web")]
                if wa.is_web_config() {
                    let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                        let cfg_arc = config_arc.clone();
                        let alias = alias.clone();
                        Arc::new(move || cfg_arc.read().channel_external_peers("whatsapp", &alias))
                    };
                    let workspace_dir = config.channel_workspace_dir(&format!("whatsapp.{alias}"));
                    let allowed_groups_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                        let cfg_arc = config_arc.clone();
                        let alias = alias.clone();
                        Arc::new(move || {
                            cfg_arc
                                .read()
                                .channels
                                .whatsapp
                                .get(&alias)
                                .map(|wa| wa.allowed_groups.clone())
                                .unwrap_or_default()
                        })
                    };
                    channels.push(ConfiguredChannel {
                        display_name: "WhatsApp",
                        alias: Some(alias.clone()),
                        channel: crate::paced_channel::PacedChannel::wrap(
                            Arc::new(
                                WhatsAppWebChannel::new(
                                    wa,
                                    alias.clone(),
                                    peer_resolver,
                                    allowed_groups_resolver,
                                )
                                .with_persistence(config_arc.clone())
                                .with_transcription(config.transcription.clone())
                                .with_tts(&config)
                                .with_workspace_dir(workspace_dir)
                                .with_dm_mention_patterns(wa.dm_mention_patterns.clone())
                                .with_group_mention_patterns(wa.group_mention_patterns.clone()),
                            ),
                            wa,
                        ),
                    });
                } else {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                        "WhatsApp Web configured but session_path not set"
                    );
                }
                #[cfg(not(feature = "whatsapp-web"))]
                {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                        "WhatsApp Web backend requires 'whatsapp-web' feature. Enable with: cargo build --features whatsapp-web"
                    );
                    eprintln!(
                        "  ⚠ WhatsApp Web is configured but the 'whatsapp-web' feature is not compiled in."
                    );
                    eprintln!("    Rebuild with: cargo build --features whatsapp-web");
                }
            }
            _ => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                    "WhatsApp config invalid: neither phone_number_id (Cloud API) nor session_path (Web) is set"
                );
            }
        }
    }

    #[cfg(feature = "channel-linq")]
    for (alias, lq) in &config.channels.linq {
        if !active_channel_aliases.contains(&format!("linq.{alias}")) {
            continue;
        }
        if !lq.enabled {
            continue;
        }
        let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || cfg_arc.read().channel_external_peers("linq", &alias))
        };
        channels.push(ConfiguredChannel {
            display_name: "Linq",
            alias: Some(alias.clone()),
            channel: Arc::new(LinqChannel::new(
                lq.api_token.clone(),
                lq.from_phone.clone(),
                alias.clone(),
                peer_resolver,
            )),
        });
    }

    #[cfg(not(feature = "channel-linq"))]
    if !config.channels.linq.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "Linq channel is configured but this build was compiled without \
             `channel-linq`; skipping Linq."
        );
    }

    #[cfg(feature = "channel-wati")]
    for (alias, wati_cfg) in &config.channels.wati {
        if !active_channel_aliases.contains(&format!("wati.{alias}")) {
            continue;
        }
        if !wati_cfg.enabled {
            continue;
        }
        let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || cfg_arc.read().channel_external_peers("wati", &alias))
        };
        let wati_channel = WatiChannel::new_with_proxy(
            wati_cfg.api_token.clone(),
            wati_cfg.api_url.clone(),
            wati_cfg.tenant_id.clone(),
            alias.clone(),
            peer_resolver,
            wati_cfg.proxy_url.clone(),
        )
        .with_transcription(config.transcription.clone());
        channels.push(ConfiguredChannel {
            display_name: "WATI",
            alias: Some(alias.clone()),
            channel: Arc::new(wati_channel),
        });
    }

    #[cfg(not(feature = "channel-wati"))]
    if !config.channels.wati.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "WATI channel is configured but this build was compiled without \
             `channel-wati`; skipping WATI."
        );
    }

    #[cfg(feature = "channel-nextcloud")]
    for (alias, nc) in &config.channels.nextcloud_talk {
        if !active_channel_aliases.contains(&format!("nextcloud_talk.{alias}")) {
            continue;
        }
        if !nc.enabled {
            continue;
        }
        let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || {
                cfg_arc
                    .read()
                    .channel_external_peers("nextcloud_talk", &alias)
            })
        };
        channels.push(ConfiguredChannel {
            display_name: "Nextcloud Talk",
            alias: Some(alias.clone()),
            channel: Arc::new(NextcloudTalkChannel::new_with_proxy(
                nc.base_url.clone(),
                nc.app_token.clone(),
                nc.bot_name.clone().unwrap_or_default(),
                alias.clone(),
                peer_resolver,
                nc.proxy_url.clone(),
            )),
        });
    }

    #[cfg(not(feature = "channel-nextcloud"))]
    if !config.channels.nextcloud_talk.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "Nextcloud Talk channel is configured but this build was compiled without \
             `channel-nextcloud`; skipping Nextcloud Talk."
        );
    }

    #[cfg(feature = "channel-email")]
    {
        // Construct once and share across all email channel instances.
        let auth_service = Arc::new(zeroclaw_providers::auth::AuthService::from_config(&config));

        for (alias, email_cfg) in &config.channels.email {
            if !active_channel_aliases.contains(&format!("email.{alias}")) {
                continue;
            }
            if !email_cfg.enabled {
                continue;
            }
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.clone();
                Arc::new(move || cfg_arc.read().channel_external_peers("email", &alias))
            };
            let mut channel = EmailChannel::new(email_cfg.clone(), alias.clone(), peer_resolver);
            if email_cfg.oauth2.is_some() {
                channel = channel.with_auth_service(auth_service.clone());
            }
            channels.push(ConfiguredChannel {
                display_name: "Email",
                alias: Some(alias.clone()),
                channel: Arc::new(channel),
            });
        }
    }

    #[cfg(feature = "channel-email")]
    for (alias, gp_cfg) in &config.channels.gmail_push {
        if !active_channel_aliases.contains(&format!("gmail_push.{alias}")) {
            continue;
        }
        if !gp_cfg.enabled {
            continue;
        }
        let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || cfg_arc.read().channel_external_peers("gmail_push", &alias))
        };
        channels.push(ConfiguredChannel {
            display_name: "Gmail Push",
            alias: Some(alias.clone()),
            channel: Arc::new(GmailPushChannel::new(
                gp_cfg.clone(),
                alias.clone(),
                peer_resolver,
            )),
        });
    }

    #[cfg(not(feature = "channel-email"))]
    if !config.channels.email.is_empty() || !config.channels.gmail_push.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "Email/Gmail Push channel is configured but this build was compiled without \
             `channel-email`; skipping Email and Gmail Push."
        );
    }

    #[cfg(feature = "channel-irc")]
    for (alias, irc) in &config.channels.irc {
        if !active_channel_aliases.contains(&format!("irc.{alias}")) {
            continue;
        }
        if !irc.enabled {
            continue;
        }
        let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || cfg_arc.read().channel_external_peers("irc", &alias))
        };
        channels.push(ConfiguredChannel {
            display_name: "IRC",
            alias: Some(alias.clone()),
            channel: Arc::new(IrcChannel::new(crate::irc::IrcChannelConfig {
                server: irc.server.clone(),
                port: irc.port,
                nickname: irc.nickname.clone(),
                username: irc.username.clone(),
                channels: irc.channels.clone(),
                alias: alias.clone(),
                peer_resolver,
                server_password: irc.server_password.clone(),
                nickserv_password: irc.nickserv_password.clone(),
                sasl_password: irc.sasl_password.clone(),
                verify_tls: irc.verify_tls.unwrap_or(true),
                mention_only: irc.mention_only,
            })),
        });
    }

    #[cfg(not(feature = "channel-irc"))]
    if !config.channels.irc.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "IRC channel is configured but this build was compiled without \
             `channel-irc`; skipping IRC."
        );
    }

    #[cfg(feature = "channel-amqp")]
    for (alias, amqp) in &config.channels.amqp {
        if !active_channel_aliases.contains(&format!("amqp.{alias}")) {
            continue;
        }
        if !amqp.enabled {
            continue;
        }
        let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || cfg_arc.read().channel_external_peers("amqp", &alias))
        };
        let amqp_channel = match AmqpChannel::new(crate::amqp::AmqpChannelConfig {
            amqp_url: amqp.amqp_url.clone(),
            exchange: amqp.exchange.clone(),
            routing_keys: amqp.routing_keys.clone(),
            queue: amqp.queue.clone(),
            ca_cert: amqp.ca_cert.clone(),
            client_cert: amqp.client_cert.clone(),
            client_key: amqp.client_key.clone(),
            sender_label: amqp.sender_label.clone(),
            content_template: amqp.content_template.clone(),
            thread_id_field: amqp.thread_id_field.clone(),
            durable_ack: amqp.durable_ack,
            alias: alias.clone(),
            peer_resolver,
        }) {
            Ok(ch) => ch,
            Err(err) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "alias": alias,
                            "error": err.to_string(),
                        })),
                    "skipping AMQP channel: SOP dispatch without engine/audit handles"
                );
                continue;
            }
        };
        channels.push(ConfiguredChannel {
            display_name: "AMQP",
            alias: Some(alias.clone()),
            channel: Arc::new(amqp_channel),
        });
    }

    #[cfg(not(feature = "channel-amqp"))]
    if !config.channels.amqp.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "AMQP channel is configured but this build was compiled without \
             `channel-amqp`; skipping AMQP."
        );
    }

    #[cfg(feature = "channel-twitch")]
    for (alias, tw) in &config.channels.twitch {
        if !active_channel_aliases.contains(&format!("twitch.{alias}")) {
            continue;
        }
        if !tw.enabled {
            continue;
        }
        let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || cfg_arc.read().channel_external_peers("twitch", &alias))
        };
        channels.push(ConfiguredChannel {
            display_name: "Twitch",
            alias: Some(alias.clone()),
            channel: Arc::new(TwitchChannel::new(
                tw.bot_username.clone(),
                tw.oauth_token.clone(),
                tw.channels.clone(),
                tw.mention_only,
                alias.clone(),
                peer_resolver,
            )),
        });
    }

    #[cfg(not(feature = "channel-twitch"))]
    if !config.channels.twitch.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "Twitch channel is configured but this build was compiled without \
             `channel-twitch`; skipping Twitch."
        );
    }

    #[cfg(feature = "channel-lark")]
    for (alias, lk) in &config.channels.lark {
        if !active_channel_aliases.contains(&format!("lark.{alias}")) {
            continue;
        }
        if !lk.enabled {
            continue;
        }
        let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || cfg_arc.read().channel_external_peers("lark", &alias))
        };
        let display_name = if lk.use_feishu { "Feishu" } else { "Lark" };
        channels.push(ConfiguredChannel {
            display_name,
            alias: Some(alias.clone()),
            channel: Arc::new(
                LarkChannel::from_config(lk, alias.clone(), peer_resolver)
                    .with_workspace_dir(config.channel_workspace_dir(&format!("lark.{alias}")))
                    .with_approval_timeout_secs(lk.approval_timeout_secs)
                    .with_per_user_session(lk.per_user_session)
                    .with_ack_reactions(lk.ack_reactions.unwrap_or(config.channels.ack_reactions))
                    .with_streaming(lk.stream_mode, lk.draft_update_interval_ms)
                    .with_transcription(config.transcription.clone()),
            ),
        });
    }

    #[cfg(not(feature = "channel-lark"))]
    if !config.channels.lark.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "Lark/Feishu channel is configured but this build was compiled without `channel-lark`; skipping Lark/Feishu health check."
        );
    }

    #[cfg(feature = "channel-line")]
    for (alias, ln) in &config.channels.line {
        if !active_channel_aliases.contains(&format!("line.{alias}")) {
            continue;
        }
        if !ln.enabled {
            continue;
        }
        let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || cfg_arc.read().channel_external_peers("line", &alias))
        };
        let sender_name_resolver: Arc<dyn Fn() -> Option<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || {
                cfg_arc
                    .read()
                    .channels
                    .line
                    .get(&alias)
                    .and_then(|ln| ln.sender_name.clone())
                    .filter(|s| !s.is_empty())
            })
        };
        channels.push(ConfiguredChannel {
            display_name: "LINE",
            alias: Some(alias.clone()),
            channel: Arc::new(
                LineChannel::from_config(ln, alias.clone(), peer_resolver, sender_name_resolver)
                    .with_persistence(config_arc.clone())
                    .with_transcription(config.transcription.clone()),
            ),
        });
    }

    #[cfg(not(feature = "channel-line"))]
    if !config.channels.line.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "LINE channel is configured but this build was compiled without `channel-line`; skipping LINE health check."
        );
    }

    #[cfg(feature = "channel-dingtalk")]
    for (alias, dt) in &config.channels.dingtalk {
        if !active_channel_aliases.contains(&format!("dingtalk.{alias}")) {
            continue;
        }
        if !dt.enabled {
            continue;
        }
        let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || cfg_arc.read().channel_external_peers("dingtalk", &alias))
        };
        channels.push(ConfiguredChannel {
            display_name: "DingTalk",
            alias: Some(alias.clone()),
            channel: Arc::new(
                DingTalkChannel::new(
                    dt.client_id.clone(),
                    dt.client_secret.clone(),
                    alias.clone(),
                    peer_resolver,
                )
                .with_proxy_url(dt.proxy_url.clone()),
            ),
        });
    }

    #[cfg(not(feature = "channel-dingtalk"))]
    if !config.channels.dingtalk.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "DingTalk channel is configured but this build was compiled without \
             `channel-dingtalk`; skipping DingTalk."
        );
    }

    #[cfg(feature = "channel-qq")]
    for (alias, qq) in &config.channels.qq {
        if !active_channel_aliases.contains(&format!("qq.{alias}")) {
            continue;
        }
        if !qq.enabled {
            continue;
        }
        let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || cfg_arc.read().channel_external_peers("qq", &alias))
        };
        channels.push(ConfiguredChannel {
            display_name: "QQ",
            alias: Some(alias.clone()),
            channel: Arc::new(
                QQChannel::new(
                    qq.app_id.clone(),
                    qq.app_secret.clone(),
                    alias.clone(),
                    peer_resolver,
                )
                .with_workspace_dir(config.channel_workspace_dir(&format!("qq.{alias}")))
                .with_proxy_url(qq.proxy_url.clone())
                .with_transcription(config.transcription.clone()),
            ),
        });
    }

    #[cfg(not(feature = "channel-qq"))]
    if !config.channels.qq.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "QQ channel is configured but this build was compiled without \
             `channel-qq`; skipping QQ."
        );
    }

    #[cfg(feature = "channel-twitter")]
    for (alias, tw) in &config.channels.twitter {
        if !active_channel_aliases.contains(&format!("twitter.{alias}")) {
            continue;
        }
        if !tw.enabled {
            continue;
        }
        let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || cfg_arc.read().channel_external_peers("twitter", &alias))
        };
        channels.push(ConfiguredChannel {
            display_name: "X/Twitter",
            alias: Some(alias.clone()),
            channel: Arc::new(TwitterChannel::new(
                tw.bearer_token.clone(),
                alias.clone(),
                peer_resolver,
            )),
        });
    }

    #[cfg(not(feature = "channel-twitter"))]
    if !config.channels.twitter.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "X/Twitter channel is configured but this build was compiled without \
             `channel-twitter`; skipping X/Twitter."
        );
    }

    #[cfg(feature = "channel-git")]
    for (alias, g) in &config.channels.git {
        if !active_channel_aliases.contains(&format!("git.{alias}")) {
            continue;
        }
        if !g.enabled {
            continue;
        }
        let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || cfg_arc.read().channel_external_peers("git", &alias))
        };
        match GitChannel::new(g.clone(), alias.clone(), peer_resolver) {
            Ok(channel) => channels.push(ConfiguredChannel {
                display_name: "Git",
                alias: Some(alias.clone()),
                channel: Arc::new(channel),
            }),
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "alias": alias,
                            "error": e.to_string(),
                        })),
                    "Git channel alias misconfigured; skipping"
                );
            }
        }
    }

    #[cfg(not(feature = "channel-git"))]
    if !config.channels.git.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "Git channel is configured but this build was compiled without \
             `channel-git`; skipping Git."
        );
    }

    #[cfg(feature = "channel-mochat")]
    for (alias, mc) in &config.channels.mochat {
        if !active_channel_aliases.contains(&format!("mochat.{alias}")) {
            continue;
        }
        if !mc.enabled {
            continue;
        }
        let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || cfg_arc.read().channel_external_peers("mochat", &alias))
        };
        channels.push(ConfiguredChannel {
            display_name: "Mochat",
            alias: Some(alias.clone()),
            channel: Arc::new(MochatChannel::new(
                mc.api_url.clone(),
                mc.api_token.clone(),
                alias.clone(),
                peer_resolver,
                mc.poll_interval_secs,
            )),
        });
    }

    #[cfg(not(feature = "channel-mochat"))]
    if !config.channels.mochat.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "Mochat channel is configured but this build was compiled without \
             `channel-mochat`; skipping Mochat."
        );
    }

    #[cfg(feature = "channel-wecom")]
    for (alias, wc) in &config.channels.wecom {
        if !active_channel_aliases.contains(&format!("wecom.{alias}")) {
            continue;
        }
        if !wc.enabled {
            continue;
        }
        let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || cfg_arc.read().channel_external_peers("wecom", &alias))
        };
        channels.push(ConfiguredChannel {
            display_name: "WeCom",
            alias: Some(alias.clone()),
            channel: Arc::new(WeComChannel::new(
                wc.webhook_key.clone(),
                alias.clone(),
                peer_resolver,
            )),
        });
    }

    #[cfg(not(feature = "channel-wecom"))]
    if !config.channels.wecom.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "WeCom channel is configured but this build was compiled without \
             `channel-wecom`; skipping WeCom."
        );
    }

    #[cfg(feature = "channel-wecom-ws")]
    for (alias, wc_ws) in &config.channels.wecom_ws {
        if !active_channel_aliases.contains(&format!("wecom_ws.{alias}"))
            && !active_channel_aliases.contains(&format!("wecom-ws.{alias}"))
        {
            continue;
        }
        if !wc_ws.enabled {
            continue;
        }
        let policy_resolver: Arc<dyn Fn() -> WeComWsRuntimePolicy + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            let snapshot = wc_ws.clone();
            Arc::new(move || {
                let config = cfg_arc.read();
                let mut external_peers = config.channel_external_peers("wecom-ws", &alias);
                external_peers.extend(config.channel_external_peers("wecom_ws", &alias));

                if let Some(wc_ws) = config.channels.wecom_ws.get(&alias) {
                    WeComWsRuntimePolicy::from_config(wc_ws, external_peers)
                } else {
                    WeComWsRuntimePolicy::from_config(&snapshot, external_peers)
                }
            })
        };
        match WeComWsChannel::new_with_alias(
            wc_ws,
            alias.clone(),
            policy_resolver,
            &config.channel_workspace_dir(&format!("wecom_ws.{alias}")),
        ) {
            Ok(channel) => channels.push(ConfiguredChannel {
                display_name: "WeCom WebSocket",
                alias: Some(alias.clone()),
                channel: Arc::new(channel),
            }),
            Err(err) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": format!("{err:#}")})),
                    format!(
                        "WeCom WebSocket channel configuration is invalid; skipping WeCom WebSocket {matrix_skip_context}"
                    ),
                );
            }
        }
    }

    #[cfg(not(feature = "channel-wecom-ws"))]
    if !config.channels.wecom_ws.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            format!(
                "WeCom WebSocket channel is configured but this build was compiled without `channel-wecom-ws`; skipping WeCom WebSocket {matrix_skip_context}."
            ),
        );
    }

    #[cfg(feature = "channel-wechat")]
    for (alias, wechat) in &config.channels.wechat {
        if !active_channel_aliases.contains(&format!("wechat.{alias}")) {
            continue;
        }
        if !wechat.enabled {
            continue;
        }
        let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
            let cfg_arc = config_arc.clone();
            let alias = alias.clone();
            Arc::new(move || cfg_arc.read().channel_external_peers("wechat", &alias))
        };
        match WeChatChannel::new(
            alias.clone(),
            peer_resolver,
            wechat.api_base_url.clone(),
            wechat.cdn_base_url.clone(),
            Some(WeChatChannel::resolve_state_dir(
                wechat.state_dir.as_deref(),
            )),
        ) {
            Ok(channel) => {
                channels.push(ConfiguredChannel {
                    display_name: "WeChat",
                    alias: Some(alias.clone()),
                    channel: Arc::new(
                        channel
                            .with_persistence(config_arc.clone())
                            .with_workspace_dir(
                                config.channel_workspace_dir(&format!("wechat.{alias}")),
                            ),
                    ),
                });
            }
            Err(err) => {
                ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"matrix_skip_context": matrix_skip_context, "err": err.to_string()})), "WeChat channel configuration is invalid; skipping WeChat");
            }
        }
    }

    #[cfg(not(feature = "channel-wechat"))]
    for alias in config.channels.wechat.keys() {
        if active_channel_aliases.contains(&format!("wechat.{alias}")) {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"matrix_skip_context": matrix_skip_context})),
                "WeChat channel is configured but this build was compiled without `channel-wechat`; skipping WeChat ."
            );
        }
    }

    #[cfg(feature = "channel-clawdtalk")]
    for (alias, ct) in &config.channels.clawdtalk {
        if !active_channel_aliases.contains(&format!("clawdtalk.{alias}")) {
            continue;
        }
        if !ct.enabled {
            continue;
        }
        channels.push(ConfiguredChannel {
            display_name: "ClawdTalk",
            alias: Some(alias.clone()),
            channel: Arc::new(ClawdTalkChannel::new(alias.clone(), ct.clone())),
        });
    }

    #[cfg(not(feature = "channel-clawdtalk"))]
    if !config.channels.clawdtalk.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "ClawdTalk channel is configured but this build was compiled without \
             `channel-clawdtalk`; skipping ClawdTalk."
        );
    }

    // Notion database poller channel
    #[cfg(feature = "channel-notion")]
    if config.notion.enabled && !config.notion.database_id.trim().is_empty() {
        let notion_api_key = config.notion.api_key.trim().to_string();
        if notion_api_key.is_empty() {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "Notion channel enabled but `notion.api_key` is unset. Set it via the schema-mirror grammar: \
                 `ZEROCLAW_notion__api_key=...`."
            );
        } else {
            channels.push(ConfiguredChannel {
                display_name: "Notion",
                alias: None,
                channel: Arc::new(NotionChannel::new(
                    "notion",
                    notion_api_key,
                    config.notion.database_id.clone(),
                    config.notion.poll_interval_secs,
                    config.notion.status_property.clone(),
                    config.notion.input_property.clone(),
                    config.notion.result_property.clone(),
                    config.notion.max_concurrent,
                    config.notion.recover_stale,
                )),
            });
        }
    }

    #[cfg(not(feature = "channel-notion"))]
    if config.notion.enabled {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "Notion channel is enabled but this build was compiled without \
             `channel-notion`; skipping Notion."
        );
    }

    #[cfg(feature = "channel-reddit")]
    for (alias, rd) in &config.channels.reddit {
        if !active_channel_aliases.contains(&format!("reddit.{alias}")) {
            continue;
        }
        if !rd.enabled {
            continue;
        }
        channels.push(ConfiguredChannel {
            display_name: "Reddit",
            alias: Some(alias.clone()),
            channel: Arc::new(RedditChannel::new(
                alias.clone(),
                rd.client_id.clone(),
                rd.client_secret.clone(),
                rd.refresh_token.clone(),
                rd.username.clone(),
                rd.subreddits.clone(),
            )),
        });
    }

    #[cfg(not(feature = "channel-reddit"))]
    if !config.channels.reddit.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "Reddit channel is configured but this build was compiled without \
             `channel-reddit`; skipping Reddit."
        );
    }

    #[cfg(feature = "channel-bluesky")]
    for (alias, bs) in &config.channels.bluesky {
        if !active_channel_aliases.contains(&format!("bluesky.{alias}")) {
            continue;
        }
        if !bs.enabled {
            continue;
        }
        channels.push(ConfiguredChannel {
            display_name: "Bluesky",
            alias: Some(alias.clone()),
            channel: Arc::new(BlueskyChannel::new(
                alias.clone(),
                bs.handle.clone(),
                bs.app_password.clone(),
            )),
        });
    }

    #[cfg(not(feature = "channel-bluesky"))]
    if !config.channels.bluesky.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "Bluesky channel is configured but this build was compiled without \
             `channel-bluesky`; skipping Bluesky."
        );
    }

    #[cfg(feature = "voice-wake")]
    for (alias, vw) in &config.channels.voice_wake {
        if !active_channel_aliases.contains(&format!("voice_wake.{alias}")) {
            continue;
        }
        if !vw.enabled {
            continue;
        }
        channels.push(ConfiguredChannel {
            display_name: "VoiceWake",
            alias: Some(alias.clone()),
            channel: Arc::new(VoiceWakeChannel::new(
                alias.clone(),
                vw.clone(),
                config.transcription.clone(),
            )),
        });
    }

    #[cfg(not(feature = "voice-wake"))]
    if !config.channels.voice_wake.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "VoiceWake channel is configured but this build was compiled without \
             `voice-wake`; skipping VoiceWake."
        );
    }

    #[cfg(feature = "channel-voice-call")]
    for (alias, vc) in &config.channels.voice_call {
        if !active_channel_aliases.contains(&format!("voice_call.{alias}")) {
            continue;
        }
        if !vc.enabled {
            continue;
        }
        channels.push(ConfiguredChannel {
            display_name: "Voice Call",
            alias: Some(alias.clone()),
            channel: Arc::new(VoiceCallChannel::new(alias.clone(), vc.clone())),
        });
    }

    #[cfg(not(feature = "channel-voice-call"))]
    if !config.channels.voice_call.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "Voice Call channel is configured but this build was compiled without \
             `channel-voice-call`; skipping Voice Call."
        );
    }

    #[cfg(feature = "channel-webhook")]
    for (alias, wh) in &config.channels.webhook {
        if !active_channel_aliases.contains(&format!("webhook.{alias}")) {
            continue;
        }
        if !wh.enabled {
            continue;
        }
        channels.push(ConfiguredChannel {
            display_name: "Webhook",
            alias: Some(alias.clone()),
            channel: crate::paced_channel::PacedChannel::wrap(
                Arc::new(WebhookChannel::new(
                    alias.clone(),
                    wh.port,
                    wh.listen_path.clone(),
                    wh.send_url.clone(),
                    wh.send_method.clone(),
                    wh.auth_header.clone(),
                    wh.secret.clone(),
                    wh.max_retries,
                    wh.retry_base_delay_ms,
                    wh.retry_max_delay_ms,
                )),
                wh,
            ),
        });
    }

    #[cfg(not(feature = "channel-webhook"))]
    if !config.channels.webhook.is_empty() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "Webhook channel is configured but this build was compiled without \
             `channel-webhook`; skipping Webhook."
        );
    }

    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
            .with_attrs(::serde_json::json!({
                "activated_bindings": active_channel_aliases.enabled_bindings.len(),
                "bindings": active_channel_aliases.enabled_bindings.iter().collect::<Vec<_>>(),
            })),
        "channel binding(s) activated from enabled agents"
    );

    channels
}

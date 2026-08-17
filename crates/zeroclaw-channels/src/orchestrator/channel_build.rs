//! Per-id channel construction. Extracted from orchestrator/mod.rs (god-file remainder C7).

#[cfg(any(
    test,
    feature = "channel-discord",
    feature = "channel-lark",
    feature = "channel-matrix",
    feature = "channel-slack",
    feature = "channel-telegram",
    feature = "channel-wechat",
    feature = "whatsapp-web",
))]
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(any(
    feature = "channel-dingtalk",
    feature = "channel-discord",
    feature = "channel-email",
    feature = "channel-git",
    feature = "channel-irc",
    feature = "channel-lark",
    feature = "channel-line",
    feature = "channel-linq",
    feature = "channel-matrix",
    feature = "channel-mattermost",
    feature = "channel-mochat",
    feature = "channel-nextcloud",
    feature = "channel-qq",
    feature = "channel-signal",
    feature = "channel-slack",
    feature = "channel-telegram",
    feature = "channel-twitch",
    feature = "channel-twitter",
    feature = "channel-voice-call",
    feature = "channel-wati",
    feature = "channel-wechat",
    feature = "channel-wecom",
    feature = "whatsapp-web",
))]
use anyhow::Context;
use anyhow::Result;
use parking_lot::RwLock;
use zeroclaw_api::channel::Channel;
use zeroclaw_config::schema::Config;

#[cfg(any(
    test,
    feature = "channel-dingtalk",
    feature = "channel-discord",
    feature = "channel-email",
    feature = "channel-git",
    feature = "channel-imessage",
    feature = "channel-irc",
    feature = "channel-lark",
    feature = "channel-line",
    feature = "channel-linq",
    feature = "channel-matrix",
    feature = "channel-mattermost",
    feature = "channel-mochat",
    feature = "channel-nextcloud",
    feature = "channel-qq",
    feature = "channel-signal",
    feature = "channel-slack",
    feature = "channel-telegram",
    feature = "channel-twitch",
    feature = "channel-twitter",
    feature = "channel-voice-call",
    feature = "channel-wati",
    feature = "channel-wechat",
    feature = "channel-wecom",
    feature = "channel-wecom-ws",
    feature = "whatsapp-web",
))]
use super::*;

#[cfg(any(
    test,
    feature = "channel-discord",
    feature = "channel-lark",
    feature = "channel-matrix",
    feature = "channel-slack",
    feature = "channel-telegram",
    feature = "channel-wechat",
    feature = "whatsapp-web",
))]
pub(crate) fn one_shot_channel_workspace_dir(
    config: &Config,
    channel_type: &str,
    alias: &str,
) -> PathBuf {
    config.channel_workspace_dir(&format!("{channel_type}.{alias}"))
}

/// Build a single channel instance by config section name (e.g. "telegram").
pub(super) fn build_channel_by_id(
    config_arc: &Arc<RwLock<Config>>,
    channel_id: &str,
) -> Result<Arc<dyn Channel>> {
    #[allow(unused_variables)]
    let config = config_arc.read();
    match channel_id {
        #[cfg(feature = "channel-telegram")]
        "telegram" => {
            let tg = config
                .channels
                .telegram
                .get("default")
                .context("Telegram channel is not configured")?;
            let ack = tg.ack_reactions.unwrap_or(config.channels.ack_reactions);
            let alias = "default".to_string();
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.clone();
                Arc::new(move || cfg_arc.read().channel_external_peers("telegram", &alias))
            };
            let workspace_dir = one_shot_channel_workspace_dir(&config, "telegram", &alias);
            let voice_peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.clone();
                Arc::new(move || cfg_arc.read().channel_voice_peers("telegram", &alias))
            };
            Ok(Arc::new(
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
                .with_tts(&config)
                .with_workspace_dir(workspace_dir)
                .with_approval_timeout_secs(tg.approval_timeout_secs),
            ))
        }
        #[cfg(not(feature = "channel-telegram"))]
        "telegram" => {
            anyhow::bail!("Telegram channel requires the `channel-telegram` feature");
        }
        #[cfg(feature = "channel-discord")]
        "discord" => {
            let dc = config
                .channels
                .discord
                .get("default")
                .context("Discord channel is not configured")?;
            let alias = "default".to_string();
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.clone();
                Arc::new(move || cfg_arc.read().channel_external_peers("discord", &alias))
            };
            let workspace_dir = one_shot_channel_workspace_dir(&config, "discord", &alias);
            Ok(Arc::new(
                DiscordChannel::new(
                    dc.bot_token.clone(),
                    dc.guild_ids.clone(),
                    alias,
                    peer_resolver,
                    dc.listen_to_bots,
                    dc.mention_only,
                )
                .with_channel_ids(dc.channel_ids.clone())
                .with_workspace_dir(workspace_dir)
                .with_streaming(
                    dc.stream_mode,
                    dc.draft_update_interval_ms,
                    dc.multi_message_delay_ms,
                )
                .with_transcription(config.transcription.clone())
                .with_stall_timeout(dc.stall_timeout_secs)
                .with_approval_timeout_secs(dc.approval_timeout_secs)
                .with_intents_mask(dc.intents_mask)
                .with_reaction_notifications(dc.reaction_notifications),
            ))
        }
        #[cfg(not(feature = "channel-discord"))]
        "discord" => {
            anyhow::bail!("Discord channel requires the `channel-discord` feature");
        }
        #[cfg(feature = "channel-slack")]
        "slack" => {
            let sl = config
                .channels
                .slack
                .get("default")
                .context("Slack channel is not configured")?;
            let alias = "default".to_string();
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.clone();
                Arc::new(move || cfg_arc.read().channel_external_peers("slack", &alias))
            };
            let workspace_dir = one_shot_channel_workspace_dir(&config, "slack", &alias);
            let bot_token = sl.resolved_bot_token().with_context(|| {
                format!(
                    "Slack channel '{alias}': bot_token is not set. Provide it in config \
                     (channels.slack.{alias}.bot_token) or via the \
                     ZEROCLAW_SLACK_BOT_TOKEN / SLACK_BOT_TOKEN environment variable."
                )
            })?;
            Ok(Arc::new(
                SlackChannel::new(
                    bot_token,
                    sl.resolved_app_token(),
                    sl.channel_ids.clone(),
                    alias,
                    peer_resolver,
                )
                .with_workspace_dir(workspace_dir)
                .with_markdown_blocks(sl.use_markdown_blocks)
                .with_transcription(config.transcription.clone())
                .with_streaming(sl.stream_drafts, sl.draft_update_interval_ms)
                .with_cancel_reaction(sl.cancel_reaction.clone())
                .with_approval_timeout_secs(sl.approval_timeout_secs),
            ))
        }
        #[cfg(not(feature = "channel-slack"))]
        "slack" => {
            anyhow::bail!("Slack channel requires the `channel-slack` feature");
        }
        #[cfg(feature = "channel-mattermost")]
        "mattermost" => {
            let mm = config
                .channels
                .mattermost
                .get("default")
                .context("Mattermost channel is not configured")?;
            let alias = "default".to_string();
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.clone();
                Arc::new(move || cfg_arc.read().channel_external_peers("mattermost", &alias))
            };
            Ok(Arc::new(
                MattermostChannel::new(
                    mm.url.clone(),
                    mm.bot_token.clone(),
                    mm.login_id.clone(),
                    mm.password.clone(),
                    mm.channel_ids.clone(),
                    alias,
                    peer_resolver,
                    mm.thread_replies.unwrap_or(true),
                    mm.mention_only.unwrap_or(false),
                )
                .with_team_ids(mm.team_ids.clone())
                .with_discover_dms(mm.discover_dms.unwrap_or(true))
                .with_listen_mode(mm.listen_mode),
            ))
        }
        #[cfg(not(feature = "channel-mattermost"))]
        "mattermost" => {
            anyhow::bail!("Mattermost channel requires the `channel-mattermost` feature");
        }
        #[cfg(feature = "channel-signal")]
        "signal" => {
            let sg = config
                .channels
                .signal
                .get("default")
                .context("Signal channel is not configured")?;
            let alias = "default".to_string();
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.clone();
                Arc::new(move || cfg_arc.read().channel_external_peers("signal", &alias))
            };
            Ok(Arc::new(
                SignalChannel::new(
                    sg.http_url.clone(),
                    sg.account.clone(),
                    sg.group_ids.clone(),
                    sg.dm_only,
                    alias,
                    peer_resolver,
                    sg.ignore_attachments,
                    sg.ignore_stories,
                )
                .with_approval_timeout_secs(sg.approval_timeout_secs),
            ))
        }
        #[cfg(not(feature = "channel-signal"))]
        "signal" => {
            anyhow::bail!("Signal channel requires the `channel-signal` feature");
        }
        "matrix" => {
            #[cfg(feature = "channel-matrix")]
            {
                let mx = config
                    .channels
                    .matrix
                    .get("default")
                    .context("Matrix channel is not configured")?;
                let alias = "default".to_string();
                let state_dir = matrix_state_dir(&config.config_path, &alias);
                let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                    let cfg_arc = config_arc.clone();
                    let alias = alias.clone();
                    Arc::new(move || cfg_arc.read().channel_external_peers("matrix", &alias))
                };
                let ack = mx.ack_reactions.unwrap_or(config.channels.ack_reactions);
                let workspace_dir = one_shot_channel_workspace_dir(&config, "matrix", &alias);
                Ok(Arc::new(
                    MatrixChannel::new(mx.clone(), alias, peer_resolver, state_dir)?
                        .with_transcription(config.transcription.clone())
                        .with_workspace_dir(workspace_dir)
                        .with_ack_reactions(ack),
                ))
            }
            #[cfg(not(feature = "channel-matrix"))]
            {
                anyhow::bail!("Matrix channel requires the `channel-matrix` feature");
            }
        }
        "whatsapp" | "whatsapp-web" | "whatsapp_web" => {
            #[cfg(feature = "whatsapp-web")]
            {
                let wa = config
                    .channels
                    .whatsapp
                    .get("default")
                    .context("WhatsApp channel is not configured")?;
                if !wa.is_web_config() {
                    anyhow::bail!(
                        "WhatsApp channel send requires Web mode (set session_path, pair_phone, or mode = personal)"
                    );
                }
                let alias = "default".to_string();
                let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                    let cfg_arc = config_arc.clone();
                    let alias = alias.clone();
                    Arc::new(move || cfg_arc.read().channel_external_peers("whatsapp", &alias))
                };
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
                let workspace_dir = one_shot_channel_workspace_dir(&config, "whatsapp", &alias);
                Ok(Arc::new(
                    WhatsAppWebChannel::new(wa, alias, peer_resolver, allowed_groups_resolver)
                        .with_persistence(config_arc.clone())
                        .with_workspace_dir(workspace_dir),
                ))
            }
            #[cfg(not(feature = "whatsapp-web"))]
            {
                anyhow::bail!("WhatsApp channel requires the `whatsapp-web` feature");
            }
        }
        #[cfg(feature = "channel-qq")]
        "qq" => {
            let qq = config
                .channels
                .qq
                .get("default")
                .context("QQ channel is not configured")?;
            let alias = "default".to_string();
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.clone();
                Arc::new(move || cfg_arc.read().channel_external_peers("qq", &alias))
            };
            Ok(Arc::new(QQChannel::new(
                qq.app_id.clone(),
                qq.app_secret.clone(),
                alias,
                peer_resolver,
            )))
        }
        #[cfg(not(feature = "channel-qq"))]
        "qq" => {
            anyhow::bail!("QQ channel requires the `channel-qq` feature");
        }
        "lark" => {
            #[cfg(feature = "channel-lark")]
            {
                let lk = config
                    .channels
                    .lark
                    .get("default")
                    .context("Lark channel is not configured")?;
                let alias = "default".to_string();
                let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                    let cfg_arc = config_arc.clone();
                    let alias = alias.clone();
                    Arc::new(move || cfg_arc.read().channel_external_peers("lark", &alias))
                };
                Ok(Arc::new(
                    LarkChannel::from_config(lk, alias, peer_resolver)
                        .with_workspace_dir(one_shot_channel_workspace_dir(
                            &config, "lark", "default",
                        ))
                        .with_approval_timeout_secs(lk.approval_timeout_secs)
                        .with_per_user_session(lk.per_user_session)
                        .with_ack_reactions(
                            lk.ack_reactions.unwrap_or(config.channels.ack_reactions),
                        )
                        .with_streaming(lk.stream_mode, lk.draft_update_interval_ms),
                ))
            }
            #[cfg(not(feature = "channel-lark"))]
            {
                anyhow::bail!("Lark channel requires the `channel-lark` feature");
            }
        }
        #[cfg(feature = "channel-dingtalk")]
        "dingtalk" => {
            let dt = config
                .channels
                .dingtalk
                .get("default")
                .context("DingTalk channel is not configured")?;
            let alias = "default".to_string();
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.clone();
                Arc::new(move || cfg_arc.read().channel_external_peers("dingtalk", &alias))
            };
            Ok(Arc::new(
                DingTalkChannel::new(
                    dt.client_id.clone(),
                    dt.client_secret.clone(),
                    alias,
                    peer_resolver,
                )
                .with_proxy_url(dt.proxy_url.clone()),
            ))
        }
        #[cfg(not(feature = "channel-dingtalk"))]
        "dingtalk" => {
            anyhow::bail!("DingTalk channel requires the `channel-dingtalk` feature");
        }
        #[cfg(feature = "channel-wecom")]
        "wecom" => {
            let wc = config
                .channels
                .wecom
                .get("default")
                .context("WeCom channel is not configured")?;
            let alias = "default".to_string();
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.clone();
                Arc::new(move || cfg_arc.read().channel_external_peers("wecom", &alias))
            };
            Ok(Arc::new(WeComChannel::new(
                wc.webhook_key.clone(),
                alias,
                peer_resolver,
            )))
        }
        #[cfg(not(feature = "channel-wecom"))]
        "wecom" => {
            anyhow::bail!("WeCom channel requires the `channel-wecom` feature");
        }
        #[cfg(feature = "channel-wecom-ws")]
        channel_id
            if channel_id == "wecom_ws"
                || channel_id == "wecom-ws"
                || channel_id.starts_with("wecom_ws.")
                || channel_id.starts_with("wecom-ws.") =>
        {
            let alias = channel_id
                .split_once('.')
                .map(|(_, alias)| alias)
                .unwrap_or("default")
                .to_string();
            let wc =
                config.channels.wecom_ws.get(&alias).with_context(|| {
                    format!("WeCom WebSocket channel '{alias}' is not configured")
                })?;
            let policy_resolver: Arc<dyn Fn() -> WeComWsRuntimePolicy + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.clone();
                let snapshot = wc.clone();
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
            Ok(Arc::new(WeComWsChannel::new_with_alias(
                wc,
                alias.clone(),
                policy_resolver,
                &config.channel_workspace_dir(&format!("wecom_ws.{alias}")),
            )?))
        }
        #[cfg(not(feature = "channel-wecom-ws"))]
        channel_id
            if channel_id == "wecom_ws"
                || channel_id == "wecom-ws"
                || channel_id.starts_with("wecom_ws.")
                || channel_id.starts_with("wecom-ws.") =>
        {
            anyhow::bail!("WeCom WebSocket channel requires the `channel-wecom-ws` feature");
        }
        #[cfg(feature = "channel-wechat")]
        "wechat" => {
            let wc = config
                .channels
                .wechat
                .get("default")
                .context("WeChat channel is not configured")?;
            let alias = "default".to_string();
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.clone();
                Arc::new(move || cfg_arc.read().channel_external_peers("wechat", &alias))
            };
            let workspace_dir = one_shot_channel_workspace_dir(&config, "wechat", &alias);
            Ok(Arc::new(
                WeChatChannel::new(
                    alias,
                    peer_resolver,
                    wc.api_base_url.clone(),
                    wc.cdn_base_url.clone(),
                    Some(WeChatChannel::resolve_state_dir(wc.state_dir.as_deref())),
                )?
                .with_persistence(config_arc.clone())
                .with_workspace_dir(workspace_dir),
            ))
        }
        #[cfg(not(feature = "channel-wechat"))]
        "wechat" => {
            anyhow::bail!("WeChat channel requires the `channel-wechat` feature");
        }
        #[cfg(feature = "channel-nextcloud")]
        "nextcloud_talk" | "nextcloud-talk" => {
            let nc = config
                .channels
                .nextcloud_talk
                .get("default")
                .context("Nextcloud Talk channel is not configured")?;
            let alias = "default".to_string();
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.clone();
                Arc::new(move || {
                    cfg_arc
                        .read()
                        .channel_external_peers("nextcloud_talk", &alias)
                })
            };
            Ok(Arc::new(
                NextcloudTalkChannel::new_with_proxy(
                    nc.base_url.clone(),
                    nc.app_token.clone(),
                    nc.bot_name.clone().unwrap_or_default(),
                    alias,
                    peer_resolver,
                    nc.proxy_url.clone(),
                )
                .with_streaming(nc.stream_mode, nc.draft_update_interval_ms),
            ))
        }
        #[cfg(not(feature = "channel-nextcloud"))]
        "nextcloud_talk" | "nextcloud-talk" => {
            anyhow::bail!("Nextcloud Talk channel requires the `channel-nextcloud` feature");
        }
        #[cfg(feature = "channel-wati")]
        "wati" => {
            let wati_cfg = config
                .channels
                .wati
                .get("default")
                .context("WATI channel is not configured")?;
            let alias = "default".to_string();
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.clone();
                Arc::new(move || cfg_arc.read().channel_external_peers("wati", &alias))
            };
            Ok(Arc::new(WatiChannel::new_with_proxy(
                wati_cfg.api_token.clone(),
                wati_cfg.api_url.clone(),
                wati_cfg.tenant_id.clone(),
                alias,
                peer_resolver,
                wati_cfg.proxy_url.clone(),
            )))
        }
        #[cfg(not(feature = "channel-wati"))]
        "wati" => {
            anyhow::bail!("WATI channel requires the `channel-wati` feature");
        }
        #[cfg(feature = "channel-linq")]
        "linq" => {
            let lq = config
                .channels
                .linq
                .get("default")
                .context("Linq channel is not configured")?;
            let alias = "default".to_string();
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.clone();
                Arc::new(move || cfg_arc.read().channel_external_peers("linq", &alias))
            };
            Ok(Arc::new(LinqChannel::new(
                lq.api_token.clone(),
                lq.from_phone.clone(),
                alias,
                peer_resolver,
            )))
        }
        #[cfg(feature = "channel-linq")]
        x if x.starts_with("linq.") => {
            let alias = x.strip_prefix("linq.").context("invalid linq channel id")?;
            let lq = config
                .channels
                .linq
                .get(alias)
                .with_context(|| format!("Linq alias '{alias}' not configured"))?;
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.to_string();
                Arc::new(move || cfg_arc.read().channel_external_peers("linq", &alias))
            };
            Ok(Arc::new(LinqChannel::new(
                lq.api_token.clone(),
                lq.from_phone.clone(),
                alias.to_string(),
                peer_resolver,
            )))
        }
        #[cfg(not(feature = "channel-linq"))]
        x if x.starts_with("linq") => {
            anyhow::bail!("Linq channel requires the `channel-linq` feature");
        }
        #[cfg(feature = "channel-email")]
        "email" => {
            let em = config
                .channels
                .email
                .get("default")
                .context("Email channel is not configured")?;
            let alias = "default".to_string();
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.clone();
                Arc::new(move || cfg_arc.read().channel_external_peers("email", &alias))
            };
            Ok(Arc::new(EmailChannel::new(
                em.clone(),
                alias,
                peer_resolver,
            )))
        }
        #[cfg(not(feature = "channel-email"))]
        "email" => {
            anyhow::bail!("Email channel requires the `channel-email` feature");
        }
        #[cfg(feature = "channel-email")]
        "gmail_push" | "gmail-push" => {
            let gp = config
                .channels
                .gmail_push
                .get("default")
                .context("Gmail Push channel is not configured")?;
            let alias = "default".to_string();
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.clone();
                Arc::new(move || cfg_arc.read().channel_external_peers("gmail_push", &alias))
            };
            Ok(Arc::new(GmailPushChannel::new(
                gp.clone(),
                alias,
                peer_resolver,
            )))
        }
        #[cfg(not(feature = "channel-email"))]
        "gmail_push" | "gmail-push" => {
            anyhow::bail!("Gmail Push channel requires the `channel-email` feature");
        }
        #[cfg(feature = "channel-irc")]
        "irc" => {
            let irc_cfg = config
                .channels
                .irc
                .get("default")
                .context("IRC channel is not configured")?;
            let alias = "default".to_string();
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.clone();
                Arc::new(move || cfg_arc.read().channel_external_peers("irc", &alias))
            };
            Ok(Arc::new(IrcChannel::new(crate::irc::IrcChannelConfig {
                server: irc_cfg.server.clone(),
                port: irc_cfg.port,
                nickname: irc_cfg.nickname.clone(),
                username: irc_cfg.username.clone(),
                channels: irc_cfg.channels.clone(),
                alias,
                peer_resolver,
                server_password: irc_cfg.server_password.clone(),
                nickserv_password: irc_cfg.nickserv_password.clone(),
                sasl_password: irc_cfg.sasl_password.clone(),
                verify_tls: irc_cfg.verify_tls.unwrap_or(true),
                mention_only: irc_cfg.mention_only,
            })))
        }
        #[cfg(not(feature = "channel-irc"))]
        "irc" => {
            anyhow::bail!("IRC channel requires the `channel-irc` feature");
        }
        #[cfg(feature = "channel-twitch")]
        "twitch" => {
            let tw_cfg = config
                .channels
                .twitch
                .get("default")
                .context("Twitch channel is not configured")?;
            let alias = "default".to_string();
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.clone();
                Arc::new(move || cfg_arc.read().channel_external_peers("twitch", &alias))
            };
            Ok(Arc::new(TwitchChannel::new(
                tw_cfg.bot_username.clone(),
                tw_cfg.oauth_token.clone(),
                tw_cfg.channels.clone(),
                tw_cfg.mention_only,
                alias,
                peer_resolver,
            )))
        }
        #[cfg(not(feature = "channel-twitch"))]
        "twitch" => {
            anyhow::bail!("Twitch channel requires the `channel-twitch` feature");
        }
        #[cfg(feature = "channel-twitter")]
        "twitter" => {
            let tw = config
                .channels
                .twitter
                .get("default")
                .context("X/Twitter channel is not configured")?;
            let alias = "default".to_string();
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.clone();
                Arc::new(move || cfg_arc.read().channel_external_peers("twitter", &alias))
            };
            Ok(Arc::new(TwitterChannel::new(
                tw.bearer_token.clone(),
                alias,
                peer_resolver,
            )))
        }
        #[cfg(not(feature = "channel-twitter"))]
        "twitter" => {
            anyhow::bail!("X/Twitter channel requires the `channel-twitter` feature");
        }
        #[cfg(feature = "channel-git")]
        "git" => {
            let g = config
                .channels
                .git
                .get("default")
                .context("Git channel is not configured")?;
            let alias = "default".to_string();
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.clone();
                Arc::new(move || cfg_arc.read().channel_external_peers("git", &alias))
            };
            Ok(Arc::new(GitChannel::new(g.clone(), alias, peer_resolver)?))
        }
        #[cfg(not(feature = "channel-git"))]
        "git" => {
            anyhow::bail!("Git channel requires the `channel-git` feature");
        }
        #[cfg(feature = "channel-mochat")]
        "mochat" => {
            let mc = config
                .channels
                .mochat
                .get("default")
                .context("Mochat channel is not configured")?;
            let alias = "default".to_string();
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.clone();
                Arc::new(move || cfg_arc.read().channel_external_peers("mochat", &alias))
            };
            Ok(Arc::new(MochatChannel::new(
                mc.api_url.clone(),
                mc.api_token.clone(),
                alias,
                peer_resolver,
                mc.poll_interval_secs,
            )))
        }
        #[cfg(not(feature = "channel-mochat"))]
        "mochat" => {
            anyhow::bail!("Mochat channel requires the `channel-mochat` feature");
        }
        #[cfg(feature = "channel-imessage")]
        "imessage" => {
            if !config.channels.imessage.contains_key("default") {
                anyhow::bail!("iMessage channel is not configured");
            }
            let alias = "default".to_string();
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = {
                let cfg_arc = config_arc.clone();
                let alias = alias.clone();
                Arc::new(move || cfg_arc.read().channel_external_peers("imessage", &alias))
            };
            Ok(Arc::new(IMessageChannel::new(alias, peer_resolver)))
        }
        #[cfg(not(feature = "channel-imessage"))]
        "imessage" => {
            anyhow::bail!("iMessage channel requires the `channel-imessage` feature");
        }
        "line" => {
            #[cfg(feature = "channel-line")]
            {
                let ln = config
                    .channels
                    .line
                    .get("default")
                    .context("LINE channel is not configured")?;
                let alias = "default".to_string();
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
                Ok(Arc::new(
                    LineChannel::from_config(ln, alias, peer_resolver, sender_name_resolver)
                        .with_persistence(config_arc.clone()),
                ))
            }
            #[cfg(not(feature = "channel-line"))]
            {
                anyhow::bail!("LINE channel requires the `channel-line` feature");
            }
        }
        "voice-call" => {
            #[cfg(feature = "channel-voice-call")]
            {
                let (alias, vc) = config
                    .channels
                    .voice_call
                    .iter()
                    .next()
                    .context("Voice Call channel is not configured")?;
                Ok(Arc::new(VoiceCallChannel::new(alias.clone(), vc.clone())))
            }
            #[cfg(not(feature = "channel-voice-call"))]
            {
                anyhow::bail!("Voice Call channel requires the `channel-voice-call` feature");
            }
        }
        other => anyhow::bail!(
            "Unknown channel '{other}'. Supported: telegram, discord, slack, mattermost, signal, \
            matrix, whatsapp, qq, lark, feishu, dingtalk, wecom, wecom_ws, nextcloud_talk, wati, linq, \
            email, gmail_push, git, irc, twitter, mochat, imessage, line, voice-call"
        ),
    }
}

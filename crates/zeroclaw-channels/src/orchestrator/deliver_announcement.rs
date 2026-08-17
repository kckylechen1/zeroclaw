//! Cron/announcement delivery to a named channel. Extracted from orchestrator/mod.rs.

#[cfg(feature = "channel-slack")]
use anyhow::Context;
#[cfg(any(
    feature = "channel-discord",
    feature = "channel-slack",
    feature = "channel-telegram",
))]
use std::sync::Arc;

#[cfg(feature = "channel-discord")]
use super::DiscordChannel;
#[cfg(feature = "channel-email")]
use super::EmailChannel;
#[cfg(feature = "channel-lark")]
use super::LarkChannel;
#[cfg(feature = "channel-signal")]
use super::SignalChannel;
#[cfg(feature = "channel-slack")]
use super::SlackChannel;
#[cfg(feature = "channel-telegram")]
use super::TelegramChannel;
#[cfg(feature = "channel-wechat")]
use super::WeChatChannel;
#[cfg(feature = "channel-webhook")]
use super::WebhookChannel;
#[cfg(feature = "whatsapp-web")]
use super::WhatsAppWebChannel;
use super::{
    CRON_CHANNEL_REGISTRY, ensure_nonempty_channel_reply, outbound_content_format_for_channel,
    redact_channel_outbound_leaks,
};

/// Start all configured channels and route messages to the agent
#[allow(clippy::too_many_lines)]
pub async fn deliver_announcement(
    config: &zeroclaw_config::schema::Config,
    channel: &str,
    target: &str,
    thread_id: Option<String>,
    output: &str,
) -> anyhow::Result<()> {
    use zeroclaw_api::channel::SendMessage;

    let safe_output = redact_channel_outbound_leaks(
        output,
        &config.security.leak_detection,
        outbound_content_format_for_channel(channel),
    );
    let safe_output = ensure_nonempty_channel_reply(safe_output, output, channel, target);

    let make_msg = |s: &str| SendMessage::new(s, target).in_thread(thread_id.clone());

    // Snapshot out of the sync RwLock before awaiting. Use the live
    // channel instance when available — critical for Matrix E2EE which
    // must reuse the authenticated client rather than re-running session
    // restore per delivery.
    let registry_snapshot = CRON_CHANNEL_REGISTRY
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(registry) = registry_snapshot
        && let Some(ch) = registry.get(channel.to_ascii_lowercase().as_str())
    {
        return ch.send(&make_msg(&safe_output)).await;
    }

    let (raw_type, alias) = channel.split_once('.').ok_or_else(|| {
        anyhow::Error::msg(format!(
            "delivery channel {channel:?} must be a dotted <type>.<alias> ref (e.g. telegram.work)"
        ))
    })?;
    let channel_type = raw_type.to_ascii_lowercase();
    #[allow(unused_variables)]
    let not_configured = || {
        ::zeroclaw_log::record!(
            ERROR,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure),
            &format!("[channels.{channel_type}.{alias}] not configured")
        );
        anyhow::Error::msg(format!("[channels.{channel_type}.{alias}] not configured"))
    };
    match channel_type.as_str() {
        #[cfg(feature = "channel-telegram")]
        "telegram" => {
            let tg = config
                .channels
                .telegram
                .get(alias)
                .ok_or_else(not_configured)?;
            let peers = config.channel_external_peers("telegram", alias);
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> =
                Arc::new(move || peers.clone());
            let ch =
                TelegramChannel::new(tg.bot_token.clone(), alias, peer_resolver, tg.mention_only)
                    .with_api_base(tg.api_base_url.clone());
            zeroclaw_api::channel::Channel::send(&ch, &make_msg(&safe_output)).await?;
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
                .get(alias)
                .ok_or_else(not_configured)?;
            let peers = config.channel_external_peers("discord", alias);
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> =
                Arc::new(move || peers.clone());
            let ch = DiscordChannel::new(
                dc.bot_token.clone(),
                dc.guild_ids.clone(),
                alias,
                peer_resolver,
                dc.listen_to_bots,
                dc.mention_only,
            )
            .with_channel_ids(dc.channel_ids.clone())
            .with_workspace_dir(config.channel_workspace_dir(channel));
            zeroclaw_api::channel::Channel::send(&ch, &make_msg(&safe_output)).await?;
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
                .get(alias)
                .ok_or_else(not_configured)?;
            let peers = config.channel_external_peers("slack", alias);
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> =
                Arc::new(move || peers.clone());
            let bot_token = sl.resolved_bot_token().with_context(|| {
                format!(
                    "Slack channel '{alias}': bot_token is not set. Provide it in config \
                     (channels.slack.{alias}.bot_token) or via the \
                     ZEROCLAW_SLACK_BOT_TOKEN / SLACK_BOT_TOKEN environment variable."
                )
            })?;
            let ch = SlackChannel::new(
                bot_token,
                sl.resolved_app_token(),
                sl.channel_ids.clone(),
                alias,
                peer_resolver,
            )
            .with_workspace_dir(config.channel_workspace_dir(channel));
            zeroclaw_api::channel::Channel::send(&ch, &make_msg(&safe_output)).await?;
        }
        #[cfg(not(feature = "channel-slack"))]
        "slack" => {
            anyhow::bail!("Slack channel requires the `channel-slack` feature");
        }
        #[cfg(feature = "channel-signal")]
        "signal" => {
            let sg = config
                .channels
                .signal
                .get(alias)
                .ok_or_else(not_configured)?;
            let peers = config.channel_external_peers("signal", alias);
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> =
                Arc::new(move || peers.clone());
            let ch = SignalChannel::new(
                sg.http_url.clone(),
                sg.account.clone(),
                sg.group_ids.clone(),
                sg.dm_only,
                alias,
                peer_resolver,
                sg.ignore_attachments,
                sg.ignore_stories,
            );
            zeroclaw_api::channel::Channel::send(&ch, &make_msg(&safe_output)).await?;
        }
        #[cfg(not(feature = "channel-signal"))]
        "signal" => {
            anyhow::bail!("Signal channel requires the `channel-signal` feature");
        }
        #[cfg(feature = "channel-wechat")]
        "wechat" => {
            let wc = config
                .channels
                .wechat
                .get(alias)
                .ok_or_else(not_configured)?;
            let peers = config.channel_external_peers("wechat", alias);
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> =
                Arc::new(move || peers.clone());
            let ch = WeChatChannel::new(
                alias,
                peer_resolver,
                wc.api_base_url.clone(),
                wc.cdn_base_url.clone(),
                Some(WeChatChannel::resolve_state_dir(wc.state_dir.as_deref())),
            )?
            .with_workspace_dir(config.channel_workspace_dir(channel));
            zeroclaw_api::channel::Channel::send(&ch, &make_msg(&safe_output)).await?;
        }
        #[cfg(not(feature = "channel-wechat"))]
        "wechat" => {
            anyhow::bail!("WeChat channel requires the `channel-wechat` feature");
        }
        #[cfg(feature = "channel-lark")]
        "lark" | "feishu" => {
            // [channels.lark.<alias>] is the single source of truth for both
            // names (AGENTS.md). from_config selects the endpoint via
            // use_feishu. Error text names the real config table, not the
            // cron alias the user wrote.
            let lk = config.channels.lark.get(alias).ok_or_else(|| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                    &format!(
                        "[channels.lark.{alias}] not configured (cron channel \"{channel_type}.{alias}\")"
                    )
                );
                anyhow::Error::msg(format!(
                    "[channels.lark.{alias}] not configured (cron channel \"{channel_type}.{alias}\")"
                ))
            })?;
            // Asymmetric by design: "feishu"+use_feishu=false is a typo
            // (hard fail). "lark"+use_feishu=true is a soft compat path
            // (warn but still deliver via fallback construction).
            if channel_type == "feishu" && !lk.use_feishu {
                anyhow::bail!(
                    "[channels.lark.{alias}] has use_feishu=false but cron channel=\"feishu.{alias}\"; \
                     use channel=\"lark.{alias}\" or set use_feishu=true"
                );
            }
            if channel_type == "lark" && lk.use_feishu {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                    &format!(
                        "cron channel=\"lark.{alias}\" with [channels.lark.{alias}] use_feishu=true \
                         falls back to one-shot channel construction; prefer channel=\"feishu.{alias}\" \
                         to reuse the live Feishu handle from start_channels"
                    )
                );
            }
            let peers = config.channel_external_peers("lark", alias);
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> =
                Arc::new(move || peers.clone());
            let ch = LarkChannel::from_config(lk, alias, peer_resolver)
                .with_workspace_dir(config.channel_workspace_dir(&format!("lark.{alias}")))
                .with_approval_timeout_secs(lk.approval_timeout_secs)
                .with_per_user_session(lk.per_user_session)
                .with_ack_reactions(lk.ack_reactions.unwrap_or(config.channels.ack_reactions))
                .with_streaming(lk.stream_mode, lk.draft_update_interval_ms);
            zeroclaw_api::channel::Channel::send(&ch, &make_msg(&safe_output)).await?;
        }
        #[cfg(not(feature = "channel-lark"))]
        "lark" | "feishu" => {
            anyhow::bail!("Lark channel requires the `channel-lark` feature");
        }
        #[cfg(feature = "channel-webhook")]
        "webhook" => {
            let wh = config
                .channels
                .webhook
                .get(alias)
                .ok_or_else(not_configured)?;
            let ch = WebhookChannel::new(
                alias.to_string(),
                wh.port,
                wh.listen_path.clone(),
                wh.send_url.clone(),
                wh.send_method.clone(),
                wh.auth_header.clone(),
                wh.secret.clone(),
                wh.max_retries,
                wh.retry_base_delay_ms,
                wh.retry_max_delay_ms,
            );
            zeroclaw_api::channel::Channel::send(&ch, &make_msg(&safe_output)).await?;
        }
        #[cfg(not(feature = "channel-webhook"))]
        "webhook" => {
            anyhow::bail!("Webhook channel requires the `channel-webhook` feature");
        }
        "wecom_ws" | "wecom-ws" => {
            let _ = config
                .channels
                .wecom_ws
                .get(alias)
                .ok_or_else(not_configured)?;
            anyhow::bail!("wecom_ws channel is not connected");
        }
        #[cfg(feature = "channel-email")]
        "email" => {
            let em = config
                .channels
                .email
                .get(alias)
                .ok_or_else(not_configured)?;
            let peers = config.channel_external_peers("email", alias);
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> =
                Arc::new(move || peers.clone());
            let ch = EmailChannel::new(em.clone(), alias.to_string(), peer_resolver);
            zeroclaw_api::channel::Channel::send(&ch, &make_msg(&safe_output)).await?;
        }
        #[cfg(not(feature = "channel-email"))]
        "email" => {
            anyhow::bail!("Email channel requires the `channel-email` feature");
        }
        #[cfg(feature = "whatsapp-web")]
        "whatsapp" | "whatsapp-web" | "whatsapp_web" => {
            let wa = config
                .channels
                .whatsapp
                .get(alias)
                .ok_or_else(not_configured)?;
            if !wa.is_web_config() {
                anyhow::bail!(
                    "WhatsApp channel send requires Web mode (set session_path, pair_phone, or mode = personal)"
                );
            }
            let peers = config.channel_external_peers("whatsapp", alias);
            let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> =
                Arc::new(move || peers.clone());
            let allowed_groups = wa.allowed_groups.clone();
            let allowed_groups_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> =
                Arc::new(move || allowed_groups.clone());
            let ch = WhatsAppWebChannel::new(
                wa,
                alias.to_string(),
                peer_resolver,
                allowed_groups_resolver,
            )
            .with_workspace_dir(config.channel_workspace_dir(&format!("whatsapp.{alias}")));
            zeroclaw_api::channel::Channel::send(&ch, &make_msg(&safe_output)).await?;
        }
        #[cfg(not(feature = "whatsapp-web"))]
        "whatsapp" | "whatsapp-web" | "whatsapp_web" => {
            anyhow::bail!("WhatsApp channel requires the `whatsapp-web` feature");
        }
        other => anyhow::bail!("unsupported delivery channel: {other}"),
    }
    #[allow(unreachable_code)]
    Ok(())
}

pub mod listing;
pub mod session_backend {
    pub use zeroclaw_infra::session_backend::*;
}
pub mod session_sqlite {
    pub use zeroclaw_infra::session_sqlite::*;
}

pub use zeroclaw_api::channel::{Channel, ChannelMessage, SendMessage};
pub use zeroclaw_config::multi_agent::{
    ChannelIdentityNormalizer, bind_channel_identity_into, channel_alias_configured,
    channel_identity_normalizer,
};

use crate::config::Config;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use zeroclaw_runtime::i18n::{get_required_cli_string, get_required_cli_string_with_args};

const SYSTEMD_STATUS_ARGS: [&str; 3] = ["--user", "is-active", "zeroclaw.service"];
const SYSTEMD_RESTART_ARGS: [&str; 3] = ["--user", "restart", "zeroclaw.service"];
const OPENRC_STATUS_ARGS: [&str; 2] = ["zeroclaw", "status"];
const OPENRC_RESTART_ARGS: [&str; 2] = ["zeroclaw", "restart"];

pub async fn handle_command(command: crate::ChannelCommands, config: &Config) -> Result<()> {
    match command {
        crate::ChannelCommands::Start => {
            anyhow::bail!("Start must be handled in main.rs (requires async runtime)")
        }
        crate::ChannelCommands::Doctor => {
            anyhow::bail!("Doctor must be handled in main.rs (requires async runtime)")
        }
        crate::ChannelCommands::List => {
            println!("{}", get_required_cli_string("cli-channels-header"));
            println!("{}", get_required_cli_string("cli-channels-cli-always"));
            for entry in listing::compiled_channels(&config.channels) {
                println!(
                    "  {} {}",
                    if entry.configured { "✅" } else { "❌" },
                    entry.name
                );
            }
            let uncompiled = listing::configured_uncompiled_channels(&config.channels);
            if !uncompiled.is_empty() {
                println!();
                println!(
                    "{}",
                    get_required_cli_string("cli-channels-not-compiled-header")
                );
                for entry in &uncompiled {
                    println!(
                        "{}",
                        get_required_cli_string_with_args(
                            "cli-channels-not-compiled-entry",
                            &[("name", entry.name)]
                        )
                    );
                }
                println!("{}", get_required_cli_string("cli-channels-build-hint"));
            }
            println!();
            println!("{}", get_required_cli_string("cli-channels-start-hint"));
            println!("{}", get_required_cli_string("cli-channels-doctor-hint"));
            println!("{}", get_required_cli_string("cli-channels-configure-hint"));
            Ok(())
        }
        crate::ChannelCommands::Add {
            channel_type,
            config: _,
        } => {
            anyhow::bail!(
                "Channel type '{channel_type}' — use `zeroclaw config set channels.{channel_type}.<alias>.<field>=<value>` to configure"
            );
        }
        crate::ChannelCommands::Remove { name } => {
            anyhow::bail!("Remove channel '{name}' — edit ~/.zeroclaw/config.toml directly");
        }
        crate::ChannelCommands::BindTelegram { identity, alias } => {
            Box::pin(bind_telegram_identity(config, &identity, &alias)).await
        }
        crate::ChannelCommands::Send {
            message,
            channel_id,
            recipient,
        } => send_channel_message(config, &channel_id, &recipient, &message).await,
    }
}

pub async fn bind_telegram_identity(config: &Config, identity: &str, alias: &str) -> Result<()> {
    let normalizer = channel_identity_normalizer("telegram")
        .context("telegram identity normalizer must be registered")?;
    let normalized = normalizer(identity);
    let mut updated = config.clone();

    if !bind_channel_identity_into(&mut updated, "telegram", alias, identity)? {
        // i18n-exempt: moved from retired zeroclaw-channels orchestrator; compiles without agent-runtime Fluent catalog
        println!("✅ Telegram identity already bound to telegram.{alias}: {normalized}");
        return Ok(());
    }

    Box::pin(updated.save()).await?;
    // i18n-exempt: moved from retired zeroclaw-channels orchestrator; compiles without agent-runtime Fluent catalog
    println!("✅ Bound Telegram identity {normalized} to telegram.{alias}");
    // i18n-exempt: moved from retired zeroclaw-channels orchestrator; compiles without agent-runtime Fluent catalog
    println!("   Saved to {}", updated.config_path.display());
    match maybe_restart_managed_daemon_service() {
        Ok(true) => {
            // i18n-exempt: moved from retired zeroclaw-channels orchestrator; compiles without agent-runtime Fluent catalog
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
            && status_output.status.success()
        {
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

#[cfg(feature = "agent-runtime")]
#[allow(clippy::unused_async)]
pub async fn send_channel_message(
    config: &Config,
    channel_id: &str,
    recipient: &str,
    message: &str,
) -> Result<()> {
    if config.gateway.bridges.contains_key(channel_id) {
        zeroclaw_runtime::cron::scheduler::enqueue_for_bridge(
            config, channel_id, recipient, None, message,
        )
        .with_context(|| format!("Failed to enqueue message for bridge {channel_id}"))?;
        // i18n-exempt: operator bridge queue diagnostic
        println!("Message queued for bridge {channel_id}.");
        return Ok(());
    }
    anyhow::bail!(
        "Channel `{channel_id}` is not configured under `[gateway.bridges.{channel_id}]` \
         (in-process channels have been retired; outbound delivery runs through gateway bridges)"
    )
}

#[cfg(not(feature = "agent-runtime"))]
#[allow(clippy::unused_async)]
pub async fn send_channel_message(
    _config: &Config,
    channel_id: &str,
    _recipient: &str,
    _message: &str,
) -> Result<()> {
    anyhow::bail!(
        "Channel `{channel_id}` requires the `agent-runtime` feature and a configured gateway bridge"
    )
}

#[cfg(feature = "agent-runtime")]
#[allow(clippy::unused_async)]
pub async fn deliver_announcement(
    config: &Config,
    channel: &str,
    target: &str,
    thread_id: Option<String>,
    output: &str,
) -> Result<()> {
    if config.gateway.bridges.contains_key(channel) {
        return zeroclaw_runtime::cron::scheduler::enqueue_for_bridge(
            config,
            channel,
            target,
            thread_id.as_deref(),
            output,
        );
    }
    anyhow::bail!(
        "delivery channel `{channel}` is not configured under `[gateway.bridges.{channel}]` \
         (in-process channels have been retired; outbound delivery runs through gateway bridges)"
    )
}

pub fn build_channel_map(_config: &Config) -> HashMap<String, Arc<dyn Channel>> {
    HashMap::new()
}

pub async fn start_channels(
    _config: Config,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    #[cfg(feature = "agent-runtime")]
    zeroclaw_runtime::health::mark_component_ok("channels");
    println!(
        "In-process channels are retired; run `zeroclaw gateway` (or `zeroclaw daemon`) \
         and connect channel bridge processes over `/ws/chat` and `/ws/bridge`."
    );
    cancel.cancelled().await;
    Ok(())
}

#[allow(clippy::unused_async)]
pub async fn doctor_channels(config: Config) -> Result<()> {
    if config.gateway.bridges.is_empty() {
        // i18n-exempt: moved from retired zeroclaw-channels orchestrator; compiles without agent-runtime Fluent catalog
        println!("No gateway bridges configured. Run `zeroclaw quickstart` to set one up.");
        return Ok(());
    }
    // i18n-exempt: moved from retired zeroclaw-channels orchestrator; compiles without agent-runtime Fluent catalog
    println!("🩺 ZeroClaw Channel Bridge Doctor");
    println!();
    for name in config.gateway.bridges.keys() {
        // i18n-exempt: moved from retired zeroclaw-channels orchestrator; compiles without agent-runtime Fluent catalog
        println!("  ✅ {name:<9} bridge configured (`[gateway.bridges.{name}]`)");
    }
    Ok(())
}

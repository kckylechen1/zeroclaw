use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use zeroclaw_bridge_telegram::{BridgeConfig, run};
use zeroclaw_gateway_client::ConnectOptions;

/// Relay the owner's private Telegram chat to a ZeroClaw gateway session.
#[derive(Debug, Parser)]
#[command(name = "zeroclaw-bridge-telegram", version)]
struct Args {
    /// Bot token from @BotFather
    #[arg(long, env = "TELEGRAM_BOT_TOKEN", hide_env_values = true)]
    telegram_token: String,

    /// Numeric Telegram user id of the owner; everyone else is ignored
    #[arg(long, env = "TELEGRAM_OWNER_ID")]
    owner_id: i64,

    /// Gateway WebSocket base URL
    #[arg(long, default_value = "ws://127.0.0.1:42617")]
    gateway: String,

    /// Paired gateway bearer token
    #[arg(long, env = "ZEROCLAW_GATEWAY_TOKEN", hide_env_values = true)]
    gateway_token: Option<String>,

    /// Agent alias to talk to (must match `[agents.<alias>]` on the gateway)
    #[arg(long)]
    agent: String,

    /// Session the owner's chat maps to; shared with `zeroclaw chat -s <session>`
    #[arg(long, default_value = "main")]
    session: String,

    /// Telegram Bot API base URL
    #[arg(long, default_value = "https://api.telegram.org")]
    telegram_api: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    zeroclaw_log::install_global_subscriber(None, "info", true);
    let config = BridgeConfig {
        telegram_api: args.telegram_api,
        telegram_token: args.telegram_token,
        owner_id: args.owner_id,
        gateway: ConnectOptions {
            gateway: args.gateway,
            agent: args.agent,
            session_id: Some(args.session),
            token: args.gateway_token,
        },
        poll_wait: Duration::from_secs(30),
    };
    tokio::select! {
        result = run(config) => result,
        signal = tokio::signal::ctrl_c() => signal.context("waiting for Ctrl-C"),
    }
}

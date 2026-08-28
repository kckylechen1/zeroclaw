//! Gateway admin HTTP helpers for CLI commands (`gateway shutdown/restart/get-paircode`,
//! `sop approve/deny/pending`). Lives in the binary crate so `reqwest` stays out of
//! library modules that do not need it.

use anyhow::Result;
use std::fmt::Write as _;

use crate::SopCommands;
use crate::config::Config;

/// Resolve a `cli-*` Fluent key for CLI output. Routes through the runtime
/// i18n catalogue under `agent-runtime` (default + CI/release); without that
/// feature the runtime crate is absent, so the English `fallback` is used.
#[allow(unused_variables)]
pub(crate) fn t(key: &str, fallback: &str) -> String {
    #[cfg(feature = "agent-runtime")]
    {
        zeroclaw_runtime::i18n::get_required_cli_string(key)
    }
    #[cfg(not(feature = "agent-runtime"))]
    {
        fallback.to_string() // i18n-exempt: English fallback when Fluent (agent-runtime) is disabled
    }
}

/// `t` with `{$name}` arguments.
#[allow(unused_variables)]
pub(crate) fn ta(key: &str, args: &[(&str, &str)], fallback: &str) -> String {
    #[cfg(feature = "agent-runtime")]
    {
        zeroclaw_runtime::i18n::get_required_cli_string_with_args(key, args)
    }
    #[cfg(not(feature = "agent-runtime"))]
    {
        fallback.to_string() // i18n-exempt: English fallback when Fluent (agent-runtime) is disabled
    }
}

/// Resolve gateway host and port from CLI args or config.
pub fn resolve_gateway_addr(
    config: &Config,
    port: Option<u16>,
    host: Option<String>,
) -> (u16, String) {
    let port = port.unwrap_or(config.gateway.port);
    let host = host.unwrap_or_else(|| config.gateway.host.clone());
    (port, host)
}

/// Log gateway startup message.
pub fn log_gateway_start(host: &str, port: u16) {
    if port == 0 {
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"host": host})),
            "🚀 Starting ZeroClaw Gateway on (random port)"
        );
    } else {
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"host": host, "port": port})),
            "🚀 Starting ZeroClaw Gateway on"
        );
    }
}

/// Gracefully shutdown a running gateway via the admin endpoint.
#[cfg(feature = "agent-runtime")]
pub async fn shutdown_gateway(host: &str, port: u16, path_prefix: Option<&str>) -> Result<()> {
    let url = gateway_admin_url(host, port, path_prefix, "/admin/shutdown");
    let client = reqwest::Client::new();

    match client
        .post(&url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => Ok(()),
        Ok(response) => {
            let status = response.status();
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"endpoint": url, "status": status.as_u16()})),
                "gateway admin shutdown returned non-success status"
            );
            Err(anyhow::Error::msg(format!(
                "Gateway responded with status: {status}"
            )))
        }
        Err(e) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"endpoint": url, "error": format!("{}", e)})),
                "gateway admin shutdown: connect failed"
            );
            Err(anyhow::Error::msg(format!(
                "Failed to connect to gateway: {e}"
            )))
        }
    }
}

#[cfg(feature = "agent-runtime")]
pub enum PaircodeAction {
    /// GET the current code; do not mint or revoke anything.
    Show,
    /// Issue a fresh code for an additional client; revoke nothing.
    AddClient,
    /// Revoke every paired token + clear the registry, then issue a code.
    RotateAll,
    /// Revoke a single device's token, then issue a code.
    RotateDevice(String),
}

#[cfg(feature = "agent-runtime")]
impl PaircodeAction {
    /// True when the action mints a new code (POST), false for `Show` (GET).
    fn mints_code(&self) -> bool {
        !matches!(self, PaircodeAction::Show)
    }

    /// True when the action revokes existing tokens.
    pub fn is_rotation(&self) -> bool {
        matches!(
            self,
            PaircodeAction::RotateAll | PaircodeAction::RotateDevice(_)
        )
    }

    /// The `rotate` query value to send, if any.
    fn rotate_query(&self) -> Option<String> {
        match self {
            PaircodeAction::RotateAll => Some("all".to_string()),
            PaircodeAction::RotateDevice(id) => Some(id.clone()),
            PaircodeAction::Show | PaircodeAction::AddClient => None,
        }
    }
}

/// Outcome of a `get-paircode` request.
#[cfg(feature = "agent-runtime")]
pub enum PaircodeResult {
    /// A code was returned (with an optional human-readable message).
    Code {
        code: String,
        message: Option<String>,
    },
    /// No code is available (with an optional explanatory message from the
    /// gateway, e.g. a revoke that succeeded but could not issue a code).
    NoCode { message: Option<String> },
}

#[cfg(feature = "agent-runtime")]
pub async fn fetch_paircode(
    host: &str,
    port: u16,
    path_prefix: Option<&str>,
    action: &PaircodeAction,
) -> Result<PaircodeResult> {
    let client = reqwest::Client::new();

    let response = if action.mints_code() {
        let mut url = gateway_admin_url(host, port, path_prefix, "/admin/paircode/new");
        if let Some(rotate) = action.rotate_query() {
            url.push_str("?rotate=");
            url.push_str(&urlencoding::encode(&rotate));
        }
        client
            .post(&url)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
    } else {
        let url = gateway_admin_url(host, port, path_prefix, "/admin/paircode");
        client
            .get(&url)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
    };

    let response = response.map_err(|e| {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
            "gateway paircode fetch: connect failed"
        );
        anyhow::Error::msg(format!("Failed to connect to gateway: {e}"))
    })?;

    let status = response.status();
    let json: serde_json::Value = response.json().await.map_err(|e| {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(
                    ::serde_json::json!({"error": format!("{}", e), "status": status.as_u16()})
                ),
            "gateway paircode response: JSON parse failed"
        );
        anyhow::Error::msg(format!("Gateway responded with status {status}: {e}"))
    })?;

    let message = json
        .get("message")
        .and_then(|v| v.as_str())
        .map(String::from);

    if json.get("success").and_then(|v| v.as_bool()) != Some(true) {
        if !status.is_success() {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"status": status.as_u16()})),
                "gateway paircode fetch returned non-success status"
            );
        }
        return Ok(PaircodeResult::NoCode { message });
    }

    match json.get("pairing_code").and_then(|v| v.as_str()) {
        Some(code) => Ok(PaircodeResult::Code {
            code: code.to_string(),
            message,
        }),
        None => Ok(PaircodeResult::NoCode { message }),
    }
}

#[cfg(feature = "agent-runtime")]
pub fn gateway_admin_url(
    host: &str,
    port: u16,
    path_prefix: Option<&str>,
    admin_path: &str,
) -> String {
    let prefix = path_prefix.unwrap_or("");
    format!("http://{host}:{port}{prefix}{admin_path}")
}

#[cfg(feature = "agent-runtime")]
pub fn paircode_no_code_message(
    host: &str,
    port: u16,
    default_host: &str,
    default_port: u16,
    action: &PaircodeAction,
    require_pairing: bool,
    gateway_message: Option<&str>,
) -> String {
    let mut lines = Vec::new();

    if let Some(message) = gateway_message.filter(|m| !m.trim().is_empty()) {
        lines.push(format!("⚠️  {message}"));
    } else if require_pairing {
        lines
            .push("🔐 Gateway pairing is enabled, but no active pairing code is available.".into());
    } else {
        lines.push(t(
            "cli-pairing-disabled",
            "⚠️  Gateway pairing is disabled in config.",
        ));
        lines.push("All requests will be accepted without authentication.".into());
        lines.push("To enable pairing, set [gateway] require_pairing = true.".into());
        return indent_paircode_lines(lines);
    }

    lines.push(String::new());
    match action {
        PaircodeAction::Show => {
            lines.push(
                "`zeroclaw gateway get-paircode` only displays an existing active code; it does not mint a new one."
                    .into(),
            );
            lines.push("To pair another device, run:".into());
            lines.push(paircode_command(
                host,
                port,
                default_host,
                default_port,
                Some("--new"),
            ));
            lines.push(String::new());
            lines.push("To revoke existing pairings and mint a replacement code, run:".into());
            lines.push(paircode_command(
                host,
                port,
                default_host,
                default_port,
                Some("--rotate"),
            ));
        }
        PaircodeAction::AddClient => {
            lines.push(
                "The gateway did not mint a new pairing code. A code may already be pending, or pairing may need a reset."
                    .into(),
            );
            lines.push(
                "Try again shortly, or revoke existing pairings and mint a replacement code:"
                    .into(),
            );
            lines.push(paircode_command(
                host,
                port,
                default_host,
                default_port,
                Some("--rotate"),
            ));
        }
        PaircodeAction::RotateAll | PaircodeAction::RotateDevice(_) => {
            lines.push("The rotate request completed without returning a replacement code.".into());
            lines.push("Check whether pairing is enabled, then request a new device code:".into());
            lines.push(paircode_command(
                host,
                port,
                default_host,
                default_port,
                Some("--new"),
            ));
        }
    }

    lines.push(String::new());
    lines.push("To inspect the running gateway:".into());
    lines.push(format!("    open http://{host}:{port}"));
    indent_paircode_lines(lines)
}

#[cfg(feature = "agent-runtime")]
fn paircode_command(
    host: &str,
    port: u16,
    default_host: &str,
    default_port: u16,
    flag: Option<&str>,
) -> String {
    let mut command = "    zeroclaw gateway get-paircode".to_string();
    if let Some(flag) = flag {
        command.push(' ');
        command.push_str(flag);
    }
    if port != default_port {
        write!(command, " --port {port}").expect("writing to String cannot fail");
    }
    if host != default_host {
        write!(command, " --host {host}").expect("writing to String cannot fail");
    }
    command
}

#[cfg(feature = "agent-runtime")]
fn indent_paircode_lines(lines: Vec<String>) -> String {
    lines
        .into_iter()
        .map(|line| {
            if line.starts_with("    ") || line.is_empty() {
                line
            } else {
                format!("  {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

//! Browser automation tool with pluggable backends.

use crate::helpers::domain_guard;
use anyhow::Context;
use async_trait::async_trait;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::net::ToSocketAddrs;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_config::policy::SecurityPolicy;

/// Computer-use sidecar settings.
#[derive(Clone)]
pub struct ComputerUseConfig {
    pub endpoint: String,
    pub api_key: Option<String>,
    pub timeout_ms: u64,
    pub allow_remote_endpoint: bool,
    pub window_allowlist: Vec<String>,
    pub max_coordinate_x: Option<i64>,
    pub max_coordinate_y: Option<i64>,
}

impl std::fmt::Debug for ComputerUseConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComputerUseConfig")
            .field("endpoint", &self.endpoint)
            .field("timeout_ms", &self.timeout_ms)
            .field("allow_remote_endpoint", &self.allow_remote_endpoint)
            .field("window_allowlist", &self.window_allowlist)
            .field("max_coordinate_x", &self.max_coordinate_x)
            .field("max_coordinate_y", &self.max_coordinate_y)
            .finish_non_exhaustive()
    }
}

impl Default for ComputerUseConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://127.0.0.1:8787/v1/actions".into(),
            api_key: None,
            timeout_ms: 15_000,
            allow_remote_endpoint: false,
            window_allowlist: Vec::new(),
            max_coordinate_x: None,
            max_coordinate_y: None,
        }
    }
}

/// Browser automation tool using pluggable backends.
pub struct BrowserTool {
    security: Arc<SecurityPolicy>,
    allowed_domains: Vec<String>,
    allowed_private_hosts: Vec<String>,
    session_name: Option<String>,
    backend: String,
    headed: Option<bool>,
    computer_use: ComputerUseConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserBackendKind {
    AgentBrowser,
    ComputerUse,
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedBackend {
    AgentBrowser,
    ComputerUse,
}

impl BrowserBackendKind {
    fn parse(raw: &str) -> anyhow::Result<Self> {
        let key = raw.trim().to_ascii_lowercase().replace('-', "_");
        match key.as_str() {
            "agent_browser" | "agentbrowser" => Ok(Self::AgentBrowser),
            "computer_use" | "computeruse" => Ok(Self::ComputerUse),
            "auto" => Ok(Self::Auto),
            "rust_native" | "native" => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({ "backend": raw })),
                    "browser backend 'rust_native' was removed"
                );
                anyhow::bail!(
                    "browser.backend '{raw}' is no longer supported: the rust_native (WebDriver) \
                     backend was removed. Use 'agent_browser', 'computer_use', or 'auto'"
                )
            }
            _ => anyhow::bail!(
                "Unsupported browser backend '{raw}'. Use 'agent_browser', 'computer_use', or 'auto'"
            ),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::AgentBrowser => "agent_browser",
            Self::ComputerUse => "computer_use",
            Self::Auto => "auto",
        }
    }
}

/// Response from agent-browser --json commands
#[derive(Debug, Deserialize)]
struct AgentBrowserResponse {
    success: bool,
    data: Option<Value>,
    error: Option<String>,
}

/// Response format from computer-use sidecar.
#[derive(Debug, Deserialize)]
struct ComputerUseResponse {
    #[serde(default)]
    success: Option<bool>,
    #[serde(default)]
    data: Option<Value>,
    #[serde(default)]
    error: Option<String>,
}

/// Supported browser actions
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserAction {
    /// Navigate to a URL
    Open { url: String },
    /// Get accessibility snapshot with refs
    Snapshot {
        #[serde(default)]
        interactive_only: bool,
        #[serde(default)]
        compact: bool,
        #[serde(default)]
        depth: Option<u32>,
    },
    /// Click an element by ref or selector
    Click { selector: String },
    /// Fill a form field
    Fill { selector: String, value: String },
    /// Type text into focused element
    Type { selector: String, text: String },
    /// Get text content of element
    GetText { selector: String },
    /// Get page title
    GetTitle,
    /// Get current URL
    GetUrl,
    /// Take screenshot
    Screenshot {
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        full_page: bool,
    },
    /// Wait for element or time
    Wait {
        #[serde(default)]
        selector: Option<String>,
        #[serde(default)]
        ms: Option<u64>,
        #[serde(default)]
        text: Option<String>,
    },
    /// Press a key
    Press { key: String },
    /// Hover over element
    Hover { selector: String },
    /// Scroll page
    Scroll {
        direction: String,
        #[serde(default)]
        pixels: Option<u32>,
    },
    /// Check if element is visible
    IsVisible { selector: String },
    /// Close browser
    Close,
    /// Find element by semantic locator
    Find {
        by: String, // role, text, label, placeholder, testid
        value: String,
        action: String, // click, fill, text, hover
        #[serde(default)]
        fill_value: Option<String>,
    },
}

impl BrowserTool {
    pub fn new(
        security: Arc<SecurityPolicy>,
        allowed_domains: Vec<String>,
        session_name: Option<String>,
    ) -> anyhow::Result<Self> {
        Self::new_with_backend(
            security,
            allowed_domains,
            session_name,
            "agent_browser".into(),
            None,
            ComputerUseConfig::default(),
            Vec::new(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_backend(
        security: Arc<SecurityPolicy>,
        allowed_domains: Vec<String>,
        session_name: Option<String>,
        backend: String,
        headed: Option<bool>,
        computer_use: ComputerUseConfig,
        allowed_private_hosts: Vec<String>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            security,
            allowed_domains: domain_guard::normalize_allowed_domains(
                allowed_domains,
                "browser.allowed_domains",
            )?,
            allowed_private_hosts: domain_guard::normalize_allowed_domains(
                allowed_private_hosts,
                "browser.allowed_private_hosts",
            )?,
            session_name,
            backend,
            headed,
            computer_use,
        })
    }

    /// Check if agent-browser CLI is available
    pub async fn is_agent_browser_available() -> bool {
        let cmd = if cfg!(target_os = "windows") {
            "agent-browser.cmd"
        } else {
            "agent-browser"
        };
        Command::new(cmd)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Backward-compatible alias.
    pub async fn is_available() -> bool {
        Self::is_agent_browser_available().await
    }

    fn configured_backend(&self) -> anyhow::Result<BrowserBackendKind> {
        BrowserBackendKind::parse(&self.backend)
    }

    fn computer_use_endpoint_url(&self) -> anyhow::Result<reqwest::Url> {
        if self.computer_use.timeout_ms == 0 {
            anyhow::bail!("browser.computer_use.timeout_ms must be > 0");
        }

        let endpoint = self.computer_use.endpoint.trim();
        if endpoint.is_empty() {
            anyhow::bail!("browser.computer_use.endpoint cannot be empty");
        }

        let parsed = reqwest::Url::parse(endpoint).map_err(|_| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"endpoint": endpoint})),
                "browser: invalid computer_use endpoint URL"
            );
            anyhow::Error::msg(format!(
                "Invalid browser.computer_use.endpoint: '{endpoint}'. Expected http(s) URL"
            ))
        })?;

        let scheme = parsed.scheme();
        if scheme != "http" && scheme != "https" {
            anyhow::bail!("browser.computer_use.endpoint must use http:// or https://");
        }

        let host = parsed.host_str().ok_or_else(|| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                "browser: browser.computer_use.endpoint must include host"
            );
            anyhow::Error::msg("browser.computer_use.endpoint must include host")
        })?;

        let host_is_private = domain_guard::is_private_or_local_host(host);
        if !self.computer_use.allow_remote_endpoint && !host_is_private {
            anyhow::bail!(
                "browser.computer_use.endpoint host '{host}' is public. Set browser.computer_use.allow_remote_endpoint=true to allow it"
            );
        }

        if self.computer_use.allow_remote_endpoint && !host_is_private && scheme != "https" {
            anyhow::bail!(
                "browser.computer_use.endpoint must use https:// when allow_remote_endpoint=true and host is public"
            );
        }

        Ok(parsed)
    }

    fn computer_use_available(&self) -> anyhow::Result<bool> {
        let endpoint = self.computer_use_endpoint_url()?;
        Ok(endpoint_reachable(&endpoint, Duration::from_millis(500)))
    }

    async fn resolve_backend(&self) -> anyhow::Result<ResolvedBackend> {
        let configured = self.configured_backend()?;

        match configured {
            BrowserBackendKind::AgentBrowser => {
                if Self::is_agent_browser_available().await {
                    Ok(ResolvedBackend::AgentBrowser)
                } else {
                    #[cfg(target_os = "windows")]
                    let install_hint = "Install with: npm install -g agent-browser (ensure npm global bin is in PATH)";
                    #[cfg(not(target_os = "windows"))]
                    let install_hint = "Install with: npm install -g agent-browser";
                    anyhow::bail!(
                        "browser.backend='{}' but agent-browser CLI is unavailable. {}",
                        configured.as_str(),
                        install_hint
                    )
                }
            }
            BrowserBackendKind::ComputerUse => {
                if !self.computer_use_available()? {
                    anyhow::bail!(
                        "browser.backend='computer_use' but sidecar endpoint is unreachable. Check browser.computer_use.endpoint and sidecar status"
                    );
                }
                Ok(ResolvedBackend::ComputerUse)
            }
            BrowserBackendKind::Auto => {
                if Self::is_agent_browser_available().await {
                    return Ok(ResolvedBackend::AgentBrowser);
                }

                let computer_use_err = match self.computer_use_available() {
                    Ok(true) => return Ok(ResolvedBackend::ComputerUse),
                    Ok(false) => None,
                    Err(err) => Some(err.to_string()),
                };

                if let Some(err) = computer_use_err {
                    anyhow::bail!(
                        "browser.backend='auto' needs agent-browser CLI or a valid computer-use sidecar (error: {err})"
                    );
                }

                anyhow::bail!(
                    "browser.backend='auto' needs agent-browser CLI or a computer-use sidecar"
                )
            }
        }
    }

    /// Validate URL against allowlist
    fn validate_url(&self, url: &str) -> anyhow::Result<()> {
        let url = url.trim();

        if url.is_empty() {
            anyhow::bail!("URL cannot be empty");
        }

        // Block file:// URLs — browser file access bypasses all SSRF and
        // domain-allowlist controls and can exfiltrate arbitrary local files.
        if url.starts_with("file://") {
            anyhow::bail!("file:// URLs are not allowed in browser automation");
        }

        if !url.starts_with("https://") && !url.starts_with("http://") {
            anyhow::bail!("Only http:// and https:// URLs are allowed");
        }

        let parsed = reqwest::Url::parse(url)
            .map_err(|e| anyhow::Error::msg(format!("Invalid URL format: {e}")))?;

        if !parsed.username().is_empty() || parsed.password().is_some() {
            anyhow::bail!("URL userinfo is not allowed");
        }

        if self.allowed_domains.is_empty() && self.allowed_private_hosts.is_empty() {
            anyhow::bail!(
                "Browser tool enabled but no allowed_domains configured. \
                Add [browser].allowed_domains in config.toml"
            );
        }

        let host_str = parsed
            .host_str()
            .ok_or_else(|| anyhow::Error::msg("URL must include a host"))?;

        let is_ipv6 = host_str.parse::<std::net::Ipv6Addr>().is_ok();
        let host = if is_ipv6 {
            format!("[{host_str}]")
        } else {
            host_str.to_lowercase()
        };

        let private_host = domain_guard::is_private_or_local_host(&host);
        let private_host_allowed = private_host
            && domain_guard::host_matches_allowlist(&host, &self.allowed_private_hosts);

        if private_host && !private_host_allowed {
            anyhow::bail!("Blocked local/private host: {host}");
        }

        if private_host_allowed {
            return Ok(());
        }

        if !domain_guard::host_matches_allowlist(&host, &self.allowed_domains) {
            anyhow::bail!("Host '{host}' not in browser.allowed_domains");
        }

        Ok(())
    }

    /// Execute an agent-browser command
    async fn run_command(&self, args: &[&str]) -> anyhow::Result<AgentBrowserResponse> {
        let mut cmd = self.agent_browser_command();

        // Add --json for machine-readable output
        cmd.args(args).arg("--json");

        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            &format!("Running: agent-browser {} --json", args.join(" "))
        );

        let output = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !stderr.is_empty() {
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                &format!("agent-browser stderr: {}", stderr)
            );
        }

        // Parse JSON response
        if let Ok(resp) = serde_json::from_str::<AgentBrowserResponse>(&stdout) {
            return Ok(resp);
        }

        // Fallback for non-JSON output
        if output.status.success() {
            Ok(AgentBrowserResponse {
                success: true,
                data: Some(json!({ "output": stdout.trim() })),
                error: None,
            })
        } else {
            Ok(AgentBrowserResponse {
                success: false,
                data: None,
                error: Some(stderr.trim().to_string()),
            })
        }
    }

    fn agent_browser_command(&self) -> Command {
        let agent_browser_bin = if cfg!(target_os = "windows") {
            "agent-browser.cmd"
        } else {
            "agent-browser"
        };
        let mut cmd = Command::new(agent_browser_bin);

        match self.headed {
            Some(true) => {
                cmd.env("AGENT_BROWSER_HEADED", "1");
            }
            Some(false) => {
                cmd.env_remove("AGENT_BROWSER_HEADED");
            }
            None => {}
        }

        // When running as a service (systemd/OpenRC), the process may lack
        // HOME which browsers need for profile directories.
        if is_service_environment() {
            ensure_browser_env(&mut cmd);
        }

        // Add session if configured
        if let Some(ref session) = self.session_name {
            cmd.arg("--session").arg(session);
        }

        cmd
    }

    /// Execute a browser action via agent-browser CLI
    #[allow(clippy::too_many_lines)]
    async fn execute_agent_browser_action(
        &self,
        action: BrowserAction,
    ) -> anyhow::Result<ToolResult> {
        match action {
            BrowserAction::Open { url } => {
                self.validate_url(&url)?;
                let resp = self.run_command(&["open", &url]).await?;
                self.to_result(resp)
            }

            BrowserAction::Snapshot {
                interactive_only,
                compact,
                depth,
            } => {
                let mut args = vec!["snapshot"];
                if interactive_only {
                    args.push("-i");
                }
                if compact {
                    args.push("-c");
                }
                let depth_str;
                if let Some(d) = depth {
                    args.push("-d");
                    depth_str = d.to_string();
                    args.push(&depth_str);
                }
                let resp = self.run_command(&args).await?;
                self.to_result(resp)
            }

            BrowserAction::Click { selector } => {
                let resp = self.run_command(&["click", &selector]).await?;
                self.to_result(resp)
            }

            BrowserAction::Fill { selector, value } => {
                let resp = self.run_command(&["fill", &selector, &value]).await?;
                self.to_result(resp)
            }

            BrowserAction::Type { selector, text } => {
                let resp = self.run_command(&["type", &selector, &text]).await?;
                self.to_result(resp)
            }

            BrowserAction::GetText { selector } => {
                let resp = self.run_command(&["get", "text", &selector]).await?;
                self.to_result(resp)
            }

            BrowserAction::GetTitle => {
                let resp = self.run_command(&["get", "title"]).await?;
                self.to_result(resp)
            }

            BrowserAction::GetUrl => {
                let resp = self.run_command(&["get", "url"]).await?;
                self.to_result(resp)
            }

            BrowserAction::Screenshot { path, full_page } => {
                let mut args = vec!["screenshot"];
                if let Some(ref p) = path {
                    args.push(p);
                }
                if full_page {
                    args.push("--full");
                }
                let resp = self.run_command(&args).await?;
                self.to_result(resp)
            }

            BrowserAction::Wait { selector, ms, text } => {
                let mut args = vec!["wait"];
                let ms_str;
                if let Some(sel) = selector.as_ref() {
                    args.push(sel);
                } else if let Some(millis) = ms {
                    ms_str = millis.to_string();
                    args.push(&ms_str);
                } else if let Some(ref t) = text {
                    args.push("--text");
                    args.push(t);
                }
                let resp = self.run_command(&args).await?;
                self.to_result(resp)
            }

            BrowserAction::Press { key } => {
                let resp = self.run_command(&["press", &key]).await?;
                self.to_result(resp)
            }

            BrowserAction::Hover { selector } => {
                let resp = self.run_command(&["hover", &selector]).await?;
                self.to_result(resp)
            }

            BrowserAction::Scroll { direction, pixels } => {
                let mut args = vec!["scroll", &direction];
                let px_str;
                if let Some(px) = pixels {
                    px_str = px.to_string();
                    args.push(&px_str);
                }
                let resp = self.run_command(&args).await?;
                self.to_result(resp)
            }

            BrowserAction::IsVisible { selector } => {
                let resp = self.run_command(&["is", "visible", &selector]).await?;
                self.to_result(resp)
            }

            BrowserAction::Close => {
                let resp = self.run_command(&["close"]).await?;
                self.to_result(resp)
            }

            BrowserAction::Find {
                by,
                value,
                action,
                fill_value,
            } => {
                let mut args = vec!["find", &by, &value, &action];
                if let Some(ref fv) = fill_value {
                    args.push(fv);
                }
                let resp = self.run_command(&args).await?;
                self.to_result(resp)
            }
        }
    }

    fn validate_coordinate(&self, key: &str, value: i64, max: Option<i64>) -> anyhow::Result<()> {
        if value < 0 {
            anyhow::bail!("'{key}' must be >= 0")
        }
        if let Some(limit) = max {
            if limit < 0 {
                anyhow::bail!("Configured coordinate limit for '{key}' must be >= 0")
            }
            if value > limit {
                anyhow::bail!("'{key}'={value} exceeds configured limit {limit}")
            }
        }
        Ok(())
    }

    fn read_required_i64(
        &self,
        params: &serde_json::Map<String, Value>,
        key: &str,
    ) -> anyhow::Result<i64> {
        params.get(key).and_then(Value::as_i64).ok_or_else(|| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                "browser: Missing or invalid '{key}' parameter"
            );
            anyhow::Error::msg("Missing or invalid '{key}' parameter")
        })
    }

    /// Validates screenshot destination path against workspace policy.
    /// Runs before any backend (agent-browser, rust-native, ComputerUse) writes a screenshot file.
    ///
    /// Applies the same guards as `file_write` / `file_edit`:
    /// 1. String-level `is_path_allowed` — rejects null bytes, `..` traversal, URL-encoded traversal
    /// 2. `resolve_tool_path` + `canonicalize` parent — resolves relative/tilde paths
    /// 3. `is_resolved_path_allowed` — confirms canonical parent is inside workspace allowlist
    /// 4. `is_runtime_config_path` — rejects `config.toml`, `config.toml.bak`, `.config.toml.tmp-*`
    /// 5. `symlink_metadata` — rejects existing symlink targets
    ///
    /// Replaces the raw path with the canonical target so backends write the checked string.
    async fn validate_screenshot_path(&self, action: &mut BrowserAction) -> anyhow::Result<()> {
        let BrowserAction::Screenshot { path, .. } = action else {
            return Ok(());
        };
        let Some(path_str) = path.as_ref() else {
            return Ok(());
        };

        // One canonical target validator shared by every backend. It returns
        // the checked target as a lossless UTF-8 string — never a lossy
        // conversion — so the backends write exactly the path that was allowed.
        *path = Some(self.validate_screenshot_target(path_str).await?);
        Ok(())
    }

    /// The single canonical screenshot-destination validator. Applies the same
    /// guards as `file_write` / `file_edit`:
    /// 1. String-level `is_path_allowed` — rejects null bytes, `..` traversal,
    ///    URL-encoded traversal.
    /// 2. `resolve_tool_path` + `canonicalize` parent — resolves relative/tilde
    ///    paths.
    /// 3. `is_resolved_path_allowed` — canonical parent inside the workspace
    ///    allowlist.
    /// 4. `is_runtime_config_path` — rejects `config.toml`, `config.toml.bak`,
    ///    `.config.toml.tmp-*`.
    /// 5. `symlink_metadata` — rejects existing symlink targets.
    /// 6. Rejects canonical destinations that are not valid UTF-8.
    ///
    /// Shared by the local backends (`validate_screenshot_path`) and the
    /// ComputerUse flow (`validate_screenshot_path_for_computer_use`) so one
    /// policy cannot drift between them.
    ///
    /// Returns the validated target as a lossless UTF-8 string. Every backend
    /// consumes the destination as a string (command argument, JSON value, or
    /// `tokio::fs::write(&str)`), so a canonical destination that is not valid
    /// UTF-8 is rejected here: a lossy conversion could change the pathname and
    /// name a location that never passed the allowlist.
    async fn validate_screenshot_target(&self, raw_path: &str) -> anyhow::Result<String> {
        // String-level reject (null bytes, .. traversal, URL-encoded traversal)
        if !self.security.is_path_allowed(raw_path) {
            let msg = crate::i18n::get_required_tool_string_with_args(
                "tool-browser-screenshot-error-path-not-allowed",
                &[("path", raw_path)],
            );
            anyhow::bail!("{msg}");
        }

        // Resolve relative / tilde paths against the workspace directory.
        let full = self.security.resolve_tool_path(raw_path);

        // The file does not exist yet, so canonicalize the *parent* directory
        // to verify it is inside the workspace allowlist.
        let parent = full.parent().unwrap_or(&full);
        let canonical = tokio::fs::canonicalize(parent).await.with_context(|| {
            crate::i18n::get_required_tool_string_with_args(
                "tool-browser-screenshot-error-parent-not-exist",
                &[
                    ("path", raw_path),
                    ("parent", &parent.display().to_string()),
                ],
            )
        })?;

        if !self.security.is_resolved_path_allowed(&canonical) {
            let msg = crate::i18n::get_required_tool_string_with_args(
                "tool-browser-screenshot-error-path-outside-workspace",
                &[
                    ("path", raw_path),
                    ("canonical", &canonical.display().to_string()),
                ],
            );
            anyhow::bail!("{msg}");
        }

        // Build the final *target* path (parent + file name) so we can apply
        // the same target-level guards the file_write / file_edit tools use.
        let Some(file_name) = full.file_name() else {
            let msg = crate::i18n::get_required_tool_string_with_args(
                "tool-browser-screenshot-error-missing-filename",
                &[("path", raw_path)],
            );
            anyhow::bail!("{msg}");
        };
        let resolved_target = canonical.join(file_name);

        if self.security.is_runtime_config_path(&resolved_target)
            || self.security.is_protected_persona_path(&resolved_target)
        {
            let msg = crate::i18n::get_required_tool_string_with_args(
                "tool-browser-screenshot-error-runtime-config-target",
                &[
                    ("path", raw_path),
                    ("target", &resolved_target.display().to_string()),
                ],
            );
            anyhow::bail!("{msg}");
        }

        // If the target already exists and is a symlink, refuse to follow it.
        if let Ok(meta) = tokio::fs::symlink_metadata(&resolved_target).await
            && meta.file_type().is_symlink()
        {
            let msg = crate::i18n::get_required_tool_string_with_args(
                "tool-browser-screenshot-error-symlink-target",
                &[("target", &resolved_target.display().to_string())],
            );
            anyhow::bail!("{msg}");
        }

        // The allowlist above validated the byte-preserving PathBuf. Every
        // backend receives the destination as a UTF-8 string, and a lossy
        // conversion (`to_string_lossy`) would silently replace non-UTF-8
        // bytes with U+FFFD — naming a pathname that never passed the policy.
        // Fail closed here, while we still hold the checked target: on Unix a
        // valid UTF-8 input can canonicalize (through a symlink) to a parent
        // containing non-UTF-8 bytes.
        let Some(resolved_str) = resolved_target.to_str() else {
            let msg = crate::i18n::get_required_tool_string_with_args(
                "tool-browser-screenshot-error-path-not-utf8",
                &[("path", raw_path)],
            );
            anyhow::bail!("{msg}");
        };

        Ok(resolved_str.to_string())
    }

    fn validate_computer_use_action(
        &self,
        action: &str,
        params: &serde_json::Map<String, Value>,
    ) -> anyhow::Result<()> {
        match action {
            "open" => {
                let url = params.get("url").and_then(Value::as_str).ok_or_else(|| {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                        "browser: Missing 'url' for open action"
                    );
                    anyhow::Error::msg("Missing 'url' for open action")
                })?;
                self.validate_url(url)?;
            }
            "mouse_move" | "mouse_click" => {
                let x = self.read_required_i64(params, "x")?;
                let y = self.read_required_i64(params, "y")?;
                self.validate_coordinate("x", x, self.computer_use.max_coordinate_x)?;
                self.validate_coordinate("y", y, self.computer_use.max_coordinate_y)?;
            }
            "mouse_drag" => {
                let from_x = self.read_required_i64(params, "from_x")?;
                let from_y = self.read_required_i64(params, "from_y")?;
                let to_x = self.read_required_i64(params, "to_x")?;
                let to_y = self.read_required_i64(params, "to_y")?;
                self.validate_coordinate("from_x", from_x, self.computer_use.max_coordinate_x)?;
                self.validate_coordinate("to_x", to_x, self.computer_use.max_coordinate_x)?;
                self.validate_coordinate("from_y", from_y, self.computer_use.max_coordinate_y)?;
                self.validate_coordinate("to_y", to_y, self.computer_use.max_coordinate_y)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Validates the screenshot path for the ComputerUse backend before the
    /// sidecar round-trip. Applies the same canonical workspace policy /
    /// runtime-config / symlink guards as the local backends (via
    /// [`Self::validate_screenshot_target`]) and classifies the raw destination
    /// into absent / valid string / invalid input.
    async fn validate_screenshot_path_for_computer_use(
        &self,
        action_str: &str,
        args: Value,
    ) -> anyhow::Result<Value> {
        if action_str != "screenshot" {
            // Not a screenshot action, pass through unchanged
            return Ok(args);
        }

        let path = args.get("path").cloned();

        // Classify path into Absent/String/NonString
        match &path {
            None | Some(Value::Null) => {
                // Absent: no path or null → inline PNG return
                Ok(args)
            }
            Some(Value::String(s)) if s.is_empty() => {
                // Absent: empty string → inline PNG return
                Ok(args)
            }
            Some(Value::String(path_str)) => {
                // String: validate against workspace through the one canonical
                // validator shared with the local backends.
                let mut args = args;
                let resolved_target = self.validate_screenshot_target(path_str).await?;

                // Store the validated path for local write after sidecar returns PNG.
                // Do NOT forward the path to the sidecar - it returns PNG bytes.
                if let Some(obj) = args.as_object_mut() {
                    obj.insert("path".to_string(), Value::String(resolved_target));
                }
                Ok(args)
            }
            Some(_) => {
                // NonString: integer, array, object → reject
                let msg = crate::i18n::get_required_tool_string_with_args(
                    "tool-browser-screenshot-error-computeruse-non-string-path",
                    &[("path", &format!("{path:?}"))],
                );
                anyhow::bail!("{msg}");
            }
        }
    }

    async fn execute_computer_use_action(
        &self,
        action: &str,
        args: &Value,
    ) -> anyhow::Result<ToolResult> {
        let endpoint = self.computer_use_endpoint_url()?;

        // Validate screenshot path but do NOT forward it to the sidecar.
        // The sidecar returns PNG bytes, and we perform the validated local write.
        let validated_path = if action == "screenshot" {
            match self
                .validate_screenshot_path_for_computer_use(action, args.clone())
                .await
            {
                Ok(validated_args) => {
                    // Extract the validated path from the returned args
                    validated_args
                        .get("path")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                }
                Err(e) => {
                    return Ok(ToolResult {
                        success: false,
                        output: ToolOutput::default(),
                        error: Some(e.to_string()),
                    });
                }
            }
        } else {
            None
        };

        // Build params without the path - sidecar should return PNG bytes
        let mut params = args.as_object().cloned().ok_or_else(|| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                "browser: screenshot args must be a JSON object"
            );
            anyhow::Error::msg(crate::i18n::get_required_tool_string(
                "tool-browser-screenshot-error-args-not-object",
            ))
        })?;

        // Remove path from params - we'll handle the write locally after validation
        params.remove("path");
        params.remove("action");

        self.validate_computer_use_action(action, &params)?;

        let payload = json!({
            "action": action,
            "params": params,
            "policy": {
                "allowed_domains": self.allowed_domains,
                "window_allowlist": self.computer_use.window_allowlist,
                "max_coordinate_x": self.computer_use.max_coordinate_x,
                "max_coordinate_y": self.computer_use.max_coordinate_y,
            },
            "metadata": {
                "session_name": self.session_name,
                "source": "zeroclaw.browser",
                "version": env!("CARGO_PKG_VERSION"),
            }
        });

        let client = zeroclaw_config::schema::build_runtime_proxy_client("tool.browser");
        let mut request = client
            .post(endpoint)
            .timeout(Duration::from_millis(self.computer_use.timeout_ms))
            .json(&payload);

        if let Some(api_key) = self.computer_use.api_key.as_deref() {
            let token = api_key.trim();
            if !token.is_empty() {
                request = request.bearer_auth(token);
            }
        }

        let response = request.send().await.with_context(|| {
            format!(
                "Failed to call computer-use sidecar at {}",
                self.computer_use.endpoint
            )
        })?;

        let status = response.status();
        let body = response
            .text()
            .await
            .context("Failed to read computer-use sidecar response body")?;

        // A path-bearing screenshot is the ONLY flow that transfers bytes from
        // the sidecar to the local filesystem. For that flow the tool must fail
        // closed: success requires a well-formed ComputerUseResponse with
        // success != false, a non-empty PNG payload, and a completed local
        // write. A non-JSON or structurally invalid 2xx body must NOT fall
        // through to a generic success (which would report success without
        // creating the requested file).
        let is_path_bearing_screenshot =
            action == "screenshot" && validated_path.as_deref().is_some_and(|p| !p.is_empty());

        if let Ok(parsed) = serde_json::from_str::<ComputerUseResponse>(&body) {
            if status.is_success() && parsed.success.unwrap_or(true) {
                // If this was a screenshot with a validated non-empty path, write the PNG
                // locally. Bind the validated path structurally (the path-bearing flag
                // above guarantees a non-empty Some here) instead of unwrapping a latent
                // panic site.
                if let Some(path_str) = validated_path.as_deref().filter(|p| !p.is_empty()) {
                    // Extract PNG data from the response
                    let png_data = parsed
                        .data
                        .as_ref()
                        .and_then(|d| d.get("png_base64"))
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            anyhow::Error::msg(crate::i18n::get_required_tool_string(
                                "tool-browser-screenshot-error-sidecar-no-png-data",
                            ))
                        })?;

                    // Decode and validate the PNG payload: it must decode to a
                    // non-empty buffer with a PNG signature. Base64-decodable
                    // arbitrary bytes are NOT a valid screenshot — writing them
                    // to the `.png` destination would turn the sidecar boundary
                    // into an arbitrary decoded-byte write.
                    let png_bytes = base64::engine::general_purpose::STANDARD
                        .decode(png_data)
                        .with_context(|| "Failed to decode PNG base64 data")?;
                    if png_bytes.is_empty() {
                        anyhow::bail!(crate::i18n::get_required_tool_string(
                            "tool-browser-screenshot-error-sidecar-empty-png",
                        ));
                    }
                    const PNG_SIGNATURE: &[u8] =
                        &[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
                    if !png_bytes.starts_with(PNG_SIGNATURE) {
                        anyhow::bail!(crate::i18n::get_required_tool_string(
                            "tool-browser-screenshot-error-sidecar-not-png",
                        ));
                    }

                    tokio::fs::write(path_str, &png_bytes)
                        .await
                        .with_context(|| format!("Failed to write screenshot to {path_str}"))?;

                    // Return success with the path information
                    let output = serde_json::to_string_pretty(&json!({
                        "backend": "computer_use",
                        "action": action,
                        "path": path_str,
                        "bytes": png_bytes.len(),
                    }))
                    .unwrap_or_default();

                    return Ok(ToolResult {
                        success: true,
                        output: output.into(),
                        error: None,
                    });
                }

                let output = parsed
                    .data
                    .map(|data| serde_json::to_string_pretty(&data).unwrap_or_default())
                    .unwrap_or_else(|| {
                        serde_json::to_string_pretty(&json!({
                            "backend": "computer_use",
                            "action": action,
                            "ok": true,
                        }))
                        .unwrap_or_default()
                    });

                return Ok(ToolResult {
                    success: true,
                    output: output.into(),
                    error: None,
                });
            }

            let error = parsed.error.or_else(|| {
                if status.is_success() && parsed.success == Some(false) {
                    Some("computer-use sidecar returned success=false".to_string())
                } else {
                    Some(format!(
                        "computer-use sidecar request failed with status {status}"
                    ))
                }
            });

            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error,
            });
        }

        if status.is_success() {
            if is_path_bearing_screenshot {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(crate::i18n::get_required_tool_string(
                        "tool-browser-screenshot-error-sidecar-non-json-success",
                    )),
                });
            }
            return Ok(ToolResult {
                success: true,
                output: body.into(),
                error: None,
            });
        }

        Ok(ToolResult {
            success: false,
            output: ToolOutput::default(),
            error: Some(format!(
                "computer-use sidecar request failed with status {status}: {}",
                body.trim()
            )),
        })
    }

    async fn execute_action(
        &self,
        mut action: BrowserAction,
        backend: ResolvedBackend,
    ) -> anyhow::Result<ToolResult> {
        // Validate screenshot path before any backend writes a file
        if matches!(action, BrowserAction::Screenshot { .. }) {
            self.validate_screenshot_path(&mut action).await?;
        }

        match backend {
            ResolvedBackend::AgentBrowser => self.execute_agent_browser_action(action).await,
            ResolvedBackend::ComputerUse => anyhow::bail!(
                "Internal error: computer_use backend must be handled before BrowserAction parsing"
            ),
        }
    }

    #[allow(clippy::unnecessary_wraps, clippy::unused_self)]
    fn to_result(&self, resp: AgentBrowserResponse) -> anyhow::Result<ToolResult> {
        if resp.success {
            let output = resp
                .data
                .map(|d| serde_json::to_string_pretty(&d).unwrap_or_default())
                .unwrap_or_default();
            Ok(ToolResult {
                success: true,
                output: output.into(),
                error: None,
            })
        } else {
            Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: resp.error,
            })
        }
    }
}

#[async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }

    fn description(&self) -> &str {
        concat!(
            "Web/browser automation with pluggable backends (agent-browser, rust-native, computer_use). ",
            "Supports DOM actions plus optional OS-level actions (mouse_move, mouse_click, mouse_drag, ",
            "key_type, key_press, screen_capture) through a computer-use sidecar. Use 'snapshot' to map ",
            "interactive elements to refs (@e1, @e2). Enforces browser.allowed_domains for open actions."
        )
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["open", "snapshot", "click", "fill", "type", "get_text",
                             "get_title", "get_url", "screenshot", "wait", "press",
                             "hover", "scroll", "is_visible", "close", "find",
                             "mouse_move", "mouse_click", "mouse_drag", "key_type",
                             "key_press", "screen_capture"],
                    "description": "Browser action to perform (OS-level actions require backend=computer_use)"
                },
                "url": {
                    "type": "string",
                    "description": "URL to navigate to (for 'open' action)"
                },
                "selector": {
                    "type": "string",
                    "description": "Element selector: @ref (e.g. @e1), CSS (#id, .class), or text=..."
                },
                "value": {
                    "type": "string",
                    "description": "Value to fill or type"
                },
                "text": {
                    "type": "string",
                    "description": "Text to type or wait for"
                },
                "key": {
                    "type": "string",
                    "description": "Key to press (Enter, Tab, Escape, etc.)"
                },
                "x": {
                    "type": "integer",
                    "description": "Screen X coordinate (computer_use: mouse_move/mouse_click)"
                },
                "y": {
                    "type": "integer",
                    "description": "Screen Y coordinate (computer_use: mouse_move/mouse_click)"
                },
                "from_x": {
                    "type": "integer",
                    "description": "Drag source X coordinate (computer_use: mouse_drag)"
                },
                "from_y": {
                    "type": "integer",
                    "description": "Drag source Y coordinate (computer_use: mouse_drag)"
                },
                "to_x": {
                    "type": "integer",
                    "description": "Drag target X coordinate (computer_use: mouse_drag)"
                },
                "to_y": {
                    "type": "integer",
                    "description": "Drag target Y coordinate (computer_use: mouse_drag)"
                },
                "button": {
                    "type": "string",
                    "enum": ["left", "right", "middle"],
                    "description": "Mouse button for computer_use mouse_click"
                },
                "direction": {
                    "type": "string",
                    "enum": ["up", "down", "left", "right"],
                    "description": "Scroll direction"
                },
                "pixels": {
                    "type": "integer",
                    "description": "Pixels to scroll"
                },
                "interactive_only": {
                    "type": "boolean",
                    "description": "For snapshot: only show interactive elements"
                },
                "compact": {
                    "type": "boolean",
                    "description": "For snapshot: remove empty structural elements"
                },
                "depth": {
                    "type": "integer",
                    "description": "For snapshot: limit tree depth"
                },
                "full_page": {
                    "type": "boolean",
                    "description": "For screenshot: capture full page"
                },
                "path": {
                    "type": "string",
                    "description": "File path for screenshot"
                },
                "ms": {
                    "type": "integer",
                    "description": "Milliseconds to wait"
                },
                "by": {
                    "type": "string",
                    "enum": ["role", "text", "label", "placeholder", "testid"],
                    "description": "For find: semantic locator type"
                },
                "find_action": {
                    "type": "string",
                    "enum": ["click", "fill", "text", "hover", "check"],
                    "description": "For find: action to perform on found element"
                },
                "fill_value": {
                    "type": "string",
                    "description": "For find with fill action: value to fill"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        // Security checks
        if !self.security.can_act() {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("Action blocked: autonomy is read-only".into()),
            });
        }

        // Rate limiting is applied by the RateLimitedTool wrapper at
        // registration time (see zeroclaw-runtime::tools::mod).

        let backend = match self.resolve_backend().await {
            Ok(selected) => selected,
            Err(error) => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(error.to_string()),
                });
            }
        };

        // Parse action from args
        let action_str = args.get("action").and_then(|v| v.as_str()).ok_or_else(|| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                "browser: Missing 'action' parameter"
            );
            anyhow::Error::msg("Missing 'action' parameter")
        })?;

        if !is_supported_browser_action(action_str) {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Unknown action: {action_str}")),
            });
        }

        if backend == ResolvedBackend::ComputerUse {
            return self.execute_computer_use_action(action_str, &args).await;
        }

        if is_computer_use_only_action(action_str) {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(unavailable_action_for_backend_error(action_str, backend)),
            });
        }

        let action = match parse_browser_action(action_str, &args) {
            Ok(a) => a,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(e.to_string()),
                });
            }
        };

        self.execute_action(action, backend).await
    }
}

// ── Action parsing ──────────────────────────────────────────────

/// Parse a JSON `args` object into a typed `BrowserAction`.
fn parse_browser_action(action_str: &str, args: &Value) -> anyhow::Result<BrowserAction> {
    match action_str {
        "open" => {
            let url = args.get("url").and_then(|v| v.as_str()).ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                    "browser: Missing 'url' for open action"
                );
                anyhow::Error::msg("Missing 'url' for open action")
            })?;
            Ok(BrowserAction::Open { url: url.into() })
        }
        "snapshot" => Ok(BrowserAction::Snapshot {
            interactive_only: args
                .get("interactive_only")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true),
            compact: args
                .get("compact")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true),
            depth: args
                .get("depth")
                .and_then(serde_json::Value::as_u64)
                .map(|d| u32::try_from(d).unwrap_or(u32::MAX)),
        }),
        "click" => {
            let selector = args
                .get("selector")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                        "browser: Missing 'selector' for click"
                    );
                    anyhow::Error::msg("Missing 'selector' for click")
                })?;
            Ok(BrowserAction::Click {
                selector: selector.into(),
            })
        }
        "fill" => {
            let selector = args
                .get("selector")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                        "browser: Missing 'selector' for fill"
                    );
                    anyhow::Error::msg("Missing 'selector' for fill")
                })?;
            let value = args.get("value").and_then(|v| v.as_str()).ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                    "browser: Missing 'value' for fill"
                );
                anyhow::Error::msg("Missing 'value' for fill")
            })?;
            Ok(BrowserAction::Fill {
                selector: selector.into(),
                value: value.into(),
            })
        }
        "type" => {
            let selector = args
                .get("selector")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                        "browser: Missing 'selector' for type"
                    );
                    anyhow::Error::msg("Missing 'selector' for type")
                })?;
            let text = args.get("text").and_then(|v| v.as_str()).ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                    "browser: Missing 'text' for type"
                );
                anyhow::Error::msg("Missing 'text' for type")
            })?;
            Ok(BrowserAction::Type {
                selector: selector.into(),
                text: text.into(),
            })
        }
        "get_text" => {
            let selector = args
                .get("selector")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                        "browser: Missing 'selector' for get_text"
                    );
                    anyhow::Error::msg("Missing 'selector' for get_text")
                })?;
            Ok(BrowserAction::GetText {
                selector: selector.into(),
            })
        }
        "get_title" => Ok(BrowserAction::GetTitle),
        "get_url" => Ok(BrowserAction::GetUrl),
        "screenshot" => {
            // Parse the raw optional destination once into absent / valid
            // string / invalid input. A present non-string `path` (number,
            // object, …) is invalid input and must be rejected up front — the
            // same contract the ComputerUse path enforces — instead of being
            // silently coerced to `None` (which would make the local backends
            // take an inline screenshot while ComputerUse rejects the same
            // input). An empty string means absent (inline screenshot), also
            // matching ComputerUse.
            match args.get("path") {
                None | Some(serde_json::Value::Null) => Ok(BrowserAction::Screenshot {
                    path: None,
                    full_page: args
                        .get("full_page")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                }),
                Some(serde_json::Value::String(s)) if s.is_empty() => {
                    Ok(BrowserAction::Screenshot {
                        path: None,
                        full_page: args
                            .get("full_page")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false),
                    })
                }
                Some(serde_json::Value::String(s)) => Ok(BrowserAction::Screenshot {
                    path: Some(s.clone()),
                    full_page: args
                        .get("full_page")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                }),
                Some(_) => Err(anyhow::Error::msg(crate::i18n::get_required_tool_string(
                    "tool-browser-screenshot-error-non-string-path",
                ))),
            }
        }
        "wait" => Ok(BrowserAction::Wait {
            selector: args
                .get("selector")
                .and_then(|v| v.as_str())
                .map(String::from),
            ms: args.get("ms").and_then(serde_json::Value::as_u64),
            text: args.get("text").and_then(|v| v.as_str()).map(String::from),
        }),
        "press" => {
            let key = args.get("key").and_then(|v| v.as_str()).ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                    "browser: Missing 'key' for press"
                );
                anyhow::Error::msg("Missing 'key' for press")
            })?;
            Ok(BrowserAction::Press { key: key.into() })
        }
        "hover" => {
            let selector = args
                .get("selector")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                        "browser: Missing 'selector' for hover"
                    );
                    anyhow::Error::msg("Missing 'selector' for hover")
                })?;
            Ok(BrowserAction::Hover {
                selector: selector.into(),
            })
        }
        "scroll" => {
            let direction = args
                .get("direction")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                        "browser: Missing 'direction' for scroll"
                    );
                    anyhow::Error::msg("Missing 'direction' for scroll")
                })?;
            Ok(BrowserAction::Scroll {
                direction: direction.into(),
                pixels: args
                    .get("pixels")
                    .and_then(serde_json::Value::as_u64)
                    .map(|p| u32::try_from(p).unwrap_or(u32::MAX)),
            })
        }
        "is_visible" => {
            let selector = args
                .get("selector")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                        "browser: Missing 'selector' for is_visible"
                    );
                    anyhow::Error::msg("Missing 'selector' for is_visible")
                })?;
            Ok(BrowserAction::IsVisible {
                selector: selector.into(),
            })
        }
        "close" => Ok(BrowserAction::Close),
        "find" => {
            let by = args.get("by").and_then(|v| v.as_str()).ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                    "browser: Missing 'by' for find"
                );
                anyhow::Error::msg("Missing 'by' for find")
            })?;
            let value = args.get("value").and_then(|v| v.as_str()).ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                    "browser: Missing 'value' for find"
                );
                anyhow::Error::msg("Missing 'value' for find")
            })?;
            let action = args
                .get("find_action")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                        "browser: Missing 'find_action' for find"
                    );
                    anyhow::Error::msg("Missing 'find_action' for find")
                })?;
            Ok(BrowserAction::Find {
                by: by.into(),
                value: value.into(),
                action: action.into(),
                fill_value: args
                    .get("fill_value")
                    .and_then(|v| v.as_str())
                    .map(String::from),
            })
        }
        other => anyhow::bail!("Unsupported browser action: {other}"),
    }
}

// ── Helper functions ─────────────────────────────────────────────

fn is_supported_browser_action(action: &str) -> bool {
    matches!(
        action,
        "open"
            | "snapshot"
            | "click"
            | "fill"
            | "type"
            | "get_text"
            | "get_title"
            | "get_url"
            | "screenshot"
            | "wait"
            | "press"
            | "hover"
            | "scroll"
            | "is_visible"
            | "close"
            | "find"
            | "mouse_move"
            | "mouse_click"
            | "mouse_drag"
            | "key_type"
            | "key_press"
            | "screen_capture"
    )
}

fn is_computer_use_only_action(action: &str) -> bool {
    matches!(
        action,
        "mouse_move" | "mouse_click" | "mouse_drag" | "key_type" | "key_press" | "screen_capture"
    )
}

fn backend_name(backend: ResolvedBackend) -> &'static str {
    match backend {
        ResolvedBackend::AgentBrowser => "agent_browser",
        ResolvedBackend::ComputerUse => "computer_use",
    }
}

fn unavailable_action_for_backend_error(action: &str, backend: ResolvedBackend) -> String {
    format!(
        "Action '{action}' is unavailable for backend '{}'",
        backend_name(backend)
    )
}

fn endpoint_reachable(endpoint: &reqwest::Url, timeout: Duration) -> bool {
    let host = match endpoint.host_str() {
        Some(host) if !host.is_empty() => host,
        _ => return false,
    };

    let port = match endpoint.port_or_known_default() {
        Some(port) => port,
        None => return false,
    };

    let mut addrs = match (host, port).to_socket_addrs() {
        Ok(addrs) => addrs,
        Err(_) => return false,
    };

    let addr = match addrs.next() {
        Some(addr) => addr,
        None => return false,
    };

    std::net::TcpStream::connect_timeout(&addr, timeout).is_ok()
}

/// Detect whether the current process is running inside a service environment
/// (e.g. systemd, OpenRC, or launchd) where the browser sandbox and
/// environment setup may be restricted.
fn is_service_environment() -> bool {
    if std::env::var_os("INVOCATION_ID").is_some() {
        return true;
    }
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        return true;
    }
    #[cfg(target_os = "linux")]
    if std::path::Path::new("/run/openrc").exists() && std::env::var_os("HOME").is_none() {
        return true;
    }
    #[cfg(target_os = "linux")]
    if std::env::var_os("HOME").is_none() {
        return true;
    }
    false
}

/// Ensure environment variables required by headless browsers are present
/// when running inside a service context.
fn ensure_browser_env(cmd: &mut Command) {
    if std::env::var_os("HOME").is_none() {
        cmd.env("HOME", "/tmp");
    }
    let existing = std::env::var("CHROMIUM_FLAGS").unwrap_or_default();
    if !existing.contains("--no-sandbox") {
        let new_flags = if existing.is_empty() {
            "--no-sandbox --disable-dev-shm-usage".to_string()
        } else {
            format!("{existing} --no-sandbox --disable-dev-shm-usage")
        };
        cmd.env("CHROMIUM_FLAGS", new_flags);
    }
}

#[cfg(test)]
mod tests;

//! MCP (Model Context Protocol) client — connects to external tool servers.
//!
//! Supports multiple transports: stdio (spawn local process), HTTP, and SSE.

use std::collections::HashMap;
use std::sync::Arc;
#[cfg(not(target_has_atomic = "64"))]
use std::sync::atomic::AtomicU32;
#[cfg(target_has_atomic = "64")]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, bail};
use serde_json::json;
use tokio::time::{Duration, timeout};

use crate::mcp_protocol::{JsonRpcRequest, MCP_PROTOCOL_VERSION, McpToolDef, McpToolsListResult};
use crate::mcp_transport::{McpTransportConn, McpTransportError, create_transport};
use zeroclaw_config::schema::McpServerConfig;

/// Timeout for receiving a response from an MCP server during init/list.
/// Prevents a hung server from blocking the daemon indefinitely.
const RECV_TIMEOUT_SECS: u64 = 30;

/// Maximum automatic reconnect attempts when the request was definitely not
/// sent (`NotSent`). Side-effecting calls are never auto-replayed after an
/// outcome-unknown transport failure.
const MAX_NOT_SENT_RETRIES: u32 = 2;

/// Fixed backoff between not-sent retry attempts (milliseconds).
const RECONNECT_BACKOFF_MS: u64 = 500;

/// Perform the MCP `initialize` + `notifications/initialized` handshake on a
/// transport. Shared by the initial [`McpServer::connect`] and the
/// recover-after-transport-error path in [`McpServer::call_tool`].
async fn handshake(transport: &dyn McpTransportConn, server_name: &str) -> Result<()> {
    let init_req = JsonRpcRequest::new(
        1,
        "initialize",
        json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "zeroclaw",
                "version": env!("CARGO_PKG_VERSION")
            }
        }),
    );

    let init_resp = timeout(
        Duration::from_secs(RECV_TIMEOUT_SECS),
        transport.send_and_recv(&init_req),
    )
    .await
    .with_context(|| {
        format!(
            "MCP server `{server_name}` timed out after {RECV_TIMEOUT_SECS}s waiting for initialize response"
        )
    })??;

    if init_resp.error.is_some() {
        bail!(
            "MCP server `{server_name}` rejected initialize: {:?}",
            init_resp.error
        );
    }

    // Notify the server the client is initialized (notifications expect no
    // response). Best effort — ignore errors.
    let notif = JsonRpcRequest::notification("notifications/initialized", json!({}));
    let _ = transport.send_and_recv(&notif).await;

    Ok(())
}

// ── Internal server state ──────────────────────────────────────────────────

/// Shared server state.
///
/// The transport is an `Arc<dyn McpTransportConn>` (shared `&self` API). Stdio
/// uses a worker/router so callers never hold a mutex across response waits;
/// HTTP/SSE keep exclusive serialization inside a transport-local mutex.
/// Metadata (`name` / `tools`) is never gated on in-flight RPCs.
struct McpServerInner {
    /// Canonical server config (also referenced by stdio transport via `Arc`).
    config: Arc<McpServerConfig>,
    transport: Arc<dyn McpTransportConn>,
    #[cfg(target_has_atomic = "64")]
    next_id: AtomicU64,
    #[cfg(not(target_has_atomic = "64"))]
    next_id: AtomicU32,
    tools: Vec<McpToolDef>,
}

// ── McpServer ──────────────────────────────────────────────────────────────

/// A live connection to one MCP server (any transport).
#[derive(Clone)]
pub struct McpServer {
    inner: Arc<McpServerInner>,
}

impl McpServer {
    /// Connect to the server, perform the initialize handshake, and fetch the tool list.
    pub async fn connect(config: McpServerConfig) -> Result<Self> {
        let config = Arc::new(config);
        let transport = create_transport(Arc::clone(&config)).with_context(|| {
            format!(
                "failed to create transport for MCP server `{}`",
                config.name
            )
        })?;

        // Initialize handshake (initialize + initialized notification)
        handshake(transport.as_ref(), &config.name).await?;

        // Fetch available tools
        let id = 2u64;
        let list_req = JsonRpcRequest::new(id, "tools/list", json!({}));

        let list_resp = timeout(
            Duration::from_secs(RECV_TIMEOUT_SECS),
            transport.send_and_recv(&list_req),
        )
        .await
        .with_context(|| {
            format!(
                "MCP server `{}` timed out after {}s waiting for tools/list response",
                config.name, RECV_TIMEOUT_SECS
            )
        })??;

        let result = list_resp.result.ok_or_else(|| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"mcp_server": &config.name})),
                "mcp_client: tools/list returned no result"
            );
            anyhow::Error::msg(format!(
                "tools/list returned no result from `{}`",
                config.name
            ))
        })?;
        let tool_list: McpToolsListResult = serde_json::from_value(result)
            .with_context(|| format!("failed to parse tools/list from `{}`", config.name))?;

        let tool_count = tool_list.tools.len();
        let server_name = config.name.clone();

        let inner = McpServerInner {
            config,
            transport,
            #[cfg(target_has_atomic = "64")]
            next_id: AtomicU64::new(3), // Start at 3 since we used 1 and 2
            #[cfg(not(target_has_atomic = "64"))]
            next_id: AtomicU32::new(3), // Start at 3 since we used 1 and 2
            tools: tool_list.tools,
        };

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            &format!("MCP server `{server_name}` connected — {tool_count} tool(s) available")
        );

        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Tools advertised by this server.
    pub async fn tools(&self) -> Vec<McpToolDef> {
        self.inner.tools.clone()
    }

    /// Server display name.
    pub async fn name(&self) -> String {
        self.inner.config.name.clone()
    }

    /// Reset transport + re-handshake for future calls. Propagates errors —
    /// never silently reuses a killed/unavailable transport.
    async fn recover_transport(&self) -> Result<()> {
        let server_name = &self.inner.config.name;
        self.inner
            .transport
            .reset()
            .await
            .with_context(|| format!("MCP server `{server_name}` failed to reset transport"))?;
        handshake(self.inner.transport.as_ref(), server_name)
            .await
            .with_context(|| {
                format!("MCP server `{server_name}` failed to re-handshake after transport reset")
            })?;
        Ok(())
    }

    /// Call a tool on this server. Returns the raw JSON result.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value> {
        // Canonical timeout policy from `McpServerConfig` (single source of truth).
        let tool_timeout = self.inner.config.resolved_tool_timeout_secs();
        let server_name = self.inner.config.name.clone();

        // Only retry when the request was definitely not sent. After any
        // outcome-unknown closure (timeout / transport closed / stale session
        // post-submit), recover the connection for *future* calls but do not
        // auto-replay this side-effecting tool invocation.
        let mut not_sent_attempt = 0u32;
        let resp = loop {
            let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
            let req = JsonRpcRequest::new(
                id,
                "tools/call",
                json!({ "name": tool_name, "arguments": arguments }),
            );

            // No server-level transport mutex: stdio worker routes by id;
            // HTTP/SSE serialize inside their SerialTransport only.
            let send_result = timeout(
                Duration::from_secs(tool_timeout),
                self.inner.transport.send_and_recv(&req),
            )
            .await;

            match send_result {
                Ok(Ok(resp)) => break resp,
                Ok(Err(err)) => match err.downcast_ref::<McpTransportError>() {
                    Some(McpTransportError::NotSent) if not_sent_attempt < MAX_NOT_SENT_RETRIES => {
                        not_sent_attempt += 1;
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Reconnect
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "mcp_server": &server_name,
                                "tool": tool_name,
                                "attempt": not_sent_attempt,
                                "max_attempts": MAX_NOT_SENT_RETRIES,
                            })),
                            "mcp_client: request not sent; recovering transport and retrying"
                        );
                        self.recover_transport().await?;
                        tokio::time::sleep(Duration::from_millis(RECONNECT_BACKOFF_MS)).await;
                        continue;
                    }
                    Some(
                        te @ (McpTransportError::StaleSession { .. }
                        | McpTransportError::TransportClosed
                        | McpTransportError::ResponseTimeout
                        | McpTransportError::OutcomeUnknown),
                    ) => {
                        let reason = te.to_string();
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Fail
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "mcp_server": &server_name,
                                "tool": tool_name,
                                "reason": &reason,
                            })),
                            "mcp_client: tool call outcome unknown; recovering transport without replay"
                        );
                        // Recover for future calls; surface outcome-unknown for this one.
                        self.recover_transport().await?;
                        return Err(anyhow::Error::new(McpTransportError::OutcomeUnknown))
                            .with_context(|| {
                                format!(
                                    "MCP server `{server_name}` tool call `{tool_name}` outcome unknown ({reason})"
                                )
                            });
                    }
                    _ => {
                        return Err(err).with_context(|| {
                            format!(
                                "MCP server `{server_name}` error during tool call `{tool_name}`"
                            )
                        });
                    }
                },
                Err(_) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Timeout)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "mcp_server": &server_name,
                                "tool": tool_name,
                                "timeout_secs": tool_timeout,
                            })),
                        "mcp_client: tool call timed out"
                    );
                    // Dropping the timed-out future cancels the stdio wait; recover
                    // explicitly and propagate reset/handshake failures.
                    self.recover_transport().await?;
                    return Err(anyhow::Error::msg(format!(
                        "MCP server `{server_name}` timed out after {tool_timeout}s during tool call `{tool_name}`"
                    )));
                }
            }
        };

        if let Some(err) = resp.error {
            bail!("MCP tool `{tool_name}` error {}: {}", err.code, err.message);
        }

        let result = resp.result.unwrap_or(serde_json::Value::Null);

        // MCP servers signal *tool-execution* failures (as opposed to JSON-RPC
        // protocol errors) with HTTP 200 + `result.isError: true` and the detail
        // in `result.content[].text`, per the MCP spec. Without surfacing this,
        // the error envelope is returned as a normal success — so the failure is
        // invisible to the model and the daemon log, and callers only ever see a
        // generic "error during tool call" with no detail.
        if result.get("isError").and_then(serde_json::Value::as_bool) == Some(true) {
            let detail = result
                .get("content")
                .and_then(|c| c.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .filter(|s: &String| !s.is_empty())
                .unwrap_or_else(|| "(no error detail returned by server)".to_string());
            // Server-controlled text: scrub secrets (sk-/ghp_/…) and bound length
            // (`sanitize_api_error` truncates to MAX_API_ERROR_CHARS) before it
            // reaches the daemon log or the returned error.
            let detail = zeroclaw_providers::sanitize_api_error(&detail);
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "mcp_server": &server_name,
                        "tool": tool_name,
                        "detail": &detail,
                    })),
                "mcp_client: tool returned isError:true"
            );
            bail!("MCP tool `{tool_name}` (server `{server_name}`) returned isError: {detail}");
        }

        Ok(result)
    }
}

// ── McpRegistry ───────────────────────────────────────────────────────────

/// Registry of all connected MCP servers, with a flat tool index.
pub struct McpRegistry {
    servers: Vec<McpServer>,
    /// prefixed_name → (server_index, original_tool_name)
    tool_index: HashMap<String, (usize, String)>,
}

impl McpRegistry {
    /// Connect to all configured servers. Non-fatal: failures are logged and skipped.
    pub async fn connect_all(configs: &[McpServerConfig]) -> Result<Self> {
        let mut servers = Vec::new();
        let mut tool_index = HashMap::new();

        for config in configs {
            match McpServer::connect(config.clone()).await {
                Ok(server) => {
                    let server_idx = servers.len();
                    // Collect tools while holding the lock once, then release
                    let tools = server.tools().await;
                    for tool in &tools {
                        // Prefix prevents name collisions across servers
                        let prefixed = format!("{}__{}", config.name, tool.name);
                        tool_index.insert(prefixed, (server_idx, tool.name.clone()));
                    }
                    servers.push(server);
                }
                // Non-fatal — log and continue with remaining servers
                Err(e) => {
                    ::zeroclaw_log::record!(
                        ERROR,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                        &format!("Failed to connect to MCP server `{}`: {:#}", config.name, e)
                    );
                }
            }
        }

        Ok(Self {
            servers,
            tool_index,
        })
    }

    /// All prefixed tool names across all connected servers.
    pub fn tool_names(&self) -> Vec<String> {
        self.tool_index.keys().cloned().collect()
    }

    /// Tool definition for a given prefixed name (cloned).
    pub async fn get_tool_def(&self, prefixed_name: &str) -> Option<McpToolDef> {
        let (server_idx, original_name) = self.tool_index.get(prefixed_name)?;
        self.servers[*server_idx]
            .inner
            .tools
            .iter()
            .find(|t| &t.name == original_name)
            .cloned()
    }

    /// Execute a tool by prefixed name.
    pub async fn call_tool(
        &self,
        prefixed_name: &str,
        arguments: serde_json::Value,
    ) -> Result<String> {
        let (server_idx, original_name) = self.tool_index.get(prefixed_name).ok_or_else(|| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"tool": prefixed_name})),
                "mcp_client: unknown MCP tool"
            );
            anyhow::Error::msg(format!("unknown MCP tool `{prefixed_name}`"))
        })?;
        let result = self.servers[*server_idx]
            .call_tool(original_name, arguments)
            .await?;
        serde_json::to_string_pretty(&result)
            .with_context(|| format!("failed to serialize result of MCP tool `{prefixed_name}`"))
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn server_count(&self) -> usize {
        self.servers.len()
    }

    pub fn tool_count(&self) -> usize {
        self.tool_index.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_config::schema::McpTransport;

    #[test]
    fn tool_name_prefix_format() {
        let prefixed = format!("{}__{}", "filesystem", "read_file");
        assert_eq!(prefixed, "filesystem__read_file");
    }

    #[tokio::test]
    async fn connect_nonexistent_command_fails_cleanly() {
        // A command that doesn't exist should fail at spawn, not panic.
        let config = McpServerConfig {
            name: "nonexistent".to_string(),
            command: "/usr/bin/this_binary_does_not_exist_zeroclaw_test".to_string(),
            args: vec![],
            env: std::collections::HashMap::default(),
            tool_timeout_secs: None,
            transport: McpTransport::Stdio,
            url: None,
            headers: std::collections::HashMap::default(),
        };
        let result = McpServer::connect(config).await;
        assert!(result.is_err());
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("failed to create transport"), "got: {msg}");
    }

    #[tokio::test]
    async fn connect_all_nonfatal_on_single_failure() {
        // If one server config is bad, connect_all should succeed (with 0 servers).
        let configs = vec![McpServerConfig {
            name: "bad".to_string(),
            command: "/usr/bin/does_not_exist_zc_test".to_string(),
            args: vec![],
            env: std::collections::HashMap::default(),
            tool_timeout_secs: None,
            transport: McpTransport::Stdio,
            url: None,
            headers: std::collections::HashMap::default(),
        }];
        let registry = McpRegistry::connect_all(&configs)
            .await
            .expect("connect_all should not fail");
        assert!(registry.is_empty());
        assert_eq!(registry.tool_count(), 0);
    }

    #[test]
    fn http_transport_requires_url() {
        let config = Arc::new(McpServerConfig {
            name: "test".into(),
            transport: McpTransport::Http,
            ..Default::default()
        });
        let result = create_transport(config);
        assert!(result.is_err());
    }

    #[test]
    fn sse_transport_requires_url() {
        let config = Arc::new(McpServerConfig {
            name: "test".into(),
            transport: McpTransport::Sse,
            ..Default::default()
        });
        let result = create_transport(config);
        assert!(result.is_err());
    }

    // ── Empty registry (no servers) ────────────────────────────────────────

    #[tokio::test]
    async fn empty_registry_is_empty() {
        let registry = McpRegistry::connect_all(&[])
            .await
            .expect("connect_all on empty slice should succeed");
        assert!(registry.is_empty());
        assert_eq!(registry.server_count(), 0);
        assert_eq!(registry.tool_count(), 0);
    }

    #[tokio::test]
    async fn empty_registry_tool_names_is_empty() {
        let registry = McpRegistry::connect_all(&[])
            .await
            .expect("connect_all should succeed");
        assert!(registry.tool_names().is_empty());
    }

    #[tokio::test]
    async fn empty_registry_get_tool_def_returns_none() {
        let registry = McpRegistry::connect_all(&[])
            .await
            .expect("connect_all should succeed");
        let result = registry.get_tool_def("nonexistent__tool").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn empty_registry_call_tool_unknown_name_returns_error() {
        let registry = McpRegistry::connect_all(&[])
            .await
            .expect("connect_all should succeed");
        let err = registry
            .call_tool("nonexistent__tool", serde_json::json!({}))
            .await
            .expect_err("should fail for unknown tool");
        assert!(err.to_string().contains("unknown MCP tool"), "got: {err}");
    }

    #[tokio::test]
    async fn connect_all_empty_gives_zero_servers() {
        let registry = McpRegistry::connect_all(&[])
            .await
            .expect("connect_all should succeed");
        // Verify all three count methods agree on zero.
        assert_eq!(registry.server_count(), 0);
        assert_eq!(registry.tool_count(), 0);
        assert!(registry.is_empty());
    }

    // ── McpServer::call_tool isError handling ──────────────────────────────
    //
    // These exercise the `result.isError == true` branch added to the
    // *inherent* `McpServer::call_tool` (the one that talks to the transport,
    // not the `McpRegistry::call_tool` wrapper). A fake transport returns a
    // canned result so no live server is needed.

    /// Transport that ignores the request and always returns one preset result.
    struct FakeTransport {
        result: serde_json::Value,
    }

    #[async_trait::async_trait]
    impl McpTransportConn for FakeTransport {
        async fn send_and_recv(
            &self,
            _request: &JsonRpcRequest,
        ) -> Result<crate::mcp_protocol::JsonRpcResponse> {
            Ok(crate::mcp_protocol::JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: Some(serde_json::json!(1)),
                result: Some(self.result.clone()),
                error: None,
            })
        }

        async fn close(&self) -> Result<()> {
            Ok(())
        }
    }

    /// Build an `McpServer` whose transport yields `result` on every call.
    fn server_returning(result: serde_json::Value) -> McpServer {
        let inner = McpServerInner {
            config: Arc::new(McpServerConfig {
                name: "fake".into(),
                ..Default::default()
            }),
            transport: Arc::new(FakeTransport { result }),
            #[cfg(target_has_atomic = "64")]
            next_id: AtomicU64::new(3),
            #[cfg(not(target_has_atomic = "64"))]
            next_id: AtomicU32::new(3),
            tools: vec![],
        };
        McpServer {
            inner: Arc::new(inner),
        }
    }

    #[tokio::test]
    async fn call_tool_iserror_err_is_sanitized_and_bounded() {
        // A secret token in the server-controlled detail must be redacted
        // before it reaches the returned error (and, by the same code path,
        // the daemon log).
        let server = server_returning(serde_json::json!({
            "isError": true,
            "content": [{ "type": "text", "text": "auth failed using sk-supersecrettoken12345abcdef" }],
        }));
        let err = server
            .call_tool("do_thing", serde_json::json!({}))
            .await
            .expect_err("isError:true must map to Err");
        let msg = err.to_string();
        assert!(msg.contains("returned isError"), "got: {msg}");
        assert!(msg.contains("[REDACTED]"), "secret not scrubbed: {msg}");
        assert!(
            !msg.contains("supersecrettoken"),
            "raw secret leaked: {msg}"
        );

        // Oversized server text must be truncated; sanitize_api_error caps the
        // detail at 500 chars and appends an ellipsis.
        let huge = "A".repeat(5000);
        let server = server_returning(serde_json::json!({
            "isError": true,
            "content": [{ "type": "text", "text": huge }],
        }));
        let msg = server
            .call_tool("do_thing", serde_json::json!({}))
            .await
            .expect_err("isError:true must map to Err")
            .to_string();
        assert!(
            msg.contains("..."),
            "bounded detail should be truncated: {msg}"
        );
        assert!(
            msg.len() < 1000,
            "5000-char payload not bounded: len={}",
            msg.len()
        );
    }

    #[tokio::test]
    async fn call_tool_success_returns_ok_result() {
        // isError absent → Ok with the raw result untouched.
        let payload = serde_json::json!({
            "content": [{ "type": "text", "text": "all good" }],
        });
        let out = server_returning(payload.clone())
            .call_tool("do_thing", serde_json::json!({}))
            .await
            .expect("absent isError must be Ok");
        assert_eq!(out, payload);

        // isError explicitly false → still Ok.
        let payload = serde_json::json!({ "isError": false, "value": 42 });
        let out = server_returning(payload.clone())
            .call_tool("do_thing", serde_json::json!({}))
            .await
            .expect("isError:false must be Ok");
        assert_eq!(out, payload);
    }

    #[tokio::test]
    async fn call_tool_iserror_empty_detail_falls_back() {
        // isError true but no content array → fallback message.
        let msg = server_returning(serde_json::json!({ "isError": true }))
            .call_tool("do_thing", serde_json::json!({}))
            .await
            .expect_err("isError:true must map to Err")
            .to_string();
        assert!(
            msg.contains("(no error detail returned by server)"),
            "got: {msg}"
        );

        // isError true with content present but empty text → same fallback.
        let msg = server_returning(serde_json::json!({
            "isError": true,
            "content": [{ "type": "text", "text": "" }],
        }))
        .call_tool("do_thing", serde_json::json!({}))
        .await
        .expect_err("isError:true must map to Err")
        .to_string();
        assert!(
            msg.contains("(no error detail returned by server)"),
            "got: {msg}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_stdio_registry_reaps_child_process() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;
        use tokio::time::{Duration, sleep};

        fn process_is_alive(pid: u32) -> bool {
            std::process::Command::new("kill")
                .arg("-0")
                .arg(pid.to_string())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        }

        async fn read_pid(path: &Path) -> u32 {
            for _ in 0..50 {
                if let Ok(raw) = tokio::fs::read_to_string(path).await
                    && let Ok(pid) = raw.trim().parse()
                {
                    return pid;
                }
                sleep(Duration::from_millis(20)).await;
            }
            panic!("stdio MCP test server did not write its pid");
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let server_path = temp.path().join("echo-mcp.sh");
        let pid_path = temp.path().join("echo-mcp.pid");
        let mut script = std::fs::File::create(&server_path).expect("script");
        script
            .write_all(
                br#"#!/bin/sh
echo "$$" > "$1"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"echo-mcp","version":"0.1.0"}}}'
      ;;
    *'"method":"tools/list"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}'
      exec tail -f /dev/null
      ;;
  esac
done
"#,
            )
            .expect("write script");
        drop(script);
        let mut perms = std::fs::metadata(&server_path)
            .expect("metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&server_path, perms).expect("chmod");

        let config = McpServerConfig {
            name: "echo".to_string(),
            command: server_path.display().to_string(),
            args: vec![pid_path.display().to_string()],
            env: std::collections::HashMap::default(),
            tool_timeout_secs: None,
            transport: McpTransport::Stdio,
            url: None,
            headers: std::collections::HashMap::default(),
        };

        let registry = McpRegistry::connect_all(&[config])
            .await
            .expect("connect_all should not fail");
        assert_eq!(registry.server_count(), 1);
        assert_eq!(registry.tool_count(), 0);
        let child_pid = read_pid(&pid_path).await;
        assert!(
            process_is_alive(child_pid),
            "stdio MCP child should be alive while the registry is alive"
        );

        drop(registry);

        for _ in 0..50 {
            if !process_is_alive(child_pid) {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
        panic!("stdio MCP child process {child_pid} survived after registry drop");
    }

    // ── Reconnect on stale session (streamable HTTP) ───────────────────────

    fn http_server_config(uri: String) -> McpServerConfig {
        McpServerConfig {
            name: "remote".into(),
            transport: McpTransport::Http,
            url: Some(uri),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn call_tool_stale_session_surfaces_outcome_unknown_then_next_call_works() {
        use wiremock::matchers::{body_partial_json, method};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        // initialize → 200 + session header. Hit twice: initial connect plus the
        // recover-after-stale-session re-handshake (no tool replay).
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "initialize"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Mcp-Session-Id", "sess-1")
                    .set_body_json(json!({"jsonrpc": "2.0", "id": 1, "result": {}})),
            )
            .expect(2)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "notifications/initialized"}),
            ))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "tools/list"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {"tools": [{"name": "echo", "description": "d", "inputSchema": {"type": "object"}}]}
            })))
            .expect(1)
            .mount(&server)
            .await;

        // First tools/call → 404 (stale session). Not auto-replayed.
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "tools/call"})))
            .respond_with(ResponseTemplate::new(404))
            .up_to_n_times(1)
            .with_priority(1)
            .expect(1)
            .mount(&server)
            .await;

        // Subsequent tools/call after recovery → success.
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "tools/call"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 3, "result": {"ok": true}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let srv = McpServer::connect(http_server_config(server.uri()))
            .await
            .expect("connect");
        let err = srv
            .call_tool("echo", json!({}))
            .await
            .expect_err("stale session must not auto-replay tools/call");
        assert!(
            format!("{err:#}").contains("outcome unknown")
                || err
                    .downcast_ref::<McpTransportError>()
                    .is_some_and(|e| matches!(e, McpTransportError::OutcomeUnknown)),
            "got: {err:#}"
        );
        let result = srv
            .call_tool("echo", json!({}))
            .await
            .expect("next call after recovery should succeed");
        assert_eq!(result, json!({"ok": true}));
        server.verify().await;
    }

    #[tokio::test]
    async fn call_tool_does_not_retry_on_tool_error() {
        use wiremock::matchers::{body_partial_json, method};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        // initialize is expected exactly once — a genuine tool error must NOT
        // trigger a reconnect (which would re-run initialize).
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "initialize"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Mcp-Session-Id", "sess-1")
                    .set_body_json(json!({"jsonrpc": "2.0", "id": 1, "result": {}})),
            )
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "notifications/initialized"}),
            ))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "tools/list"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {"tools": [{"name": "echo", "description": "d", "inputSchema": {"type": "object"}}]}
            })))
            .mount(&server)
            .await;

        // tools/call → JSON-RPC error body over HTTP 200 (a real tool failure).
        // Expected exactly once: no retry.
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "tools/call"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 3, "error": {"code": -32000, "message": "boom"}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let srv = McpServer::connect(http_server_config(server.uri()))
            .await
            .expect("connect");
        let err = srv
            .call_tool("echo", json!({}))
            .await
            .expect_err("tool error should surface");
        assert!(err.to_string().contains("boom"), "got: {err}");
        server.verify().await;
    }

    #[tokio::test]
    async fn call_tool_does_not_retry_sessionless_404() {
        use wiremock::matchers::{body_partial_json, method};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        // initialize returns 200 with NO Mcp-Session-Id header — a stateless server,
        // so the transport never holds a session id. Expected exactly once: a 404
        // with no session in play must NOT trigger a reconnect (re-running initialize).
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "initialize"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"jsonrpc": "2.0", "id": 1, "result": {}})),
            )
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "notifications/initialized"}),
            ))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "tools/list"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {"tools": [{"name": "echo", "description": "d", "inputSchema": {"type": "object"}}]}
            })))
            .mount(&server)
            .await;

        // tools/call → 404 with no session. This is a missing endpoint, not a stale
        // session: it surfaces as a plain error and is hit exactly once (no retry).
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "tools/call"})))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        let srv = McpServer::connect(http_server_config(server.uri()))
            .await
            .expect("connect");
        let err = srv
            .call_tool("echo", json!({}))
            .await
            .expect_err("sessionless 404 should surface as an error");
        // The 404 lives in the error source chain (call_tool wraps it with context).
        assert!(
            format!("{err:?}").contains("MCP server returned HTTP 404"),
            "got: {err:?}"
        );
        // server.verify() pins the no-retry: initialize and tools/call each hit once.
        server.verify().await;
    }

    #[test]
    fn no_duplicate_timeout_constants_in_client() {
        // Split needles so this assertion text cannot false-positive.
        let src = include_str!("mcp_client.rs");
        let default_needle = ["const DEFAULT_TOOL_", "TIMEOUT_SECS: u64"].concat();
        let max_needle = ["const MAX_TOOL_", "TIMEOUT_SECS: u64"].concat();
        assert!(
            !src.contains(&default_needle),
            "client must resolve timeouts from McpServerConfig, not local 180"
        );
        assert!(
            !src.contains(&max_needle),
            "client must resolve timeouts from McpServerConfig, not local 600"
        );
        assert_eq!(
            McpServerConfig::default().resolved_tool_timeout_secs(),
            McpServerConfig::DEFAULT_TOOL_TIMEOUT_SECS
        );
    }

    /// Stdio MCP script helpers shared by acceptance tests.
    #[cfg(unix)]
    mod stdio_scripts {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::path::{Path, PathBuf};

        pub fn write_executable(path: &Path, body: &[u8]) {
            let mut script = std::fs::File::create(path).expect("script");
            script.write_all(body).expect("write");
            drop(script);
            let mut perms = std::fs::metadata(path).expect("metadata").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms).expect("chmod");
        }

        pub fn temp_script(name: &str, body: &[u8]) -> (tempfile::TempDir, PathBuf) {
            let temp = tempfile::tempdir().expect("tempdir");
            let path = temp.path().join(name);
            write_executable(&path, body);
            (temp, path)
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_skips_mismatched_id_then_returns_matching_response() {
        let (_tmp, server_path) = stdio_scripts::temp_script(
            "mismatch-mcp.sh",
            br#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"m","version":"0"}}}'
      ;;
    *'"method":"tools/list"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"d","inputSchema":{"type":"object"}}]}}'
      ;;
    *'"method":"tools/call"'*)
      # Wrong id first, then the matching id (3 on first tools/call).
      printf '%s\n' '{"jsonrpc":"2.0","id":999,"result":{"wrong":true}}'
      printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"ok":true}}'
      ;;
  esac
done
"#,
        );
        let config = McpServerConfig {
            name: "mismatch".into(),
            command: server_path.display().to_string(),
            transport: McpTransport::Stdio,
            ..Default::default()
        };
        let srv = McpServer::connect(config).await.expect("connect");
        let result = srv
            .call_tool("echo", json!({}))
            .await
            .expect("matching id must win");
        assert_eq!(result, json!({"ok": true}));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_server_request_with_colliding_id_not_accepted_as_response() {
        let (_tmp, server_path) = stdio_scripts::temp_script(
            "collide-mcp.sh",
            br#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"c","version":"0"}}}'
      ;;
    *'"method":"tools/list"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"d","inputSchema":{"type":"object"}}]}}'
      ;;
    *'"method":"tools/call"'*)
      # Same id as the in-flight tools/call, but this is a server request.
      printf '%s\n' '{"jsonrpc":"2.0","id":3,"method":"roots/list","params":{}}'
      printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"ok":true}}'
      ;;
  esac
done
"#,
        );
        let config = McpServerConfig {
            name: "collide".into(),
            command: server_path.display().to_string(),
            transport: McpTransport::Stdio,
            ..Default::default()
        };
        let srv = McpServer::connect(config).await.expect("connect");
        let result = srv
            .call_tool("echo", json!({}))
            .await
            .expect("server request must not be mistaken for the response");
        assert_eq!(result, json!({"ok": true}));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_caller_cancellation_does_not_poison_next_call() {
        let temp = tempfile::tempdir().expect("tempdir");
        let server_path = temp.path().join("cancel-mcp.sh");
        let state_path = temp.path().join("slow.once");
        stdio_scripts::write_executable(
            &server_path,
            br#"#!/bin/sh
STATE="$1"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"x","version":"0"}}}'
      ;;
    *'"method":"tools/list"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"d","inputSchema":{"type":"object"}}]}}'
      ;;
    *'"method":"tools/call"'*)
      if [ ! -f "$STATE" ]; then
        touch "$STATE"
        # Hang until the client cancels / worker resets (SIGKILL).
        sleep 30
      else
        id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p' | head -n1)
        printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id:-3},\"result\":{\"ok\":true}}"
      fi
      ;;
  esac
done
"#,
        );
        let config = McpServerConfig {
            name: "cancel".into(),
            command: server_path.display().to_string(),
            args: vec![state_path.display().to_string()],
            transport: McpTransport::Stdio,
            tool_timeout_secs: Some(1),
            ..Default::default()
        };
        let srv = McpServer::connect(config).await.expect("connect");
        let err = srv
            .call_tool("echo", json!({}))
            .await
            .expect_err("first call should time out");
        assert!(
            err.to_string().contains("timed out") || format!("{err:#}").contains("outcome unknown"),
            "got: {err:#}"
        );
        // Same server after recover: marker file survives respawn, so the
        // replacement child answers quickly — no poisoned partial-frame reuse.
        let result = srv
            .call_tool("echo", json!({}))
            .await
            .expect("next call after cancellation must succeed");
        assert_eq!(result, json!({"ok": true}));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_concurrent_metadata_not_blocked_by_inflight_tool() {
        use tokio::time::{Duration, Instant, timeout};

        let (_tmp, server_path) = stdio_scripts::temp_script(
            "slow-mcp.sh",
            br#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"slow","version":"0"}}}'
      ;;
    *'"method":"tools/list"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"d","inputSchema":{"type":"object"}}]}}'
      ;;
    *'"method":"tools/call"'*)
      sleep 2
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p' | head -n1)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id:-3},\"result\":{\"ok\":true}}"
      ;;
  esac
done
"#,
        );
        let config = McpServerConfig {
            name: "slow".into(),
            command: server_path.display().to_string(),
            transport: McpTransport::Stdio,
            tool_timeout_secs: Some(10),
            ..Default::default()
        };
        let srv = McpServer::connect(config).await.expect("connect");
        let srv2 = srv.clone();
        let tool = zeroclaw_spawn::spawn!(async move { srv2.call_tool("echo", json!({})).await });
        // Give the tool call a head start so it is in-flight.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let started = Instant::now();
        // Metadata must not wait on the slow tools/call (no server HOL mutex).
        let name = timeout(Duration::from_millis(200), srv.name())
            .await
            .expect("name must not block behind in-flight tool");
        let tools = timeout(Duration::from_millis(200), srv.tools())
            .await
            .expect("tools must not block behind in-flight tool");
        assert_eq!(name, "slow");
        assert_eq!(tools.len(), 1);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "metadata took too long: {:?}",
            started.elapsed()
        );
        // Stdio writes are still serialized by the worker; document that by
        // also submitting a second call and ensuring both complete.
        let srv3 = srv.clone();
        let tool2 = zeroclaw_spawn::spawn!(async move { srv3.call_tool("echo", json!({})).await });
        let r1 = tool.await.expect("join").expect("tool1");
        let r2 = tool2.await.expect("join").expect("tool2");
        assert_eq!(r1, json!({"ok": true}));
        assert_eq!(r2, json!({"ok": true}));
    }
}

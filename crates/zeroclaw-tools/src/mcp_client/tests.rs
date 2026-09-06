#[cfg(test)]
use super::*;
use crate::mcp_transport::create_transport;
#[cfg(unix)]
use crate::mcp_transport::{StdioTransport, StdioWriteTestHook};
use zeroclaw_config::schema::McpTransport;

#[cfg(unix)]
fn write_executable_script(path: &std::path::Path, body: &[u8]) {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let mut script = std::fs::File::create(path).expect("create script");
    script.write_all(body).expect("write script");
    drop(script);
    let mut permissions = std::fs::metadata(path)
        .expect("script metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("chmod script");
}

#[cfg(unix)]
fn make_fifo(path: &std::path::Path) {
    let status = std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("run mkfifo");
    assert!(status.success(), "mkfifo failed for {}", path.display());
}

#[cfg(unix)]
async fn read_fifo(path: &std::path::Path) -> String {
    tokio::time::timeout(Duration::from_secs(5), tokio::fs::read_to_string(path))
        .await
        .expect("fifo writer timed out")
        .expect("read fifo")
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
fn stdio_test_config(
    name: &str,
    script: &std::path::Path,
    args: Vec<String>,
    timeout_secs: u64,
) -> McpServerConfig {
    McpServerConfig {
        name: name.to_string(),
        command: script.display().to_string(),
        args,
        tool_timeout_secs: Some(timeout_secs),
        transport: McpTransport::Stdio,
        ..Default::default()
    }
}

#[test]
fn tool_name_prefix_format() {
    let prefixed = format!("{}__{}", "filesystem", "read_file");
    assert_eq!(prefixed, "filesystem__read_file");
}

#[test]
fn split_prefix_separates_server_and_rest() {
    assert_eq!(
        McpRegistry::split_prefixed("srvA__file:///x"),
        Some(("srvA".to_string(), "file:///x".to_string()))
    );
    assert_eq!(McpRegistry::split_prefixed("noprefix"), None);
}

#[tokio::test]
async fn registry_server_supports_flags_default_false() {
    let registry = McpRegistry::connect_all(&[]).await.expect("connect_all");
    assert!(!registry.server_supports_resources("missing").await);
    assert!(!registry.server_supports_prompts("missing").await);
}

#[tokio::test]
async fn registry_read_resource_unknown_server_errors() {
    let registry = McpRegistry::connect_all(&[]).await.expect("connect_all");
    let err = registry
        .read_resource("ghost__file:///x")
        .await
        .expect_err("unknown server should error");
    assert!(err.to_string().contains("unknown MCP server"), "got: {err}");
}

#[tokio::test]
async fn registry_get_prompt_unknown_server_errors() {
    let registry = McpRegistry::connect_all(&[]).await.expect("connect_all");
    let err = registry
        .get_prompt("ghost__p", serde_json::json!({}))
        .await
        .expect_err("unknown server should error");
    assert!(err.to_string().contains("unknown MCP server"), "got: {err}");
}

#[tokio::test]
async fn registry_list_all_empty_for_empty_registry() {
    let registry = McpRegistry::connect_all(&[]).await.expect("connect_all");
    assert!(registry.list_all_resources().await.is_empty());
    assert!(registry.list_all_prompts().await.is_empty());
}

#[tokio::test]
async fn list_server_prompts_prefixes_name_and_returns_cursor() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    // initialize advertises prompts capability so the method is not gated.
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Mcp-Session-Id", "s")
                .set_body_json(json!({
                    "jsonrpc":"2.0","id":1,
                    "result":{"capabilities":{"prompts":{}}}
                })),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method":"notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method":"tools/list"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc":"2.0","id":2,"result":{"tools":[]}
        })))
        .mount(&server)
        .await;
    // prompts/list returns a bare name plus a nextCursor.
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method":"prompts/list"})))
        .respond_with(|request: &wiremock::Request| {
            let id = serde_json::from_slice::<serde_json::Value>(&request.body)
                .expect("JSON-RPC request")
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc":"2.0","id":id,
                "result":{"prompts":[{"name":"summarize"}],"nextCursor":"page2"}
            }))
        })
        .mount(&server)
        .await;

    let registry = McpRegistry::connect_all(&[http_server_config(server.uri())])
        .await
        .expect("connect_all");

    // The configured server name is "remote" (see http_server_config).
    let (defs, next) = registry
        .list_server_prompts("remote", None)
        .await
        .expect("list_server_prompts should succeed");
    assert_eq!(defs.len(), 1);
    // Regression: the listed name must be the prefixed form that `get` needs.
    assert_eq!(defs[0].name, "remote__summarize");
    // Regression: the server's nextCursor must be surfaced to the caller.
    assert_eq!(next.as_deref(), Some("page2"));

    // And list_all_prompts must also carry the prefixed name in the def.
    let all = registry.list_all_prompts().await;
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].1.name, "remote__summarize");
}

#[tokio::test]
async fn connect_nonexistent_command_fails_cleanly() {
    // A command that doesn't exist should fail at spawn, not panic.
    let config = McpServerConfig {
        pinned_resources: Vec::new(),
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
        pinned_resources: Vec::new(),
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
    let config = McpServerConfig {
        pinned_resources: Vec::new(),
        name: "test".into(),
        transport: McpTransport::Http,
        ..Default::default()
    };
    let result = create_transport(&config);
    assert!(result.is_err());
}

#[test]
fn sse_transport_requires_url() {
    let config = McpServerConfig {
        name: "test".into(),
        transport: McpTransport::Sse,
        ..Default::default()
    };
    let result = create_transport(&config);
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

/// Transport that ignores the request and always returns one preset result.
struct FakeTransport {
    result: serde_json::Value,
}

#[async_trait::async_trait]
impl SharedMcpTransportConn for FakeTransport {
    async fn send_and_recv(
        &self,
        request: &JsonRpcRequest,
        _lifecycle: &McpRequestLifecycle,
    ) -> Result<crate::mcp_protocol::JsonRpcResponse> {
        Ok(crate::mcp_protocol::JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: request.id.clone(),
            result: Some(self.result.clone()),
            error: None,
        })
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

fn server_with_transport(
    name: &str,
    transport: Arc<dyn SharedMcpTransportConn>,
    timeout_secs: u64,
) -> McpServer {
    let inner = McpServerInner {
        config: McpServerConfig {
            name: name.into(),
            tool_timeout_secs: Some(timeout_secs),
            ..Default::default()
        },
        #[cfg(target_has_atomic = "64")]
        next_id: AtomicU64::new(3),
        #[cfg(not(target_has_atomic = "64"))]
        next_id: AtomicU32::new(3),
        tools: vec![],
        capabilities: McpServerCapabilities::default(),
        peer: PeerProtocol::legacy_default(),
        list_caches: ListCaches::default(),
        tools_ttl: ToolsTtl::Sticky,
        tasks: McpTaskStore::new(),
    };
    McpServer {
        inner: Arc::new(Mutex::new(inner)),
        transport,
        epoch_gate: Arc::new(RwLock::new(0)),
        serial_gate: None,
        recovery: Arc::new(RecoveryBarrier::new()),
    }
}

struct PreWriteBlockingTransport {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    resets: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl SharedMcpTransportConn for PreWriteBlockingTransport {
    async fn send_and_recv(
        &self,
        _request: &JsonRpcRequest,
        _lifecycle: &McpRequestLifecycle,
    ) -> Result<crate::mcp_protocol::JsonRpcResponse> {
        self.entered.notify_one();
        self.release.notified().await;
        Err(McpTransportError::TransportClosed.into())
    }

    async fn reset(&self) -> Result<()> {
        self.resets.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn cancellation_before_write_does_not_reset_or_replay() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let resets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let transport: Arc<dyn SharedMcpTransportConn> = Arc::new(PreWriteBlockingTransport {
        entered: Arc::clone(&entered),
        release,
        resets: Arc::clone(&resets),
    });
    let server = server_with_transport("pre-write", transport, 5);
    let call_server = server.clone();
    let call =
        zeroclaw_spawn::spawn!(async move { call_server.call_tool("test", json!({})).await });
    entered.notified().await;
    call.abort();
    assert!(
        call.await
            .expect_err("call must be cancelled")
            .is_cancelled()
    );
    tokio::task::yield_now().await;
    assert_eq!(resets.load(Ordering::SeqCst), 0);
}

struct CancellationSafeRecoveryTransport {
    tool_calls: Arc<std::sync::atomic::AtomicUsize>,
    resets: Arc<std::sync::atomic::AtomicUsize>,
    handshake_entered: Arc<tokio::sync::Notify>,
    release_handshake: Arc<tokio::sync::Notify>,
    handshake_completed: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl SharedMcpTransportConn for CancellationSafeRecoveryTransport {
    async fn send_and_recv(
        &self,
        request: &JsonRpcRequest,
        _lifecycle: &McpRequestLifecycle,
    ) -> Result<crate::mcp_protocol::JsonRpcResponse> {
        match request.method.as_str() {
            "tools/call" if self.tool_calls.fetch_add(1, Ordering::SeqCst) == 0 => {
                Err(McpTransportError::TransportClosed.into())
            }
            "initialize" => {
                self.handshake_entered.notify_one();
                self.release_handshake.notified().await;
                Ok(crate::mcp_protocol::JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id: request.id.clone(),
                    result: Some(json!({
                        "protocolVersion": MCP_PROTOCOL_VERSION,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "recovery", "version": "1"}
                    })),
                    error: None,
                })
            }
            "notifications/initialized" => {
                self.handshake_completed.notify_one();
                Ok(crate::mcp_protocol::JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id: None,
                    result: None,
                    error: None,
                })
            }
            "tools/call" => Ok(crate::mcp_protocol::JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: request.id.clone(),
                result: Some(json!({"ok": true})),
                error: None,
            }),
            other => panic!("unexpected method {other}"),
        }
    }

    async fn reset(&self) -> Result<()> {
        self.resets.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn cancellation_during_recovery_does_not_abandon_rehandshake() {
    let tool_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let resets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handshake_entered = Arc::new(tokio::sync::Notify::new());
    let release_handshake = Arc::new(tokio::sync::Notify::new());
    let handshake_completed = Arc::new(tokio::sync::Notify::new());
    let transport: Arc<dyn SharedMcpTransportConn> = Arc::new(CancellationSafeRecoveryTransport {
        tool_calls: Arc::clone(&tool_calls),
        resets: Arc::clone(&resets),
        handshake_entered: Arc::clone(&handshake_entered),
        release_handshake: Arc::clone(&release_handshake),
        handshake_completed: Arc::clone(&handshake_completed),
    });
    let server = server_with_transport("cancel-recovery", transport, 5);

    let call_server = server.clone();
    let call =
        zeroclaw_spawn::spawn!(
            async move { call_server.call_tool("side_effect", json!({})).await }
        );
    handshake_entered.notified().await;
    call.abort();
    assert!(
        call.await
            .expect_err("call must be cancelled")
            .is_cancelled()
    );

    release_handshake.notify_one();
    timeout(Duration::from_secs(2), handshake_completed.notified())
        .await
        .expect("detached recovery must finish its handshake");

    let result = timeout(Duration::from_secs(2), server.call_tool("probe", json!({})))
        .await
        .expect("next call must not hang behind abandoned recovery")
        .expect("next call must use recovered connection");
    assert_eq!(result, json!({"ok": true}));
    assert_eq!(resets.load(Ordering::SeqCst), 1);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 2);
}

struct FailedResetTransport;

#[async_trait::async_trait]
impl SharedMcpTransportConn for FailedResetTransport {
    async fn send_and_recv(
        &self,
        _request: &JsonRpcRequest,
        _lifecycle: &McpRequestLifecycle,
    ) -> Result<crate::mcp_protocol::JsonRpcResponse> {
        unreachable!("failed-reset test never sends a request")
    }

    async fn reset(&self) -> Result<()> {
        bail!("reset/reap failed")
    }

    async fn close(&self) -> Result<()> {
        bail!("cleanup failed")
    }
}

#[tokio::test]
async fn recovery_surfaces_reset_and_cleanup_failures() {
    let transport: Arc<dyn SharedMcpTransportConn> = Arc::new(FailedResetTransport);
    let server = server_with_transport("broken", transport, 5);
    let error = server
        .reestablish(0)
        .await
        .expect_err("failed reset and cleanup must surface");
    let detail = format!("{error:#}");
    assert!(detail.contains("reset/reap failed"), "got: {detail}");
    assert!(detail.contains("cleanup failed"), "got: {detail}");
}

struct TimeoutThenRecoverTransport {
    tool_calls: Arc<std::sync::atomic::AtomicUsize>,
    recovered: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl SharedMcpTransportConn for TimeoutThenRecoverTransport {
    async fn send_and_recv(
        &self,
        request: &JsonRpcRequest,
        lifecycle: &McpRequestLifecycle,
    ) -> Result<crate::mcp_protocol::JsonRpcResponse> {
        if request.method == "tools/call" {
            self.tool_calls.fetch_add(1, Ordering::SeqCst);
            lifecycle.mark_outcome_unknown(0);
            std::future::pending().await
        }
        Ok(crate::mcp_protocol::JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: request.id.clone(),
            result: Some(json!({})),
            error: None,
        })
    }

    async fn reset(&self) -> Result<()> {
        self.recovered.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn configured_timeout_recovers_without_replaying_outcome_unknown_tool() {
    let tool_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let recovered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let transport: Arc<dyn SharedMcpTransportConn> = Arc::new(TimeoutThenRecoverTransport {
        tool_calls: Arc::clone(&tool_calls),
        recovered: Arc::clone(&recovered),
    });
    let server = server_with_transport("timeout", transport, 1);

    let error = timeout(
        Duration::from_secs(2),
        server.call_tool("side_effect", json!({})),
    )
    .await
    .expect("configured timeout must return promptly")
    .expect_err("tool must time out");
    assert!(
        error.to_string().contains("outcome unknown") && error.to_string().contains("not replayed"),
        "got: {error:#}"
    );
    timeout(Duration::from_secs(2), async {
        while !recovered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("recovery did not run");
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
}

/// Like `server_with_transport` but with an HTTP/SSE-style serial gate so a
/// second concurrent write queues behind the first.
fn server_with_serialized_transport(
    name: &str,
    transport: Arc<dyn SharedMcpTransportConn>,
    timeout_secs: u64,
) -> McpServer {
    let mut server = server_with_transport(name, transport, timeout_secs);
    server.serial_gate = Some(Arc::new(Mutex::new(())));
    server
}

/// Transport whose first `tools/call` marks the outcome unknown after
/// "writing" and then hangs (the caller cancels it). A later `reset` +
/// re-handshake succeeds. Records the exact ordering of writes vs. reset so
/// tests can prove a queued second call never writes on the ambiguous
/// session before recovery completes.
struct QueuedAfterUnknownTransport {
    tool_writes: Arc<std::sync::atomic::AtomicUsize>,
    reset_done: Arc<std::sync::atomic::AtomicBool>,
    wrote_before_reset: Arc<std::sync::atomic::AtomicBool>,
    first_entered: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl SharedMcpTransportConn for QueuedAfterUnknownTransport {
    async fn send_and_recv(
        &self,
        request: &JsonRpcRequest,
        lifecycle: &McpRequestLifecycle,
    ) -> Result<crate::mcp_protocol::JsonRpcResponse> {
        match request.method.as_str() {
            "tools/call" => {
                let n = self.tool_writes.fetch_add(1, Ordering::SeqCst);
                if !self.reset_done.load(Ordering::SeqCst) && n > 0 {
                    // A write reached the transport while recovery had not
                    // yet reset the session — the exact bug we guard.
                    self.wrote_before_reset.store(true, Ordering::SeqCst);
                }
                if n == 0 {
                    // First call: outcome becomes unknown after the write,
                    // then the future hangs until the caller cancels it.
                    lifecycle.mark_outcome_unknown(0);
                    self.first_entered.notify_one();
                    std::future::pending::<()>().await;
                    unreachable!("cancelled before resuming");
                }
                Ok(crate::mcp_protocol::JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id: request.id.clone(),
                    result: Some(json!({"ok": true})),
                    error: None,
                })
            }
            "initialize" => Ok(crate::mcp_protocol::JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: request.id.clone(),
                result: Some(json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "queued", "version": "1"}
                })),
                error: None,
            }),
            "notifications/initialized" => Ok(crate::mcp_protocol::JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: None,
                result: None,
                error: None,
            }),
            other => panic!("unexpected method {other}"),
        }
    }

    async fn reset(&self) -> Result<()> {
        self.reset_done.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

/// A second serialized call queued while the first call's outcome is
/// unknown must wait for reset + re-handshake and must never write on the
/// ambiguous session before recovery completes.
#[tokio::test]
async fn queued_write_waits_for_recovery_after_outcome_unknown() {
    let tool_writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let reset_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let wrote_before_reset = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let first_entered = Arc::new(tokio::sync::Notify::new());
    let transport: Arc<dyn SharedMcpTransportConn> = Arc::new(QueuedAfterUnknownTransport {
        tool_writes: Arc::clone(&tool_writes),
        reset_done: Arc::clone(&reset_done),
        wrote_before_reset: Arc::clone(&wrote_before_reset),
        first_entered: Arc::clone(&first_entered),
    });
    let server = server_with_serialized_transport("queued", transport, 5);

    // First call writes, marks outcome-unknown, then hangs; cancel it.
    let first_server = server.clone();
    let first =
        zeroclaw_spawn::spawn!(
            async move { first_server.call_tool("side_effect", json!({})).await }
        );
    first_entered.notified().await;
    first.abort();
    let _ = first.await;

    // The queued second call must not resolve until recovery reset the
    // session, and must never write before that reset.
    let second = timeout(Duration::from_secs(3), server.call_tool("probe", json!({})))
        .await
        .expect("second call must not hang behind recovery")
        .expect("second call must succeed on the recovered session");
    assert_eq!(second, json!({"ok": true}));
    assert!(
        !wrote_before_reset.load(Ordering::SeqCst),
        "second call wrote on the ambiguous session before reset/re-handshake"
    );
    assert!(
        reset_done.load(Ordering::SeqCst),
        "recovery must have reset the session before the second write"
    );
    // Two tool writes total: the cancelled first and the recovered second.
    assert_eq!(tool_writes.load(Ordering::SeqCst), 2);
}

/// Transport whose first `tools/call` becomes outcome-unknown after writing,
/// but whose recovery re-handshake permanently fails.
struct FailedRehandshakeTransport {
    tool_writes: Arc<std::sync::atomic::AtomicUsize>,
    first_entered: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl SharedMcpTransportConn for FailedRehandshakeTransport {
    async fn send_and_recv(
        &self,
        request: &JsonRpcRequest,
        lifecycle: &McpRequestLifecycle,
    ) -> Result<crate::mcp_protocol::JsonRpcResponse> {
        match request.method.as_str() {
            "tools/call" => {
                self.tool_writes.fetch_add(1, Ordering::SeqCst);
                lifecycle.mark_outcome_unknown(0);
                self.first_entered.notify_one();
                std::future::pending::<()>().await;
                unreachable!("cancelled before resuming");
            }
            // Re-handshake fails: `initialize` errors during recovery.
            "initialize" => bail!("re-handshake refused"),
            other => panic!("unexpected method {other}"),
        }
    }

    async fn reset(&self) -> Result<()> {
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

/// After a post-write outcome-unknown request whose recovery re-handshake
/// fails, later calls must fail closed instead of writing on an
/// unrecovered/unhandshaken session.
#[tokio::test]
async fn failed_rehandshake_fails_closed_for_later_calls() {
    let tool_writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let first_entered = Arc::new(tokio::sync::Notify::new());
    let transport: Arc<dyn SharedMcpTransportConn> = Arc::new(FailedRehandshakeTransport {
        tool_writes: Arc::clone(&tool_writes),
        first_entered: Arc::clone(&first_entered),
    });
    let server = server_with_serialized_transport("failing", transport, 5);

    let first_server = server.clone();
    let first =
        zeroclaw_spawn::spawn!(
            async move { first_server.call_tool("side_effect", json!({})).await }
        );
    first_entered.notified().await;
    first.abort();
    let _ = first.await;

    // Recovery (detached) must poison the barrier once its re-handshake
    // fails; the next call then fails closed without writing.
    let error = timeout(Duration::from_secs(3), server.call_tool("probe", json!({})))
        .await
        .expect("later call must not hang behind a failed recovery")
        .expect_err("later call must fail closed after failed re-handshake");
    let detail = format!("{error:#}");
    assert!(
        detail.contains("unavailable") || detail.contains("recovery failed"),
        "expected fail-closed error, got: {detail}"
    );
    // Only the first (cancelled) call ever wrote a tool request.
    assert_eq!(tool_writes.load(Ordering::SeqCst), 1);
}

/// Build an `McpServer` whose transport yields `result` on every call.
fn server_returning(result: serde_json::Value) -> McpServer {
    let transport: Arc<dyn SharedMcpTransportConn> = Arc::new(FakeTransport { result });
    let inner = McpServerInner {
        config: McpServerConfig {
            name: "fake".into(),
            ..Default::default()
        },
        #[cfg(target_has_atomic = "64")]
        next_id: AtomicU64::new(3),
        #[cfg(not(target_has_atomic = "64"))]
        next_id: AtomicU32::new(3),
        tools: vec![],
        capabilities: McpServerCapabilities::default(),
        peer: PeerProtocol::legacy_default(),
        list_caches: ListCaches::default(),
        tools_ttl: ToolsTtl::Sticky,
        tasks: McpTaskStore::new(),
    };
    McpServer {
        inner: Arc::new(Mutex::new(inner)),
        transport,
        epoch_gate: Arc::new(RwLock::new(0)),
        serial_gate: None,
        recovery: Arc::new(RecoveryBarrier::new()),
    }
}

/// Like `server_returning`, but with explicit advertised capabilities.
fn server_with_caps_returning(
    capabilities: McpServerCapabilities,
    result: serde_json::Value,
) -> McpServer {
    let transport: Arc<dyn SharedMcpTransportConn> = Arc::new(FakeTransport { result });
    let inner = McpServerInner {
        config: McpServerConfig {
            name: "fake".into(),
            ..Default::default()
        },
        #[cfg(target_has_atomic = "64")]
        next_id: AtomicU64::new(3),
        #[cfg(not(target_has_atomic = "64"))]
        next_id: AtomicU32::new(3),
        tools: vec![],
        capabilities,
        peer: PeerProtocol::legacy_default(),
        list_caches: ListCaches::default(),
        tools_ttl: ToolsTtl::Sticky,
        tasks: McpTaskStore::new(),
    };
    McpServer {
        inner: Arc::new(Mutex::new(inner)),
        transport,
        epoch_gate: Arc::new(RwLock::new(0)),
        serial_gate: None,
        recovery: Arc::new(RecoveryBarrier::new()),
    }
}

#[tokio::test]
async fn list_resources_gated_when_unsupported() {
    let server = server_returning(serde_json::json!({}));
    let err = server
        .list_resources(None)
        .await
        .expect_err("unsupported resources must error locally");
    assert!(
        err.to_string().contains("does not support resources"),
        "got: {err}"
    );
}

#[tokio::test]
async fn list_resources_parses_when_supported() {
    let server = server_with_caps_returning(
        McpServerCapabilities {
            resources: true,
            prompts: false,
        },
        serde_json::json!({"resources":[{"uri":"u","name":"n"}],"nextCursor":"c"}),
    );
    let res = server.list_resources(None).await.expect("should parse");
    assert_eq!(res.resources.len(), 1);
    assert_eq!(res.next_cursor.as_deref(), Some("c"));
}

#[tokio::test]
async fn get_prompt_gated_when_unsupported() {
    let server = server_returning(serde_json::json!({}));
    let err = server
        .get_prompt("p", serde_json::json!({}))
        .await
        .expect_err("unsupported prompts must error locally");
    assert!(
        err.to_string().contains("does not support prompts"),
        "got: {err}"
    );
}

#[tokio::test]
async fn get_prompt_parses_when_supported() {
    let server = server_with_caps_returning(
        McpServerCapabilities {
            resources: false,
            prompts: true,
        },
        serde_json::json!({"messages":[{"role":"user","content":{"type":"text","text":"hi"}}]}),
    );
    let res = server
        .get_prompt("p", serde_json::json!({}))
        .await
        .expect("parse");
    assert_eq!(res.messages.len(), 1);
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
*'"method":"server/discover"'*)
  printf '%s\n' '{"jsonrpc":"2.0","id":0,"error":{"code":-32601,"message":"Method not found"}}'
  ;;
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
        pinned_resources: Vec::new(),
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

#[cfg(unix)]
#[tokio::test]
async fn stdio_concurrent_calls_route_mismatched_and_out_of_order_replies() {
    let temp = tempfile::tempdir().expect("tempdir");
    let script_path = temp.path().join("multiplex-mcp.sh");
    let first_received = temp.path().join("first-received.fifo");
    make_fifo(&first_received);
    write_executable_script(
        &script_path,
        br#"#!/bin/sh
first_id=
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
*'"method":"server/discover"'*)
  printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"error\":{\"code\":-32601,\"message\":\"Method not found\"}}"
  ;;
*'"method":"initialize"'*)
  printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"multiplex\",\"version\":\"1\"}}}"
  ;;
*'"method":"tools/list"'*)
  printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"A\",\"inputSchema\":{\"type\":\"object\"}},{\"name\":\"B\",\"inputSchema\":{\"type\":\"object\"}}]}}"
  ;;
*'"method":"tools/call"'*'"name":"A"'*)
  first_id=$id
  printf '%s\n' ready > "$1"
  ;;
*'"method":"tools/call"'*'"name":"B"'*)
  printf '%s\n' '{"jsonrpc":"2.0","id":"3","result":{"which":"wrong-shape"}}'
  printf '%s\n' '{"jsonrpc":"2.0","id":999999,"result":{"which":"wrong-id"}}'
  printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"which\":\"B\"}}"
  printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$first_id,\"result\":{\"which\":\"A\"}}"
  ;;
  esac
done
"#,
    );

    let server = McpServer::connect(stdio_test_config(
        "multiplex",
        &script_path,
        vec![first_received.display().to_string()],
        5,
    ))
    .await
    .expect("connect");
    let first_server = server.clone();
    let first = zeroclaw_spawn::spawn!(async move { first_server.call_tool("A", json!({})).await });
    assert_eq!(read_fifo(&first_received).await.trim(), "ready");
    let second_server = server.clone();
    let second =
        zeroclaw_spawn::spawn!(async move { second_server.call_tool("B", json!({})).await });

    let (first_result, second_result) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(first, second)
    })
    .await
    .expect("multiplexed calls timed out");
    assert_eq!(
        first_result.expect("first task").expect("first response"),
        json!({"which":"A"})
    );
    assert_eq!(
        second_result
            .expect("second task")
            .expect("second response"),
        json!({"which":"B"})
    );
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_post_write_cancellation_reaps_rehandshakes_and_never_replays() {
    let temp = tempfile::tempdir().expect("tempdir");
    let script_path = temp.path().join("cancel-mcp.sh");
    let effect_ready = temp.path().join("effect-ready.fifo");
    let recovered = temp.path().join("recovered.fifo");
    let generations = temp.path().join("generations.log");
    let effects = temp.path().join("effects.log");
    make_fifo(&effect_ready);
    make_fifo(&recovered);
    write_executable_script(
        &script_path,
        br#"#!/bin/sh
printf '%s\n' "$$" >> "$3"
generation=$(wc -l < "$3" | tr -d ' ')
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
*'"method":"server/discover"'*)
  printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"error\":{\"code\":-32601,\"message\":\"Method not found\"}}"
  ;;
*'"method":"initialize"'*)
  printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"cancel\",\"version\":\"1\"}}}"
  ;;
*'"method":"notifications/initialized"'*)
  if [ "$generation" -gt 1 ]; then printf '%s\n' recovered > "$2"; fi
  ;;
*'"method":"tools/list"'*)
  printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"side_effect\",\"inputSchema\":{\"type\":\"object\"}},{\"name\":\"probe\",\"inputSchema\":{\"type\":\"object\"}}]}}"
  ;;
*'"method":"tools/call"'*'"name":"side_effect"'*)
  printf '%s\n' effect >> "$4"
  if [ "$generation" -eq 1 ]; then
    printf '%s\n' ready > "$1"
    exec tail -f /dev/null
  fi
  printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"replayed\":true}}"
  ;;
*'"method":"tools/call"'*'"name":"probe"'*)
  printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"ok\":true}}"
  ;;
  esac
done
"#,
    );

    let server = McpServer::connect(stdio_test_config(
        "cancel",
        &script_path,
        vec![
            effect_ready.display().to_string(),
            recovered.display().to_string(),
            generations.display().to_string(),
            effects.display().to_string(),
        ],
        5,
    ))
    .await
    .expect("connect");
    let call_server = server.clone();
    let call =
        zeroclaw_spawn::spawn!(
            async move { call_server.call_tool("side_effect", json!({})).await }
        );
    assert_eq!(read_fifo(&effect_ready).await.trim(), "ready");
    call.abort();
    assert!(
        call.await
            .expect_err("call must be cancelled")
            .is_cancelled()
    );
    assert_eq!(read_fifo(&recovered).await.trim(), "recovered");

    let effects_text = tokio::fs::read_to_string(&effects)
        .await
        .expect("read effects");
    assert_eq!(effects_text.lines().count(), 1, "tool call was replayed");
    let pids = tokio::fs::read_to_string(&generations)
        .await
        .expect("read generation pids");
    assert_eq!(pids.lines().count(), 2, "expected exactly one respawn");
    let first_pid = pids
        .lines()
        .next()
        .expect("first generation pid")
        .parse::<u32>()
        .expect("numeric first generation pid");
    assert!(
        !process_is_alive(first_pid),
        "old child must be reaped before recovery completes"
    );
    let result = server
        .call_tool("probe", json!({}))
        .await
        .expect("fresh child should accept subsequent call");
    assert_eq!(result, json!({"ok":true}));
}

/// A stdio writer already queued on the transport state must re-check a
/// recovery published by the cancelled writer before emitting any bytes.
/// This exercises the real stdio boundary without an HTTP/SSE serial gate.
#[cfg(unix)]
#[tokio::test]
async fn stdio_queued_writer_waits_for_recovery_at_writer_boundary() {
    let temp = tempfile::tempdir().expect("tempdir");
    let script_path = temp.path().join("queued-writer-mcp.sh");
    let requests = temp.path().join("requests.log");
    let generations = temp.path().join("generations.log");
    write_executable_script(
        &script_path,
        br#"#!/bin/sh
printf '%s\n' "$$" >> "$2"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$1"
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
*'"method":"initialize"'*)
  printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"queued-writer\",\"version\":\"1\"}}}"
  ;;
*'"method":"tools/call"'*)
  printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"ok\":true}}"
  ;;
  esac
done
"#,
    );

    let config = stdio_test_config(
        "queued-writer",
        &script_path,
        vec![
            requests.display().to_string(),
            generations.display().to_string(),
        ],
        5,
    );
    let transport = Arc::new(StdioTransport::new(&config).expect("build transport"));
    let hook = Arc::new(StdioWriteTestHook::new());
    hook.pause_next_payload();
    transport.set_write_test_hook(Arc::clone(&hook));
    let shared_transport: Arc<dyn SharedMcpTransportConn> = transport.clone();
    let server = server_with_transport("queued-writer", shared_transport, 5);
    timeout(Duration::from_secs(3), async {
        loop {
            if tokio::fs::read_to_string(&generations)
                .await
                .is_ok_and(|log| log.lines().count() == 1)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("initial stdio child did not start");

    // Pause the first request after its JSON payload has crossed the OS
    // stdin boundary but before newline/flush completes. `state` remains
    // locked and the request outcome is unknown.
    let first_server = server.clone();
    let first =
        zeroclaw_spawn::spawn!(
            async move { first_server.call_tool("side_effect", json!({})).await }
        );
    timeout(Duration::from_secs(3), hook.wait_for_payload_pause())
        .await
        .expect("first stdio write did not reach the post-payload pause");

    // Enter a second call before cancellation and prove it reached the
    // transport, where it is queued on the same stdio state lock.
    let second_server = server.clone();
    let second =
        zeroclaw_spawn::spawn!(async move { second_server.call_tool("probe", json!({})).await });
    timeout(Duration::from_secs(3), hook.wait_for_attempts(2))
        .await
        .expect("second stdio writer did not queue on transport state");

    first.abort();
    assert!(
        first
            .await
            .expect_err("first call must be cancelled")
            .is_cancelled()
    );

    let second_result = timeout(Duration::from_secs(5), second)
        .await
        .expect("second call hung behind recovery")
        .expect("second task failed")
        .expect("second call failed after recovery");
    assert_eq!(second_result, json!({"ok": true}));

    let request_log = tokio::fs::read_to_string(&requests)
        .await
        .expect("read request log");
    let tool_writes = request_log
        .lines()
        .filter(|line| line.contains(r#""method":"tools/call""#))
        .count();
    assert_eq!(
        tool_writes, 1,
        "queued writer reached the ambiguous child before recovery"
    );
    let request_lines = request_log.lines().collect::<Vec<_>>();
    let initialized_index = request_lines
        .iter()
        .position(|line| line.contains(r#""method":"notifications/initialized""#))
        .expect("recovery handshake notification missing");
    let tool_index = request_lines
        .iter()
        .position(|line| line.contains(r#""method":"tools/call""#))
        .expect("recovered tool write missing");
    assert!(
        initialized_index < tool_index,
        "queued tool write occurred before recovery handshake completed"
    );

    let generation_log = tokio::fs::read_to_string(&generations)
        .await
        .expect("read generation log");
    assert_eq!(
        generation_log.lines().count(),
        2,
        "expected exactly one recovery respawn"
    );

    drop(server);
    SharedMcpTransportConn::close(transport.as_ref())
        .await
        .expect("close transport");
}

// ── Server capabilities parsing ──────────────────────────────────────────

#[test]
fn capabilities_parse_from_init_result() {
    let init = serde_json::json!({
        "capabilities": {
            "resources": { "subscribe": true, "listChanged": false },
            "prompts": { "listChanged": true }
        }
    });
    let caps = McpServerCapabilities::from_init_result(&init);
    assert!(caps.supports_resources());
    assert!(caps.supports_prompts());
}

#[test]
fn capabilities_absent_means_unsupported() {
    let init = serde_json::json!({ "capabilities": {} });
    let caps = McpServerCapabilities::from_init_result(&init);
    assert!(!caps.supports_resources());
    assert!(!caps.supports_prompts());
}

#[test]
fn capabilities_missing_object_is_unsupported() {
    let init = serde_json::json!({});
    let caps = McpServerCapabilities::from_init_result(&init);
    assert!(!caps.supports_resources());
    assert!(!caps.supports_prompts());
}

// ── Dual-era adapter (issue #26) ──────────────────────────────────────

async fn mount_tools_list_empty(server: &wiremock::MockServer) {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, ResponseTemplate};
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/list"})))
        .respond_with(|request: &wiremock::Request| {
            let id = serde_json::from_slice::<serde_json::Value>(&request.body)
                .expect("JSON-RPC request")
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]}
            }))
        })
        .mount(server)
        .await;
}

async fn mount_modern_discover(server: &wiremock::MockServer) {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, ResponseTemplate};
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "server/discover"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": {
                "resultType": "complete",
                "supportedVersions": ["2026-07-28"],
                "capabilities": {"tools": {}, "resources": {}}
            }
        })))
        .mount(server)
        .await;
}

fn request_json(request: &wiremock::Request) -> serde_json::Value {
    serde_json::from_slice(&request.body).expect("JSON-RPC request")
}

fn header_str(request: &wiremock::Request, name: &str) -> Option<String> {
    request
        .headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

async fn mount_echo_tool_call(server: &wiremock::MockServer) {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, ResponseTemplate};
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(|request: &wiremock::Request| {
            let id = serde_json::from_slice::<serde_json::Value>(&request.body)
                .expect("JSON-RPC request")
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"ok": true}
            }))
        })
        .mount(server)
        .await;
}

async fn mount_modern_tools_list(server: &wiremock::MockServer) {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, ResponseTemplate};
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/list"})))
        .respond_with(|request: &wiremock::Request| {
            let id = serde_json::from_slice::<serde_json::Value>(&request.body)
                .expect("JSON-RPC request")
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "resultType": "complete",
                    "tools": [{"name": "echo", "inputSchema": {"type": "object"}}]
                }
            }))
        })
        .mount(server)
        .await;
}

async fn mount_modern_echo_tool_call(server: &wiremock::MockServer) {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, ResponseTemplate};
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(|request: &wiremock::Request| {
            let id = serde_json::from_slice::<serde_json::Value>(&request.body)
                .expect("JSON-RPC request")
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"resultType": "complete", "ok": true}
            }))
        })
        .mount(server)
        .await;
}

#[tokio::test]
async fn connect_legacy_server_reads_initialize_protocol_version() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "server/discover"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 0,
            "error": {"code": -32601, "message": "Method not found"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}}
            }
        })))
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
    mount_tools_list_empty(&server).await;
    mount_echo_tool_call(&server).await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("legacy connect");
    assert_eq!(mcp.peer_era().await, PeerEra::Legacy);
    assert_eq!(mcp.peer_protocol_version().await, "2024-11-05");
    let result = mcp
        .call_tool("echo", json!({}))
        .await
        .expect("legacy tools/call");
    assert_eq!(result, json!({"ok": true}));

    let received = server.received_requests().await.expect("requests");
    let initialize = received
        .iter()
        .map(request_json)
        .find(|body| body.get("method").and_then(|m| m.as_str()) == Some("initialize"))
        .expect("legacy initialize");
    assert_eq!(
        initialize
            .get("params")
            .and_then(|p| p.get("protocolVersion"))
            .and_then(|v| v.as_str()),
        Some(MCP_PROTOCOL_VERSION)
    );
    assert!(
        initialize
            .get("params")
            .and_then(|p| p.get("_meta"))
            .is_none(),
        "legacy initialize must not grow a modern _meta object"
    );
    let init_http = received
        .iter()
        .find(|req| request_json(req).get("method").and_then(|m| m.as_str()) == Some("initialize"))
        .expect("initialize POST");
    assert!(
        header_str(init_http, crate::mcp_era::MCP_METHOD_HEADER).is_none(),
        "legacy initialize must not send Mcp-Method"
    );
    let tools_list = received
        .iter()
        .map(request_json)
        .find(|body| body.get("method").and_then(|m| m.as_str()) == Some("tools/list"))
        .expect("legacy tools/list");
    assert!(
        tools_list
            .get("params")
            .and_then(|p| p.get("_meta"))
            .is_none(),
        "legacy tools/list must not send _meta"
    );
}

#[tokio::test]
async fn connect_strict_legacy_server_never_sees_modern_headers() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "server/discover"})))
        .respond_with(|request: &wiremock::Request| {
            if header_str(request, crate::mcp_era::MCP_METHOD_HEADER).is_some()
                || header_str(request, crate::mcp_era::MCP_PROTOCOL_VERSION_HEADER).is_some()
            {
                return ResponseTemplate::new(400)
                    .set_body_string("legacy server rejects Mcp-* headers");
            }
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": 0,
                "error": {"code": -32601, "message": "Method not found"}
            }))
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}}
            }
        })))
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
    mount_tools_list_empty(&server).await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("strict legacy connect");
    assert_eq!(mcp.peer_era().await, PeerEra::Legacy);

    let received = server.received_requests().await.expect("requests");
    assert!(
        received.iter().all(|req| {
            header_str(req, crate::mcp_era::MCP_METHOD_HEADER).is_none()
                && header_str(req, crate::mcp_era::MCP_PROTOCOL_VERSION_HEADER).is_none()
        }),
        "strict legacy peer must never observe modern MCP headers"
    );
    assert_eq!(
        received
            .iter()
            .filter(|req| {
                request_json(req).get("method").and_then(|m| m.as_str()) == Some("server/discover")
            })
            .count(),
        1,
        "legacy probe must not retry with modern headers"
    );
}

#[tokio::test]
async fn connect_strict_modern_server_classifies_after_header_retry() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "server/discover"})))
        .respond_with(|request: &wiremock::Request| {
            if header_str(request, crate::mcp_era::MCP_METHOD_HEADER).as_deref()
                != Some("server/discover")
            {
                return ResponseTemplate::new(400).set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": 0,
                    "error": {"code": -32020, "message": "Header mismatch"}
                }));
            }
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": 0,
                "result": {
                    "resultType": "complete",
                    "supportedVersions": ["2026-07-28"],
                    "capabilities": {"tools": {}}
                }
            }))
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(ResponseTemplate::new(500).set_body_string("initialize must not run"))
        .expect(0)
        .mount(&server)
        .await;
    mount_modern_tools_list(&server).await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("strict modern connect");
    assert_eq!(mcp.peer_era().await, PeerEra::Modern);
    assert_eq!(mcp.peer_protocol_version().await, "2026-07-28");

    let discover_posts = server
        .received_requests()
        .await
        .expect("requests")
        .into_iter()
        .filter(|req| {
            request_json(req).get("method").and_then(|m| m.as_str()) == Some("server/discover")
        })
        .collect::<Vec<_>>();
    assert_eq!(discover_posts.len(), 2, "one legacy probe then one retry");
    assert!(
        header_str(&discover_posts[0], crate::mcp_era::MCP_METHOD_HEADER).is_none(),
        "first discover probe must omit modern headers"
    );
    assert_eq!(
        header_str(&discover_posts[1], crate::mcp_era::MCP_METHOD_HEADER).as_deref(),
        Some("server/discover")
    );
}

#[tokio::test]
async fn connect_discover_handshake_only_versions_stays_legacy() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "server/discover"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": {
                "supportedVersions": ["2025-11-25"],
                "capabilities": {"tools": {}}
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": "2025-11-25",
                "capabilities": {"tools": {}}
            }
        })))
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
    mount_tools_list_empty(&server).await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("handshake-era discover overlap still initializes");
    assert_eq!(mcp.peer_era().await, PeerEra::Legacy);
    assert_eq!(mcp.peer_protocol_version().await, "2025-11-25");
}

#[tokio::test]
async fn connect_modern_server_skips_initialize_and_uses_meta() {
    use wiremock::matchers::{body_partial_json, header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(ResponseTemplate::new(500).set_body_string("initialize must not run"))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({
            "method": "tools/list",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28"
                }
            }
        })))
        .and(header(crate::mcp_era::MCP_METHOD_HEADER, "tools/list"))
        .and(header(
            crate::mcp_era::MCP_PROTOCOL_VERSION_HEADER,
            "2026-07-28",
        ))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "resultType": "complete",
                    "tools": [{"name": "echo", "inputSchema": {"type": "object"}}],
                    "ttlMs": 60_000,
                    "cacheScope": "public"
                }
            }))
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .and(header(crate::mcp_era::MCP_METHOD_HEADER, "tools/call"))
        .and(header(crate::mcp_era::MCP_NAME_HEADER, "echo"))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"resultType": "complete", "ok": true}
            }))
        })
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    assert_eq!(mcp.peer_era().await, PeerEra::Modern);
    assert_eq!(mcp.peer_protocol_version().await, "2026-07-28");
    assert!(mcp.capabilities().await.supports_resources());
    let result = mcp
        .call_tool("echo", json!({}))
        .await
        .expect("modern tools/call");
    assert_eq!(result, json!({"resultType": "complete", "ok": true}));

    let received = server.received_requests().await.expect("requests");
    assert!(
        received.iter().all(|req| {
            request_json(req).get("method").and_then(|m| m.as_str()) != Some("initialize")
        }),
        "modern arm must not send initialize"
    );
    assert!(
        received
            .iter()
            .all(|req| header_str(req, "Mcp-Session-Id").is_none()),
        "modern arm must not send Mcp-Session-Id"
    );
}

#[tokio::test]
async fn connect_unknown_discover_version_is_incompatible() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "server/discover"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": {
                "supportedVersions": ["2027-01-01"]
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(ResponseTemplate::new(500).set_body_string("initialize must not run"))
        .expect(0)
        .mount(&server)
        .await;

    let result = McpServer::connect(http_server_config(server.uri())).await;
    let err = match result {
        Ok(_) => panic!("no common version must fail"),
        Err(err) => err,
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("incompatible"), "got: {msg}");
    assert!(msg.contains("no mutually supported"), "got: {msg}");
    assert!(msg.contains("2027-01-01"), "got: {msg}");
}

#[tokio::test]
async fn connect_modern_discover_omitted_result_type_fails_closed() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "server/discover"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": {
                "supportedVersions": ["2026-07-28"],
                "capabilities": {"tools": {}}
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(ResponseTemplate::new(500).set_body_string("initialize must not run"))
        .expect(0)
        .mount(&server)
        .await;

    let result = McpServer::connect(http_server_config(server.uri())).await;
    let err = match result {
        Ok(_) => panic!("modern discover without resultType must fail"),
        Err(err) => err,
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("incompatible"), "got: {msg}");
    assert!(msg.contains("resultType"), "got: {msg}");
    assert!(
        msg.contains("omitted") || msg.contains("must not guess"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn connect_legacy_discover_omitted_result_type_stays_legacy() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "server/discover"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": {
                "supportedVersions": ["2025-11-25"],
                "capabilities": {"tools": {}}
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": "2025-11-25",
                "capabilities": {"tools": {}}
            }
        })))
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
    mount_tools_list_empty(&server).await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("legacy discover without resultType still initializes");
    assert_eq!(mcp.peer_era().await, PeerEra::Legacy);
    assert_eq!(mcp.peer_protocol_version().await, "2025-11-25");
}

#[tokio::test]
async fn connect_unsupported_protocol_version_error_is_modern() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "server/discover"})))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 0,
            "error": {
                "code": -32022,
                "message": "Unsupported protocol version",
                "data": {"supported": ["2026-07-28"], "requested": "1900-01-01"}
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(ResponseTemplate::new(500).set_body_string("initialize must not run"))
        .expect(0)
        .mount(&server)
        .await;
    mount_modern_tools_list(&server).await;
    mount_modern_echo_tool_call(&server).await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern error connect");
    assert_eq!(mcp.peer_era().await, PeerEra::Modern);
    assert_eq!(mcp.peer_protocol_version().await, "2026-07-28");
}

#[tokio::test]
async fn connect_modern_misclassified_peer_fails_closed_without_legacy_fallback() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}}
            }
        })))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/list"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": -32600, "message": "server not initialized"}
        })))
        .mount(&server)
        .await;

    let result = McpServer::connect(http_server_config(server.uri())).await;
    let err = match result {
        Ok(_) => panic!("modern arm must not fall back to initialize"),
        Err(err) => err,
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("tools/list") || msg.contains("no result") || msg.contains("not initialized"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn modern_list_honours_ttl_ms_cache_scope() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    mount_modern_tools_list(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "resources/list"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "resultType": "complete",
                    "resources": [{"uri": "file:///a", "name": "a"}],
                    "ttlMs": 60_000,
                    "cacheScope": "private"
                }
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    let first = mcp
        .list_resources(None)
        .await
        .expect("first resources/list");
    let second = mcp
        .list_resources(None)
        .await
        .expect("cached resources/list");
    assert_eq!(first.resources.len(), 1);
    assert_eq!(second.resources[0].uri, "file:///a");
}

#[tokio::test]
async fn modern_list_overflow_ttl_is_not_cached() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    mount_modern_tools_list(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "resources/list"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "resultType": "complete",
                    "resources": [{"uri": "file:///a", "name": "a"}],
                    "ttlMs": u64::MAX,
                    "cacheScope": "public"
                }
            }))
        })
        .expect(2)
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    mcp.list_resources(None).await.expect("first list");
    mcp.list_resources(None).await.expect("second list");
}

#[tokio::test]
async fn modern_tools_ttl_zero_refetches_on_tools() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/list"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "resultType": "complete",
                    "tools": [{"name": "echo", "inputSchema": {"type": "object"}}],
                    "ttlMs": 0,
                    "cacheScope": "public"
                }
            }))
        })
        .expect(2)
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    let tools = mcp.tools().await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
}

#[tokio::test]
async fn modern_header_mismatch_fails_closed() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    mount_modern_tools_list(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "error": {
                "code": -32020,
                "message": "Header mismatch"
            }
        })))
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    let err = mcp
        .call_tool("echo", json!({}))
        .await
        .expect_err("header mismatch must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("-32020") || msg.contains("Header mismatch") || msg.contains("header"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn modern_omitted_result_type_on_call_fails_closed() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    mount_modern_tools_list(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"ok": true}
            }))
        })
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    let err = mcp
        .call_tool("echo", json!({}))
        .await
        .expect_err("omitted resultType must not be guessed complete");
    let msg = format!("{err:#}");
    assert!(msg.contains("resultType"), "got: {msg}");
    assert!(
        msg.contains("omitted") || msg.contains("rejected"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn modern_input_required_on_call_is_not_complete_and_does_not_retry() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    mount_modern_tools_list(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "resultType": "input_required",
                    "inputRequests": {
                        "github_login": {
                            "method": "elicitation/create",
                            "params": {"mode": "form", "message": "name"}
                        }
                    },
                    "requestState": "AEAD-protected blob"
                }
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    let err = mcp
        .call_tool("echo", json!({}))
        .await
        .expect_err("input_required is not a completed tool result");
    let pending = err
        .downcast_ref::<McpTaskPending>()
        .expect("minted task handle");
    assert_eq!(pending.method, "tools/call");
    assert!(
        crate::mcp_task::is_our_task_handle(&pending.handle),
        "handle {}",
        pending.handle
    );
    assert_eq!(
        pending.input_required.request_state.as_deref(),
        Some("AEAD-protected blob")
    );
    assert!(
        pending
            .input_required
            .input_requests
            .as_ref()
            .is_some_and(|map| map.contains_key("github_login"))
    );
    let msg = pending.to_string();
    assert!(msg.contains("input_required"), "got: {msg}");
    assert!(msg.contains(crate::mcp_task::TASK_HANDLE_ARG), "got: {msg}");
    assert!(
        !msg.contains("AEAD-protected blob"),
        "requestState must not be model-visible: {msg}"
    );
    assert_eq!(mcp.inner.lock().await.tasks.len(), 1);
    server.verify().await;
}

#[tokio::test]
async fn modern_input_required_continue_retries_original_with_answers() {
    use crate::mcp_task::{INPUT_RESPONSES_FIELD, TASK_HANDLE_ARG};
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    mount_modern_tools_list(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(|request: &wiremock::Request| {
            let body = request_json(request);
            let id = body.get("id").cloned().expect("request id");
            let params = body.get("params").cloned().unwrap_or(json!({}));
            if params.get("inputResponses").is_some() {
                assert_eq!(params["name"], "echo");
                assert_eq!(params["arguments"], json!({"q": 1}));
                assert_eq!(params["requestState"], "AEAD-protected blob");
                assert_eq!(
                    params["inputResponses"],
                    json!({"github_login": {"action": "accept", "content": {"name": "octocat"}}})
                );
                assert!(
                    params.get("_meta").is_some(),
                    "modern retry must keep _meta"
                );
                assert!(
                    params.get(TASK_HANDLE_ARG).is_none(),
                    "client handle must not go on the wire"
                );
                ResponseTemplate::new(200).set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"resultType": "complete", "ok": true}
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "resultType": "input_required",
                        "inputRequests": {
                            "github_login": {
                                "method": "elicitation/create",
                                "params": {"mode": "form", "message": "name"}
                            }
                        },
                        "requestState": "AEAD-protected blob"
                    }
                }))
            }
        })
        .expect(2)
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    let err = mcp
        .call_tool("echo", json!({"q": 1}))
        .await
        .expect_err("pending handle");
    let handle = err
        .downcast_ref::<McpTaskPending>()
        .expect("pending")
        .handle
        .clone();
    let result = mcp
        .call_tool(
            "echo",
            json!({
                TASK_HANDLE_ARG: handle,
                INPUT_RESPONSES_FIELD: {
                    "github_login": {"action": "accept", "content": {"name": "octocat"}}
                }
            }),
        )
        .await
        .expect("continue");
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["ok"], true);
    assert!(mcp.inner.lock().await.tasks.is_empty());
    server.verify().await;
}

#[tokio::test]
async fn modern_unknown_task_handle_fails_closed_without_retry() {
    use crate::mcp_task::TASK_HANDLE_ARG;
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    mount_modern_tools_list(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(ResponseTemplate::new(500).set_body_string("must not retry"))
        .expect(0)
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    let err = mcp
        .call_tool(
            "echo",
            json!({ TASK_HANDLE_ARG: "zc-mrtr-00000000000000000000000000000000" }),
        )
        .await
        .expect_err("unknown handle");
    let msg = format!("{err:#}");
    assert!(msg.contains("unknown"), "got: {msg}");
    server.verify().await;
}

#[tokio::test]
async fn legacy_mcp_task_handle_argument_is_forwarded_verbatim() {
    use crate::mcp_task::TASK_HANDLE_ARG;
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "server/discover"})))
        .respond_with(|request: &wiremock::Request| {
            if header_str(request, crate::mcp_era::MCP_METHOD_HEADER).is_some()
                || header_str(request, crate::mcp_era::MCP_PROTOCOL_VERSION_HEADER).is_some()
            {
                return ResponseTemplate::new(400)
                    .set_body_string("legacy server rejects Mcp-* headers");
            }
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": 0,
                "error": {"code": -32601, "message": "Method not found"}
            }))
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}}
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method": "notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    mount_tools_list_empty(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(|request: &wiremock::Request| {
            if header_str(request, crate::mcp_era::MCP_METHOD_HEADER).is_some()
                || header_str(request, crate::mcp_era::MCP_PROTOCOL_VERSION_HEADER).is_some()
            {
                return ResponseTemplate::new(400)
                    .set_body_string("legacy tools/call must not see modern headers");
            }
            let body = request_json(request);
            let params = body.get("params").cloned().unwrap_or(json!({}));
            assert!(
                params.get("_meta").is_none(),
                "legacy tools/call has no _meta"
            );
            assert_eq!(
                params["arguments"][TASK_HANDLE_ARG],
                "zc-mrtr-00000000000000000000000000000000"
            );
            let id = body.get("id").cloned().expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"ok": true}
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("legacy connect");
    let result = mcp
        .call_tool(
            "echo",
            json!({ TASK_HANDLE_ARG: "zc-mrtr-00000000000000000000000000000000" }),
        )
        .await
        .expect("legacy argument forwarded");
    assert_eq!(result["ok"], true);
    assert!(mcp.inner.lock().await.tasks.is_empty());
    server.verify().await;
}

#[tokio::test]
async fn modern_foreign_mcp_task_handle_argument_is_forwarded_verbatim() {
    use crate::mcp_task::TASK_HANDLE_ARG;
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    mount_modern_tools_list(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(|request: &wiremock::Request| {
            let body = request_json(request);
            let params = body.get("params").cloned().unwrap_or(json!({}));
            assert_eq!(params["arguments"][TASK_HANDLE_ARG], "github_login");
            assert!(
                params.get("inputResponses").is_none(),
                "foreign mcpTaskHandle is not a continuation"
            );
            let id = body.get("id").cloned().expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"resultType": "complete", "ok": true}
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    let result = mcp
        .call_tool("echo", json!({ TASK_HANDLE_ARG: "github_login" }))
        .await
        .expect("foreign argument forwarded");
    assert_eq!(result["ok"], true);
    assert!(mcp.inner.lock().await.tasks.is_empty());
    server.verify().await;
}

#[tokio::test]
async fn modern_expired_task_handle_fails_closed_without_retry() {
    use crate::mcp_task::TASK_HANDLE_ARG;
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    mount_modern_tools_list(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "resultType": "input_required",
                    "requestState": "blob"
                }
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    mcp.inner.lock().await.tasks =
        McpTaskStore::with_limits(8, std::time::Duration::from_millis(1));
    let err = mcp
        .call_tool("echo", json!({}))
        .await
        .expect_err("pending handle");
    let handle = err
        .downcast_ref::<McpTaskPending>()
        .expect("pending")
        .handle
        .clone();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let err = mcp
        .call_tool("echo", json!({ TASK_HANDLE_ARG: handle }))
        .await
        .expect_err("expired handle");
    let msg = format!("{err:#}");
    assert!(msg.contains("expired"), "got: {msg}");
    server.verify().await;
}

#[tokio::test]
async fn legacy_input_required_shaped_result_never_mints_a_handle() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "server/discover"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 0,
            "error": {"code": -32601, "message": "Method not found"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}}
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method": "notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    mount_tools_list_empty(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(|request: &wiremock::Request| {
            let body = request_json(request);
            let params = body.get("params").cloned().unwrap_or(json!({}));
            assert!(
                params.get("inputResponses").is_none(),
                "legacy retry must not grow MRTR fields"
            );
            assert!(
                params.get("_meta").is_none(),
                "legacy tools/call has no _meta"
            );
            let id = body.get("id").cloned().expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "ok": true,
                    "resultType": "input_required",
                    "requestState": "legacy-blob"
                }
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("legacy connect");
    let result = mcp
        .call_tool("echo", json!({}))
        .await
        .expect("legacy treats omitted-era payload as complete");
    assert_eq!(result["ok"], true);
    assert!(mcp.inner.lock().await.tasks.is_empty());
    server.verify().await;
}

#[tokio::test]
async fn modern_oversized_request_state_is_length_bounded_and_not_minted() {
    use crate::mcp_task::MAX_REQUEST_STATE_BYTES;
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let huge = "S".repeat(MAX_REQUEST_STATE_BYTES + 1);
    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    mount_modern_tools_list(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with({
            let huge = huge.clone();
            move |request: &wiremock::Request| {
                let id = request_json(request)
                    .get("id")
                    .cloned()
                    .expect("request id");
                ResponseTemplate::new(200).set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "resultType": "input_required",
                        "requestState": huge
                    }
                }))
            }
        })
        .expect(1)
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    let err = mcp
        .call_tool("echo", json!({}))
        .await
        .expect_err("oversized requestState fails closed");
    let msg = format!("{err:#}");
    assert!(msg.contains("requestState"), "got: {msg}");
    assert!(
        !msg.contains(&huge),
        "opaque blob must not leak into the error"
    );
    assert!(
        err.downcast_ref::<McpTaskPending>().is_none(),
        "oversized state must not mint a handle"
    );
    assert!(mcp.inner.lock().await.tasks.is_empty());
    server.verify().await;
}

#[tokio::test]
async fn modern_prompts_get_input_required_is_typed_error_without_handle() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "server/discover"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": {
                "resultType": "complete",
                "supportedVersions": ["2026-07-28"],
                "capabilities": {"tools": {}, "prompts": {}}
            }
        })))
        .mount(&server)
        .await;
    mount_modern_tools_list(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "prompts/get"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "resultType": "input_required",
                    "requestState": "prompt-blob"
                }
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    let err = mcp
        .get_prompt("p", json!({}))
        .await
        .expect_err("prompts/get input_required is a typed error");
    let typed = err
        .downcast_ref::<McpInputRequiredError>()
        .expect("Stage 3 typed error");
    assert_eq!(typed.method, "prompts/get");
    assert!(err.downcast_ref::<McpTaskPending>().is_none());
    assert!(mcp.inner.lock().await.tasks.is_empty());
    server.verify().await;
}

#[tokio::test]
async fn modern_resources_read_input_required_is_typed_error_without_handle() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    mount_modern_tools_list(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "resources/read"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "resultType": "input_required",
                    "requestState": "resource-blob"
                }
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    let err = mcp
        .read_resource("file:///x")
        .await
        .expect_err("resources/read input_required is a typed error");
    let typed = err
        .downcast_ref::<McpInputRequiredError>()
        .expect("Stage 3 typed error");
    assert_eq!(typed.method, "resources/read");
    assert!(err.downcast_ref::<McpTaskPending>().is_none());
    assert!(mcp.inner.lock().await.tasks.is_empty());
    server.verify().await;
}

#[tokio::test]
async fn modern_omitted_result_type_on_list_fails_connect() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    mount_tools_list_empty(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(ResponseTemplate::new(500).set_body_string("initialize must not run"))
        .expect(0)
        .mount(&server)
        .await;

    let result = McpServer::connect(http_server_config(server.uri())).await;
    let err = match result {
        Ok(_) => panic!("modern tools/list without resultType must fail"),
        Err(err) => err,
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("resultType"), "got: {msg}");
}

#[tokio::test]
async fn modern_malformed_result_type_does_not_panic() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    mount_modern_tools_list(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "resultType": ["complete"],
                    "ok": true
                }
            }))
        })
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    let err = mcp
        .call_tool("echo", json!({}))
        .await
        .expect_err("array resultType must fail closed");
    let msg = format!("{err:#}");
    assert!(msg.contains("resultType"), "got: {msg}");
}

#[tokio::test]
async fn modern_malformed_input_requests_does_not_panic() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    mount_modern_tools_list(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "resultType": "input_required",
                    "inputRequests": u64::MAX,
                    "requestState": {"not": "a string"}
                }
            }))
        })
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    let err = mcp
        .call_tool("echo", json!({}))
        .await
        .expect_err("malformed MRTR must fail closed");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("inputRequests") || msg.contains("resultType"),
        "got: {msg}"
    );
    assert!(
        err.downcast_ref::<McpInputRequiredError>().is_none(),
        "malformed envelope is not a well-formed input_required"
    );
}

#[tokio::test]
async fn modern_input_required_on_list_fails_closed() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/list"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "resultType": "input_required",
                    "requestState": "blob",
                    "tools": []
                }
            }))
        })
        .mount(&server)
        .await;

    let result = McpServer::connect(http_server_config(server.uri())).await;
    let err = match result {
        Ok(_) => panic!("input_required on tools/list must fail"),
        Err(err) => err,
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("input_required") || msg.contains("resultType"),
        "got: {msg}"
    );
}

fn sample_create_task_result(task_id: &str, poll_interval_ms: u64) -> serde_json::Value {
    json!({
        "resultType": "task",
        "taskId": task_id,
        "status": "working",
        "createdAt": "2026-07-28T00:00:00Z",
        "lastUpdatedAt": "2026-07-28T00:00:01Z",
        "ttlMs": 60_000,
        "pollIntervalMs": poll_interval_ms
    })
}

fn sample_task_get_result(
    task_id: &str,
    status: &str,
    extra: serde_json::Value,
) -> serde_json::Value {
    let mut result = json!({
        "resultType": "complete",
        "taskId": task_id,
        "status": status,
        "createdAt": "2026-07-28T00:00:00Z",
        "lastUpdatedAt": "2026-07-28T00:00:02Z",
        "ttlMs": 60_000,
        "pollIntervalMs": 0
    });
    if let (Some(obj), Some(extra)) = (result.as_object_mut(), extra.as_object()) {
        obj.extend(extra.clone());
    }
    result
}

fn client_caps(request: &wiremock::Request) -> Option<serde_json::Value> {
    request_json(request)
        .get("params")
        .and_then(|p| p.get("_meta"))
        .and_then(|m| m.get(crate::mcp_era::META_CLIENT_CAPABILITIES))
        .cloned()
}

#[tokio::test]
async fn modern_task_result_polls_until_complete() {
    use crate::mcp_era::{TASKS_EXTENSION, modern_client_capabilities};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    mount_modern_tools_list(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": sample_create_task_result("srv-task-1", 0)
            }))
        })
        .expect(1)
        .mount(&server)
        .await;
    let gets = Arc::new(AtomicU32::new(0));
    let gets_for_mock = Arc::clone(&gets);
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tasks/get"})))
        .respond_with(move |request: &wiremock::Request| {
            let body = request_json(request);
            assert_eq!(body["params"]["taskId"], "srv-task-1");
            assert_eq!(
                body["params"]["_meta"][crate::mcp_era::META_CLIENT_CAPABILITIES],
                modern_client_capabilities()
            );
            let n = gets_for_mock.fetch_add(1, Ordering::SeqCst);
            let id = body.get("id").cloned().expect("request id");
            let result = if n == 0 {
                sample_task_get_result("srv-task-1", "working", json!({}))
            } else {
                sample_task_get_result(
                    "srv-task-1",
                    "completed",
                    json!({"result": {"resultType": "complete", "ok": true}}),
                )
            };
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result
            }))
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tasks/update"})))
        .respond_with(ResponseTemplate::new(500).set_body_string("must not update"))
        .expect(0)
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    let result = mcp
        .call_tool("echo", json!({"q": 1}))
        .await
        .expect("task polled to completion");
    assert_eq!(result, json!({"resultType": "complete", "ok": true}));
    assert!(mcp.inner.lock().await.tasks.is_empty());
    assert_eq!(gets.load(Ordering::SeqCst), 2);

    let received = server.received_requests().await.expect("requests");
    for req in &received {
        let body = request_json(req);
        let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
        if matches!(method, "tools/list" | "tools/call" | "tasks/get") {
            let caps = client_caps(req).expect("modern _meta");
            assert_eq!(
                caps["extensions"][TASKS_EXTENSION],
                json!({}),
                "modern {method} must advertise tasks"
            );
        }
    }
    server.verify().await;
}

#[tokio::test]
async fn modern_task_poll_limit_fails_closed() {
    use crate::mcp_task::MAX_TASK_POLLS;
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    mount_modern_tools_list(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": sample_create_task_result("srv-task-limit", 0)
            }))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tasks/get"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": sample_task_get_result("srv-task-limit", "working", json!({}))
            }))
        })
        .expect(u64::from(MAX_TASK_POLLS))
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    let err = mcp
        .call_tool("echo", json!({}))
        .await
        .expect_err("poll limit");
    let msg = format!("{err:#}");
    assert!(msg.contains("poll limit"), "got: {msg}");
    assert!(mcp.inner.lock().await.tasks.is_empty());
    server.verify().await;
}

#[tokio::test]
async fn legacy_task_shaped_payload_never_polls() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "server/discover"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 0,
            "error": {"code": -32601, "message": "Method not found"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}}
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method": "notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    mount_tools_list_empty(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(|request: &wiremock::Request| {
            let body = request_json(request);
            assert!(
                body.get("params").and_then(|p| p.get("_meta")).is_none(),
                "legacy tools/call has no _meta"
            );
            let id = body.get("id").cloned().expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": sample_create_task_result("legacy-forged", 0)
            }))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tasks/get"})))
        .respond_with(ResponseTemplate::new(500).set_body_string("must not poll"))
        .expect(0)
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("legacy connect");
    let result = mcp
        .call_tool("echo", json!({}))
        .await
        .expect("legacy treats shaped task payload as complete");
    assert_eq!(result["resultType"], "task");
    assert_eq!(result["taskId"], "legacy-forged");
    assert!(mcp.inner.lock().await.tasks.is_empty());

    let received = server.received_requests().await.expect("requests");
    assert!(
        received.iter().all(|req| {
            request_json(req).get("method").and_then(|m| m.as_str()) != Some("tasks/get")
        }),
        "legacy peer must not be polled"
    );
    for req in &received {
        let body = request_json(req);
        let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
        match method {
            "server/discover" => {
                let caps = client_caps(req).expect("probe _meta");
                assert_eq!(caps, json!({}));
                assert!(
                    caps.get("extensions").is_none(),
                    "era probe must not advertise tasks"
                );
            }
            _ => {
                assert!(
                    client_caps(req).is_none(),
                    "legacy {method} must not carry clientCapabilities"
                );
            }
        }
    }
    server.verify().await;
}

#[tokio::test]
async fn modern_malformed_and_oversized_task_payload_fails_closed() {
    use crate::mcp_era::MAX_TASK_ID_BYTES;
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    mount_modern_tools_list(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(|request: &wiremock::Request| {
            let body = request_json(request);
            let id = body.get("id").cloned().expect("request id");
            let name = body["params"]["name"].as_str().unwrap_or("");
            let result = if name == "huge" {
                let mut payload = sample_create_task_result("x", 0);
                payload["taskId"] = json!("H".repeat(MAX_TASK_ID_BYTES + 1));
                payload
            } else {
                json!({"resultType": "task", "status": "working"})
            };
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result
            }))
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tasks/get"})))
        .respond_with(ResponseTemplate::new(500).set_body_string("must not poll"))
        .expect(0)
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    let malformed = mcp
        .call_tool("echo", json!({}))
        .await
        .expect_err("malformed task");
    let malformed_msg = format!("{malformed:#}");
    assert!(
        malformed_msg.contains("malformed") || malformed_msg.contains("resultType"),
        "got: {malformed_msg}"
    );
    let huge = mcp
        .call_tool("huge", json!({}))
        .await
        .expect_err("oversized taskId");
    let huge_msg = format!("{huge:#}");
    assert!(huge_msg.contains("taskId"), "got: {huge_msg}");
    assert!(
        !huge_msg.contains(&"H".repeat(80)),
        "unbounded taskId leaked: {huge_msg}"
    );
    assert!(
        huge_msg.len() < 1000,
        "error not bounded: {}",
        huge_msg.len()
    );
    assert!(mcp.inner.lock().await.tasks.is_empty());
    server.verify().await;
}

#[tokio::test]
async fn modern_task_input_required_continues_via_tasks_update() {
    use crate::mcp_task::{INPUT_RESPONSES_FIELD, TASK_HANDLE_ARG};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    mount_modern_tools_list(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": sample_create_task_result("srv-task-in", 0)
            }))
        })
        .expect(1)
        .mount(&server)
        .await;
    let gets = Arc::new(AtomicU32::new(0));
    let gets_for_mock = Arc::clone(&gets);
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tasks/get"})))
        .respond_with(move |request: &wiremock::Request| {
            let body = request_json(request);
            let id = body.get("id").cloned().expect("request id");
            let n = gets_for_mock.fetch_add(1, Ordering::SeqCst);
            let result = if n == 0 {
                sample_task_get_result(
                    "srv-task-in",
                    "input_required",
                    json!({
                        "inputRequests": {
                            "github_login": {
                                "method": "elicitation/create",
                                "params": {"mode": "form", "message": "name"}
                            }
                        }
                    }),
                )
            } else {
                sample_task_get_result(
                    "srv-task-in",
                    "completed",
                    json!({"result": {"resultType": "complete", "ok": true}}),
                )
            };
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result
            }))
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tasks/update"})))
        .respond_with(|request: &wiremock::Request| {
            let body = request_json(request);
            let params = body.get("params").cloned().unwrap_or(json!({}));
            assert_eq!(params["taskId"], "srv-task-in");
            assert_eq!(
                params["inputResponses"],
                json!({"github_login": {"action": "accept"}})
            );
            assert!(params.get("_meta").is_some());
            assert!(
                params.get(TASK_HANDLE_ARG).is_none(),
                "client handle must not go on the wire"
            );
            let id = body.get("id").cloned().expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"resultType": "complete"}
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    let err = mcp
        .call_tool("echo", json!({"q": 1}))
        .await
        .expect_err("pending handle");
    let pending = err.downcast_ref::<McpTaskPending>().expect("minted handle");
    assert!(crate::mcp_task::is_our_task_handle(&pending.handle));
    let msg = pending.to_string();
    assert!(!msg.contains("srv-task-in"), "server taskId leaked: {msg}");
    let result = mcp
        .call_tool(
            "echo",
            json!({
                TASK_HANDLE_ARG: pending.handle,
                INPUT_RESPONSES_FIELD: {"github_login": {"action": "accept"}}
            }),
        )
        .await
        .expect("continue via tasks/update");
    assert_eq!(result, json!({"resultType": "complete", "ok": true}));
    assert!(mcp.inner.lock().await.tasks.is_empty());
    server.verify().await;
}

fn http_server_config_with_timeout(uri: String, timeout_secs: u64) -> McpServerConfig {
    McpServerConfig {
        name: "remote".into(),
        transport: McpTransport::Http,
        url: Some(uri),
        tool_timeout_secs: Some(timeout_secs),
        ..Default::default()
    }
}

#[tokio::test]
async fn modern_task_wall_budget_caps_slow_get() {
    use crate::mcp_task::MAX_TASK_POLL_WALL;
    use std::time::Instant as StdInstant;
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    mount_modern_tools_list(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": sample_create_task_result("srv-task-slow", 0)
            }))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tasks/get"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(45))
                .set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": sample_task_get_result(
                        "srv-task-slow",
                        "completed",
                        json!({"result": {"resultType": "complete", "ok": true}})
                    )
                }))
        })
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config_with_timeout(server.uri(), 60))
        .await
        .expect("modern connect");
    let started = StdInstant::now();
    let err = mcp
        .call_tool("echo", json!({}))
        .await
        .expect_err("slow get must not wait the tool timeout");
    let elapsed = started.elapsed();
    let msg = format!("{err:#}");
    assert!(
        elapsed <= MAX_TASK_POLL_WALL + std::time::Duration::from_secs(5),
        "wall budget not enforced: elapsed {elapsed:?} msg {msg}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(60),
        "used full tool timeout: {elapsed:?}"
    );
    assert!(mcp.inner.lock().await.tasks.is_empty());
}

#[tokio::test]
async fn modern_task_completed_nested_input_required_fails_closed() {
    use crate::mcp_task::TASK_HANDLE_ARG;
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    mount_modern_tools_list(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": sample_create_task_result("srv-task-nested-ir", 0)
            }))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tasks/get"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": sample_task_get_result(
                    "srv-task-nested-ir",
                    "completed",
                    json!({
                        "result": {
                            "resultType": "input_required",
                            "requestState": "replay-me"
                        }
                    })
                )
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    let err = mcp
        .call_tool("echo", json!({}))
        .await
        .expect_err("nested input_required");
    assert!(
        err.downcast_ref::<McpTaskPending>().is_none(),
        "must not mint an MRTR handle"
    );
    let msg = format!("{err:#}");
    assert!(
        msg.contains("input_required") || msg.contains("nested"),
        "got: {msg}"
    );
    assert!(mcp.inner.lock().await.tasks.is_empty());
    let continue_err = mcp
        .call_tool(
            "echo",
            json!({ TASK_HANDLE_ARG: "zc-mrtr-00000000000000000000000000000000" }),
        )
        .await
        .expect_err("no minted handle to continue");
    let continue_msg = format!("{continue_err:#}");
    assert!(continue_msg.contains("unknown"), "got: {continue_msg}");
    server.verify().await;
}

#[tokio::test]
async fn modern_task_honours_create_poll_interval_capped() {
    use crate::mcp_task::MAX_POLL_INTERVAL;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Instant as StdInstant;
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let first_get = Arc::new(Mutex::new(None));
    let first_get_for_mock = Arc::clone(&first_get);
    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    mount_modern_tools_list(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": sample_create_task_result("srv-task-interval", 10_000)
            }))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tasks/get"})))
        .respond_with(move |request: &wiremock::Request| {
            let mut slot = first_get_for_mock.lock().expect("lock");
            if slot.is_none() {
                *slot = Some(StdInstant::now());
            }
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": sample_task_get_result(
                    "srv-task-interval",
                    "completed",
                    json!({"result": {"resultType": "complete", "ok": true}})
                )
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    let started = StdInstant::now();
    let result = mcp
        .call_tool("echo", json!({}))
        .await
        .expect("polled after create interval");
    assert_eq!(result["ok"], true);
    let first = first_get.lock().expect("lock").expect("tasks/get ran");
    let waited = first.saturating_duration_since(started);
    assert!(
        waited >= MAX_POLL_INTERVAL - std::time::Duration::from_millis(400),
        "create pollIntervalMs ignored: waited {waited:?}"
    );
    assert!(
        waited <= MAX_POLL_INTERVAL + std::time::Duration::from_secs(1),
        "create pollIntervalMs not capped: waited {waited:?}"
    );
    server.verify().await;
}

#[tokio::test]
async fn modern_task_get_error_redacts_task_id() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    mount_modern_tools_list(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": sample_create_task_result("srv-secret-id", 0)
            }))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tasks/get"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32000,
                    "message": "no such task srv-secret-id"
                }
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    let err = mcp
        .call_tool("echo", json!({}))
        .await
        .expect_err("get error");
    let msg = format!("{err:#}");
    assert!(!msg.contains("srv-secret-id"), "taskId leaked: {msg}");
    assert!(msg.contains("[task-id]"), "got: {msg}");
    assert!(mcp.inner.lock().await.tasks.is_empty());
    server.verify().await;
}

#[tokio::test]
async fn modern_task_cancel_discards_handle() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    mount_modern_tools_list(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": sample_create_task_result("srv-task-cancel", 0)
            }))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tasks/get"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(30))
                .set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": sample_task_get_result(
                        "srv-task-cancel",
                        "working",
                        json!({})
                    )
                }))
        })
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    let call_server = mcp.clone();
    let call =
        zeroclaw_spawn::spawn!(async move { call_server.call_tool("echo", json!({})).await });
    timeout(Duration::from_secs(3), async {
        loop {
            let received = server.received_requests().await.expect("requests");
            if received.iter().any(|req| {
                request_json(req).get("method").and_then(|m| m.as_str()) == Some("tasks/get")
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("tasks/get did not start");
    assert_eq!(mcp.inner.lock().await.tasks.len(), 1);
    call.abort();
    assert!(
        call.await
            .expect_err("call must be cancelled")
            .is_cancelled()
    );
    timeout(Duration::from_secs(2), async {
        loop {
            if mcp.inner.lock().await.tasks.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancelled poll left a zombie handle");
}

#[tokio::test]
async fn modern_task_get_nested_task_result_type_fails_closed() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_modern_discover(&server).await;
    mount_modern_tools_list(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": sample_create_task_result("srv-task-nested", 0)
            }))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tasks/get"})))
        .respond_with(|request: &wiremock::Request| {
            let id = request_json(request)
                .get("id")
                .cloned()
                .expect("request id");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "resultType": "task",
                    "taskId": "srv-task-nested",
                    "status": "working",
                    "createdAt": "2026-07-28T00:00:00Z",
                    "lastUpdatedAt": "2026-07-28T00:00:02Z",
                    "ttlMs": 60_000,
                    "pollIntervalMs": 0
                }
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("modern connect");
    let err = mcp
        .call_tool("echo", json!({}))
        .await
        .expect_err("nested task on get");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("resultType") || msg.contains("task"),
        "got: {msg}"
    );
    assert!(mcp.inner.lock().await.tasks.is_empty());
    server.verify().await;
}

#[tokio::test]
async fn connect_unknown_initialize_version_snaps_to_nearest_legacy() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": "2023-01-01",
                "capabilities": {"tools": {}}
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method": "notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    mount_tools_list_empty(&server).await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("unknown legacy connect");
    assert_eq!(mcp.peer_era().await, PeerEra::Legacy);
    assert_eq!(mcp.peer_protocol_version().await, "2024-11-05");
    let peer = mcp.inner.lock().await.peer.clone();
    assert_eq!(peer.advertised, "2023-01-01");
    assert_eq!(peer.quality, VersionQuality::UnknownRevision);
}

#[tokio::test]
async fn connect_malformed_initialize_version_falls_back_conservatively() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": 42,
                "capabilities": {"tools": {}}
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method": "notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    mount_tools_list_empty(&server).await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("malformed initialize still connects");
    assert_eq!(mcp.peer_era().await, PeerEra::Legacy);
    assert_eq!(mcp.peer_protocol_version().await, "2024-11-05");
    let peer = mcp.inner.lock().await.peer.clone();
    assert_eq!(peer.advertised, "42");
    assert_eq!(peer.quality, VersionQuality::Malformed);
}

#[tokio::test]
async fn connect_missing_initialize_version_is_malformed() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "capabilities": {"tools": {}}
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method": "notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    mount_tools_list_empty(&server).await;

    let mcp = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("missing protocolVersion still connects");
    let peer = mcp.inner.lock().await.peer.clone();
    assert_eq!(peer.advertised, "<missing>");
    assert_eq!(peer.quality, VersionQuality::Malformed);
    assert_eq!(peer.version, "2024-11-05");
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
async fn call_tool_recovers_stale_session_without_replaying_tool() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    // initialize → 200 + session header. Hit twice: initial connect plus the
    // reconnect that follows the stale-session error.
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

    // tools/call → 404 (stale session). Even though the response indicates
    // a stale session, the request crossed the write boundary, so the
    // client recovers the connection but does not replay the tool.
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(ResponseTemplate::new(404))
        .up_to_n_times(1)
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;

    let srv = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("connect");
    let error = srv
        .call_tool("echo", json!({}))
        .await
        .expect_err("outcome-unknown tool call must not be replayed");
    assert!(
        error.to_string().contains("request was not replayed"),
        "got: {error:#}"
    );
    timeout(Duration::from_secs(5), async {
        loop {
            if *srv.epoch_gate.read().await == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("stale-session recovery did not complete");
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

// ── dispatch_method: generic JSON-RPC dispatch ────────────────────────

#[tokio::test]
async fn dispatch_method_returns_raw_result() {
    let server = server_returning(serde_json::json!({ "ok": 1 }));
    let out = server
        .dispatch_method("resources/list", serde_json::json!({}))
        .await
        .expect("dispatch should succeed");
    assert_eq!(out, serde_json::json!({ "ok": 1 }));
}

#[tokio::test]
async fn dispatch_method_surfaces_is_error_envelope_scrubbed() {
    // An `isError: true` envelope on a resources/prompts result must map to
    // Err (not be returned as success), with the server-controlled detail
    // secret-scrubbed and length-bounded — same contract as `call_tool`.
    let server = server_returning(serde_json::json!({
        "isError": true,
        "content": [{ "type": "text", "text": "boom using sk-supersecrettoken12345abcdef" }],
    }));
    let err = server
        .dispatch_method("resources/read", serde_json::json!({}))
        .await
        .expect_err("isError:true must map to Err");
    let msg = err.to_string();
    assert!(msg.contains("returned isError"), "got: {msg}");
    assert!(msg.contains("[REDACTED]"), "secret not scrubbed: {msg}");
    assert!(
        !msg.contains("supersecrettoken"),
        "raw secret leaked: {msg}"
    );
}

#[tokio::test]
async fn dispatch_method_surfaces_jsonrpc_error() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Mcp-Session-Id", "s")
                .set_body_json(
                    json!({"jsonrpc":"2.0","id":1,"result":{"capabilities":{"resources":{}}}}),
                ),
        )
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
            "jsonrpc":"2.0","id":2,"result":{"tools":[]}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "resources/list"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"nope"}
        })))
        .mount(&server)
        .await;

    let srv = McpServer::connect(http_server_config(server.uri()))
        .await
        .expect("connect");
    let err = srv
        .dispatch_method("resources/list", json!({}))
        .await
        .expect_err("jsonrpc error should surface");
    assert!(err.to_string().contains("nope"), "got: {err}");
}

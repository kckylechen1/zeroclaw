#[cfg(test)]
use super::*;

#[tokio::test]
async fn stdio_routes_only_exact_numeric_id_and_preserves_other_waiters() {
    let pending: PendingMap = Arc::new(ParkingMutex::new(HashMap::new()));
    let (sender, receiver) = oneshot::channel();
    register_pending(&pending, 7, 5, sender).expect("register waiter");

    assert!(!deliver_stdio_response(
        &pending,
        7,
        JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(6)),
            result: Some(serde_json::json!("wrong")),
            error: None,
        }
    ));
    assert!(pending.lock().contains_key(&(7, 5)));
    assert!(!deliver_stdio_response(
        &pending,
        7,
        JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!("5")),
            result: Some(serde_json::json!("wrong shape")),
            error: None,
        }
    ));
    assert!(pending.lock().contains_key(&(7, 5)));

    assert!(deliver_stdio_response(
        &pending,
        7,
        JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(5)),
            result: Some(serde_json::json!("correct")),
            error: None,
        }
    ));
    let response = receiver.await.expect("exact-id response");
    assert_eq!(response.result, Some(serde_json::json!("correct")));
}

#[tokio::test]
async fn duplicate_stdio_id_does_not_evict_original_waiter() {
    let pending: PendingMap = Arc::new(ParkingMutex::new(HashMap::new()));
    let (original_sender, original_receiver) = oneshot::channel();
    register_pending(&pending, 3, 9, original_sender).expect("register original");
    let (duplicate_sender, duplicate_receiver) = oneshot::channel();
    let error = register_pending(&pending, 3, 9, duplicate_sender)
        .expect_err("duplicate id must be rejected");
    assert!(error.to_string().contains("duplicate in-flight"));
    drop(duplicate_receiver);

    assert!(deliver_stdio_response(
        &pending,
        3,
        JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(9)),
            result: Some(serde_json::json!("original")),
            error: None,
        }
    ));
    assert_eq!(
        original_receiver
            .await
            .expect("original waiter must remain registered")
            .result,
        Some(serde_json::json!("original"))
    );
}

#[tokio::test]
async fn old_stdio_reader_finalizer_cannot_drain_new_generation() {
    let pending: PendingMap = Arc::new(ParkingMutex::new(HashMap::new()));
    let (old_sender, old_receiver) = oneshot::channel();
    let (new_sender, new_receiver) = oneshot::channel();
    register_pending(&pending, 1, 3, old_sender).expect("old waiter");
    register_pending(&pending, 2, 3, new_sender).expect("new waiter");
    let alive = AtomicBool::new(true);
    let active_generation = AtomicU64::new(2);

    finish_stdio_generation(&pending, 1, &alive, &active_generation);

    assert!(alive.load(Ordering::Acquire));
    assert!(pending.lock().contains_key(&(1, 3)));
    assert!(pending.lock().contains_key(&(2, 3)));
    drop(old_receiver);
    drop(new_receiver);
}

#[tokio::test]
async fn late_stdio_response_from_old_generation_cannot_reach_new_waiter() {
    let pending: PendingMap = Arc::new(ParkingMutex::new(HashMap::new()));
    let (new_sender, new_receiver) = oneshot::channel();
    register_pending(&pending, 2, 3, new_sender).expect("new waiter");

    assert!(!deliver_stdio_response(
        &pending,
        1,
        JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(3)),
            result: Some(serde_json::json!("late")),
            error: None,
        }
    ));
    assert!(pending.lock().contains_key(&(2, 3)));
    assert!(deliver_stdio_response(
        &pending,
        2,
        JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(3)),
            result: Some(serde_json::json!("current")),
            error: None,
        }
    ));
    assert_eq!(
        new_receiver
            .await
            .expect("new waiter must receive response")
            .result,
        Some(serde_json::json!("current"))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn cancelled_direct_stdio_request_removes_pending_waiter() {
    let config = McpServerConfig {
        name: "stdio-cancel".into(),
        transport: McpTransport::Stdio,
        command: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            "while IFS= read -r line; do exec tail -f /dev/null; done".into(),
        ],
        ..Default::default()
    };
    let transport = Arc::new(StdioTransport::new(&config).expect("build transport"));
    let task_transport = Arc::clone(&transport);
    let request = JsonRpcRequest::new(7, "tools/call", serde_json::json!({}));
    let lifecycle = McpRequestLifecycle::uncoordinated(0);
    let call = zeroclaw_spawn::spawn!(async move {
        SharedMcpTransportConn::send_and_recv(task_transport.as_ref(), &request, &lifecycle).await
    });
    timeout(Duration::from_secs(2), async {
        while transport.pending.lock().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("request did not register a waiter");

    call.abort();
    assert!(
        call.await
            .expect_err("call must be cancelled")
            .is_cancelled()
    );
    assert!(transport.pending.lock().is_empty());
    SharedMcpTransportConn::close(transport.as_ref())
        .await
        .expect("close transport");
}

/// `close()` must deliver stdin EOF and let a well-behaved server exit on
/// its own before escalating to a signal. The stub reads stdin to EOF, then
/// writes a marker and exits 0; a force-kill that landed before EOF would
/// SIGKILL the shell before the marker write, so the marker's presence
/// proves the graceful path ran.
#[cfg(unix)]
#[tokio::test]
async fn stdio_close_delivers_eof_before_killing_the_server() {
    let marker =
        std::env::temp_dir().join(format!("zeroclaw_stdio_eof_marker_{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);

    let config = McpServerConfig {
        name: "stdio-graceful-eof".into(),
        transport: McpTransport::Stdio,
        command: "/bin/sh".into(),
        // The marker path is passed positionally (`$1`), never interpolated
        // into the script, so it is safe under a TMPDIR with spaces.
        args: vec![
            "-c".into(),
            "cat >/dev/null; printf done > \"$1\"".into(),
            "sh".into(),
            marker.to_string_lossy().into_owned(),
        ],
        ..Default::default()
    };
    let transport = StdioTransport::new(&config).expect("build transport");

    SharedMcpTransportConn::close(&transport)
        .await
        .expect("close transport");

    assert!(
        marker.exists(),
        "close() did not deliver stdin EOF before killing: the server was \
         signalled before it could observe EOF and write its marker"
    );
    let _ = std::fs::remove_file(&marker);
}

/// When the direct child exits but a descendant keeps the inherited stdout
/// pipe open (so the reader never sees EOF), `health_check` must still
/// report the transport unhealthy. It relies on the nonblocking
/// direct-child-exit watcher, not on stdout EOF.
#[cfg(unix)]
#[tokio::test]
async fn health_check_detects_direct_child_exit_with_inherited_stdout_open() {
    let config = McpServerConfig {
        name: "stdio-orphan-stdout".into(),
        transport: McpTransport::Stdio,
        command: "/bin/sh".into(),
        // Background a bounded process that inherits stdout, then the
        // direct shell child exits immediately. stdout stays open long
        // enough to prove the direct-child watcher wins over EOF.
        // Keep private transport stdout open without retaining runner stderr.
        args: vec!["-c".into(), "sleep 2 2>/dev/null & exit 0".into()],
        ..Default::default()
    };
    let transport = StdioTransport::new(&config).expect("build transport");

    // Once the direct child exits, the watcher must flip health to false
    // even though stdout (held by the descendant) never reached EOF.
    let became_unhealthy = timeout(Duration::from_secs(5), async {
        loop {
            if !SharedMcpTransportConn::health_check(&transport) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        became_unhealthy.is_ok(),
        "health_check kept reporting healthy after the direct child exited \
         (inherited stdout stayed open, so EOF alone is insufficient)"
    );

    SharedMcpTransportConn::close(&transport)
        .await
        .expect("close transport");
}

#[tokio::test]
async fn sse_pre_write_cancellation_does_not_leak_pending_waiter() {
    use std::future::Future;
    use std::task::{Context as TaskContext, Poll};

    let config = McpServerConfig {
        name: "sse-cancel".into(),
        transport: McpTransport::Sse,
        url: Some("http://localhost:1/sse".into()),
        ..Default::default()
    };
    let transport = SseTransport::new(&config).expect("build transport");
    let reader = zeroclaw_spawn::spawn!(std::future::pending::<()>());
    {
        let mut conn = transport.conn.lock().await;
        conn.stream_state = SseStreamState::Connected;
        conn.reader_task = Some(reader);
    }
    {
        let mut shared = transport.shared.lock().await;
        shared.message_url = Some("http://localhost:1/messages".into());
        shared.message_url_from_endpoint = true;
    }

    let epoch_gate = Arc::new(RwLock::new(0));
    let epoch_writer = epoch_gate.write().await;
    let lifecycle = McpRequestLifecycle::coordinated(
        Arc::clone(&epoch_gate),
        None,
        &PeerProtocol::legacy_default(),
    );
    let request = JsonRpcRequest::new(7, "tools/call", serde_json::json!({}));
    let mut send = Box::pin(SharedMcpTransportConn::send_and_recv(
        &transport, &request, &lifecycle,
    ));
    let waker = futures_util::task::noop_waker();
    let mut context = TaskContext::from_waker(&waker);
    assert!(matches!(send.as_mut().poll(&mut context), Poll::Pending));
    drop(send);

    assert!(lifecycle.outcome_unknown_epoch().is_none());
    assert!(transport.pending.lock().is_empty());
    drop(epoch_writer);
    SharedMcpTransportConn::close(&transport)
        .await
        .expect("close transport");
}

#[test]
fn test_transport_default_is_stdio() {
    let config = McpServerConfig::default();
    assert_eq!(config.transport, McpTransport::Stdio);
}

#[test]
fn test_http_transport_requires_url() {
    let config = McpServerConfig {
        name: "test".into(),
        transport: McpTransport::Http,
        ..Default::default()
    };
    assert!(HttpTransport::new(&config).is_err());
}

#[test]
fn test_sse_transport_requires_url() {
    let config = McpServerConfig {
        name: "test".into(),
        transport: McpTransport::Sse,
        ..Default::default()
    };
    assert!(SseTransport::new(&config).is_err());
}

#[test]
fn http_request_timeout_defaults_non_tool_requests_to_legacy_value() {
    let request = JsonRpcRequest::new(1, "initialize", serde_json::json!({}));
    assert_eq!(
        http_request_timeout_secs(&request, None),
        Some(DEFAULT_HTTP_REQUEST_TIMEOUT_SECS)
    );
}

#[test]
fn http_request_timeout_does_not_shorten_non_tool_requests_from_tool_config() {
    let request = JsonRpcRequest::new(1, "tools/list", serde_json::json!({}));
    assert_eq!(
        http_request_timeout_secs(&request, Some(5)),
        Some(DEFAULT_HTTP_REQUEST_TIMEOUT_SECS)
    );
}

#[test]
fn http_request_timeout_honors_configured_tool_call_timeout_above_legacy_value() {
    let request = JsonRpcRequest::new(1, TOOLS_CALL_METHOD, serde_json::json!({}));
    assert_eq!(
        http_request_timeout_secs(&request, Some(DEFAULT_HTTP_REQUEST_TIMEOUT_SECS + 60)),
        Some(DEFAULT_HTTP_REQUEST_TIMEOUT_SECS + 60)
    );
}

#[test]
fn http_request_timeout_leaves_default_tool_call_budget_to_client_wrapper() {
    let request = JsonRpcRequest::new(1, TOOLS_CALL_METHOD, serde_json::json!({}));
    assert_eq!(http_request_timeout_secs(&request, None), None);
}

#[test]
fn http_sse_read_timeout_defaults_non_tool_requests_to_recv_timeout() {
    let request = JsonRpcRequest::new(1, "initialize", serde_json::json!({}));
    assert_eq!(
        http_sse_read_timeout_secs(&request, None),
        Some(RECV_TIMEOUT_SECS)
    );
}

#[test]
fn http_sse_read_timeout_honors_configured_tool_call_timeout() {
    let request = JsonRpcRequest::new(1, TOOLS_CALL_METHOD, serde_json::json!({}));
    assert_eq!(
        http_sse_read_timeout_secs(&request, Some(DEFAULT_HTTP_REQUEST_TIMEOUT_SECS + 60)),
        Some(DEFAULT_HTTP_REQUEST_TIMEOUT_SECS + 60)
    );
}

#[test]
fn http_sse_read_timeout_leaves_default_tool_call_budget_to_client_wrapper() {
    let request = JsonRpcRequest::new(1, TOOLS_CALL_METHOD, serde_json::json!({}));
    assert_eq!(http_sse_read_timeout_secs(&request, None), None);
}

#[test]
fn http_transport_stores_configured_tool_timeout() {
    let config = McpServerConfig {
        name: "test-http".into(),
        transport: McpTransport::Http,
        url: Some("http://localhost/mcp".into()),
        tool_timeout_secs: Some(DEFAULT_HTTP_REQUEST_TIMEOUT_SECS + 60),
        ..Default::default()
    };
    let transport = HttpTransport::new(&config).expect("build transport");
    assert_eq!(
        transport.tool_timeout_secs,
        Some(DEFAULT_HTTP_REQUEST_TIMEOUT_SECS + 60)
    );
}

#[test]
fn sse_transport_stores_configured_tool_timeout() {
    let config = McpServerConfig {
        name: "test-sse".into(),
        transport: McpTransport::Sse,
        url: Some("http://localhost/sse".into()),
        tool_timeout_secs: Some(DEFAULT_HTTP_REQUEST_TIMEOUT_SECS + 60),
        ..Default::default()
    };
    let transport = SseTransport::new(&config).expect("build transport");
    assert_eq!(
        transport.tool_timeout_secs,
        Some(DEFAULT_HTTP_REQUEST_TIMEOUT_SECS + 60)
    );
}

#[test]
fn test_extract_json_from_sse_data_no_space() {
    let input = "data:{\"jsonrpc\":\"2.0\",\"result\":{}}\n\n";
    let extracted = extract_json_from_sse_text(input);
    let _: JsonRpcResponse = serde_json::from_str(extracted.as_ref()).unwrap();
}

#[test]
fn test_extract_json_from_sse_with_event_and_id() {
    let input = "id: 1\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"result\":{}}\n\n";
    let extracted = extract_json_from_sse_text(input);
    let _: JsonRpcResponse = serde_json::from_str(extracted.as_ref()).unwrap();
}

#[test]
fn test_extract_json_from_sse_multiline_data() {
    let input = "event: message\ndata: {\ndata:   \"jsonrpc\": \"2.0\",\ndata:   \"result\": {}\ndata: }\n\n";
    let extracted = extract_json_from_sse_text(input);
    let _: JsonRpcResponse = serde_json::from_str(extracted.as_ref()).unwrap();
}

#[test]
fn test_extract_json_from_sse_skips_bom_and_leading_whitespace() {
    let input = "\u{feff}\n\n  data: {\"jsonrpc\":\"2.0\",\"result\":{}}\n\n";
    let extracted = extract_json_from_sse_text(input);
    let _: JsonRpcResponse = serde_json::from_str(extracted.as_ref()).unwrap();
}

#[test]
fn test_extract_json_from_sse_uses_last_event_with_data() {
    let input =
        ": keep-alive\n\nid: 1\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"result\":{}}\n\n";
    let extracted = extract_json_from_sse_text(input);
    let _: JsonRpcResponse = serde_json::from_str(extracted.as_ref()).unwrap();
}

#[test]
fn test_parse_jsonrpc_response_text_handles_plain_json() {
    let parsed = parse_jsonrpc_response_text("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}")
        .expect("plain JSON response should parse");
    assert_eq!(parsed.id, Some(serde_json::json!(1)));
    assert!(parsed.error.is_none());
}

#[test]
fn test_parse_jsonrpc_response_text_handles_sse_framed_json() {
    let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"ok\":true}}\n\n";
    let parsed = parse_jsonrpc_response_text(sse).expect("SSE-framed JSON response should parse");
    assert_eq!(parsed.id, Some(serde_json::json!(2)));
    assert_eq!(
        parsed
            .result
            .as_ref()
            .and_then(|v| v.get("ok"))
            .and_then(|v| v.as_bool()),
        Some(true)
    );
}

#[test]
fn test_parse_jsonrpc_response_text_rejects_empty_payload() {
    assert!(parse_jsonrpc_response_text(" \n\t ").is_err());
}

#[test]
fn http_transport_updates_session_id_from_response_headers() {
    let config = McpServerConfig {
        name: "test-http".into(),
        transport: McpTransport::Http,
        url: Some("http://localhost/mcp".into()),
        ..Default::default()
    };
    let transport = HttpTransport::new(&config).expect("build transport");

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::HeaderName::from_static("mcp-session-id"),
        reqwest::header::HeaderValue::from_static("session-abc"),
    );
    transport.update_session_id_from_headers(&headers);
    assert_eq!(transport.session_id.lock().as_deref(), Some("session-abc"));
}

#[test]
fn http_transport_injects_session_id_header_when_available() {
    let config = McpServerConfig {
        name: "test-http".into(),
        transport: McpTransport::Http,
        url: Some("http://localhost/mcp".into()),
        ..Default::default()
    };
    let transport = HttpTransport::new(&config).expect("build transport");
    *transport.session_id.lock() = Some("session-xyz".to_string());

    let req = transport
        .apply_session_header(reqwest::Client::new().post("http://localhost/mcp"))
        .build()
        .expect("build request");
    assert_eq!(
        req.headers()
            .get(MCP_SESSION_ID_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some("session-xyz")
    );
}

// ── derive_message_url tests ──────────────────────────────────────────────

#[test]
fn derive_message_url_replaces_sse_segment_with_messages() {
    let url = derive_message_url("http://localhost:3000/mcp/sse", "messages");
    assert_eq!(url, Some("http://localhost:3000/mcp/messages".to_string()));
}

#[test]
fn derive_message_url_appends_when_no_sse_segment() {
    let url = derive_message_url("http://localhost:3000/mcp", "messages");
    assert_eq!(url, Some("http://localhost:3000/mcp/messages".to_string()));
}

#[test]
fn derive_message_url_returns_none_for_invalid_url() {
    let url = derive_message_url("not-a-url", "messages");
    assert!(url.is_none());
}

#[test]
fn derive_message_url_message_path_variant() {
    let url = derive_message_url("http://localhost:3000/mcp/sse", "message");
    assert_eq!(url, Some("http://localhost:3000/mcp/message".to_string()));
}

// ── parse_endpoint_from_data tests ───────────────────────────────────────

#[test]
fn parse_endpoint_absolute_http_url_returned_as_is() {
    let result = parse_endpoint_from_data("http://base/sse", "http://other/messages");
    assert_eq!(result, Some("http://other/messages".to_string()));
}

#[test]
fn parse_endpoint_absolute_https_url_returned_as_is() {
    let result = parse_endpoint_from_data("https://base/sse", "https://other/messages");
    assert_eq!(result, Some("https://other/messages".to_string()));
}

#[test]
fn parse_endpoint_relative_path_resolved_against_base() {
    let result = parse_endpoint_from_data("http://localhost:3000/sse", "/messages");
    assert_eq!(result, Some("http://localhost:3000/messages".to_string()));
}

#[test]
fn parse_endpoint_json_object_with_endpoint_key() {
    let json_data = r#"{"endpoint":"/messages"}"#;
    let result = parse_endpoint_from_data("http://localhost:3000/sse", json_data);
    assert_eq!(result, Some("http://localhost:3000/messages".to_string()));
}

// ── looks_like_sse_text tests ─────────────────────────────────────────────

#[test]
fn looks_like_sse_text_detects_data_prefix() {
    assert!(looks_like_sse_text("data:{\"jsonrpc\":\"2.0\"}"));
}

#[test]
fn looks_like_sse_text_detects_event_prefix() {
    assert!(looks_like_sse_text("event: message\ndata: {}"));
}

#[test]
fn looks_like_sse_text_detects_embedded_data_line() {
    assert!(looks_like_sse_text("id: 1\ndata:{\"x\":1}"));
}

#[test]
fn looks_like_sse_text_plain_json_is_not_sse() {
    assert!(!looks_like_sse_text(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}"
    ));
}

// ── extract_json_from_sse_text edge cases ─────────────────────────────────

#[test]
fn extract_json_skips_comment_lines() {
    let input = ": keep-alive\ndata: {\"jsonrpc\":\"2.0\",\"result\":{}}\n\n";
    let extracted = extract_json_from_sse_text(input);
    let v: serde_json::Value = serde_json::from_str(extracted.as_ref()).unwrap();
    assert_eq!(v["jsonrpc"], "2.0");
}

#[test]
fn extract_json_empty_input_returns_empty_trimmed() {
    let result = extract_json_from_sse_text("   ");
    assert!(result.as_ref().trim().is_empty());
}

#[test]
fn extract_json_plain_json_returned_unchanged() {
    let input = "{\"jsonrpc\":\"2.0\",\"result\":{}}";
    let extracted = extract_json_from_sse_text(input);
    // No SSE framing, extracted as-is (trimmed)
    assert_eq!(extracted.as_ref(), input);
}

// ── parse_jsonrpc_response_text edge cases ────────────────────────────────

#[test]
fn parse_jsonrpc_response_rejects_whitespace_only() {
    assert!(parse_jsonrpc_response_text("   \n\t  ").is_err());
}

#[test]
fn parse_jsonrpc_response_with_error_result() {
    let json = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"not found"}}"#;
    let resp = parse_jsonrpc_response_text(json).unwrap();
    assert!(resp.error.is_some());
    assert_eq!(resp.error.unwrap().code, -32601);
}

// ── create_transport factory ──────────────────────────────────────────────

#[test]
fn create_transport_stdio_fails_without_valid_command() {
    // Spawning a non-existent binary should fail
    let config = McpServerConfig {
        name: "test-stdio".into(),
        transport: McpTransport::Stdio,
        command: "/usr/bin/zeroclaw_nonexistent_binary_abc123".into(),
        ..Default::default()
    };
    let result = create_transport(&config);
    assert!(result.is_err());
}

#[test]
fn create_transport_http_without_url_fails() {
    let config = McpServerConfig {
        name: "test-http".into(),
        transport: McpTransport::Http,
        ..Default::default()
    };
    assert!(create_transport(&config).is_err());
}

#[test]
fn create_transport_sse_without_url_fails() {
    let config = McpServerConfig {
        name: "test-sse".into(),
        transport: McpTransport::Sse,
        ..Default::default()
    };
    assert!(create_transport(&config).is_err());
}

#[test]
fn create_transport_http_with_url_succeeds() {
    let config = McpServerConfig {
        name: "test-http".into(),
        transport: McpTransport::Http,
        url: Some("http://localhost:9999/mcp".into()),
        ..Default::default()
    };
    // Build should succeed even if server isn't running
    assert!(create_transport(&config).is_ok());
}

#[test]
fn create_transport_sse_with_url_succeeds() {
    let config = McpServerConfig {
        name: "test-sse".into(),
        transport: McpTransport::Sse,
        url: Some("http://localhost:9999/sse".into()),
        ..Default::default()
    };
    assert!(create_transport(&config).is_ok());
}

// ── HTTP session id whitespace handling ───────────────────────────────────

#[test]
fn http_transport_ignores_empty_session_id_header() {
    let config = McpServerConfig {
        name: "test-http".into(),
        transport: McpTransport::Http,
        url: Some("http://localhost/mcp".into()),
        ..Default::default()
    };
    let transport = HttpTransport::new(&config).expect("build transport");
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::HeaderName::from_static("mcp-session-id"),
        reqwest::header::HeaderValue::from_static("   "),
    );
    transport.update_session_id_from_headers(&headers);
    // Whitespace-only session id should not be stored
    assert!(transport.session_id.lock().is_none());
}

#[test]
fn http_transport_no_session_header_leaves_none() {
    let config = McpServerConfig {
        name: "test-http".into(),
        transport: McpTransport::Http,
        url: Some("http://localhost/mcp".into()),
        ..Default::default()
    };
    let transport = HttpTransport::new(&config).expect("build transport");
    assert!(transport.session_id.lock().is_none());
}

#[test]
fn http_transport_apply_session_header_noop_when_no_session() {
    let config = McpServerConfig {
        name: "test-http".into(),
        transport: McpTransport::Http,
        url: Some("http://localhost/mcp".into()),
        ..Default::default()
    };
    let transport = HttpTransport::new(&config).expect("build transport");
    let req = transport
        .apply_session_header(reqwest::Client::new().post("http://localhost/mcp"))
        .build()
        .expect("build request");
    assert!(req.headers().get(MCP_SESSION_ID_HEADER).is_none());
}

#[tokio::test]
async fn http_transport_reset_clears_session_id() {
    let config = McpServerConfig {
        name: "test-http".into(),
        transport: McpTransport::Http,
        url: Some("http://localhost/mcp".into()),
        ..Default::default()
    };
    let transport = HttpTransport::new(&config).expect("build transport");
    *transport.session_id.lock() = Some("stale-session".into());
    SharedMcpTransportConn::reset(&transport)
        .await
        .expect("reset");
    assert!(transport.session_id.lock().is_none());
}

#[tokio::test]
async fn http_transport_maps_404_to_stale_session() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let config = McpServerConfig {
        name: "test-http".into(),
        transport: McpTransport::Http,
        url: Some(server.uri()),
        ..Default::default()
    };
    let transport = HttpTransport::new(&config).expect("build transport");
    // A 404 only signals a stale session when the request carried a session id.
    *transport.session_id.lock() = Some("sess-1".into());
    let req = JsonRpcRequest::new(1, "tools/call", serde_json::json!({}));
    let lifecycle = McpRequestLifecycle::uncoordinated(0);
    let err = transport
        .send_and_recv(&req, &lifecycle)
        .await
        .expect_err("404 should error");
    match err.downcast_ref::<McpTransportError>() {
        Some(McpTransportError::StaleSession { status }) => assert_eq!(*status, 404),
        other => panic!("expected StaleSession, got {other:?}"),
    }
}

#[tokio::test]
async fn http_transport_404_without_session_is_plain_error() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let config = McpServerConfig {
        name: "test-http".into(),
        transport: McpTransport::Http,
        url: Some(server.uri()),
        ..Default::default()
    };
    // No session id was ever issued (stateless server, or a misconfigured url):
    // a 404 here is a missing endpoint, not a stale session — it must NOT map to
    // StaleSession (which would make `call_tool` burn a wasted reconnect).
    let transport = HttpTransport::new(&config).expect("build transport");
    assert!(transport.session_id.lock().is_none());
    let req = JsonRpcRequest::new(1, "tools/call", serde_json::json!({}));
    let lifecycle = McpRequestLifecycle::uncoordinated(0);
    let err = transport
        .send_and_recv(&req, &lifecycle)
        .await
        .expect_err("404 should error");
    assert!(
        !matches!(
            err.downcast_ref::<McpTransportError>(),
            Some(McpTransportError::StaleSession { .. })
        ),
        "sessionless 404 must not be classified as StaleSession, got: {err:?}"
    );
    assert!(
        err.to_string().contains("MCP server returned HTTP 404"),
        "got: {err}"
    );
}

#[tokio::test]
async fn http_transport_400_modern_error_is_jsonrpc_response() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
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

    let config = McpServerConfig {
        name: "test-http".into(),
        transport: McpTransport::Http,
        url: Some(server.uri()),
        ..Default::default()
    };
    let transport = HttpTransport::new(&config).expect("build transport");
    let req = JsonRpcRequest::new(0, "server/discover", serde_json::json!({}));
    let lifecycle = McpRequestLifecycle::uncoordinated(0);
    let resp = transport
        .send_and_recv(&req, &lifecycle)
        .await
        .expect("modern 400 must surface as JSON-RPC");
    let error = resp.error.expect("error payload");
    assert_eq!(error.code, crate::mcp_era::UNSUPPORTED_PROTOCOL_VERSION);
    assert!(
        crate::mcp_era::is_recognized_modern_error(error.code),
        "code {} should be a recognized modern error",
        error.code
    );
}

#[tokio::test]
async fn sse_post_404_is_not_replayed_to_derived_endpoint() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/sse"))
        .respond_with(ResponseTemplate::new(405))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/sse"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let config = McpServerConfig {
        name: "sse-no-replay".into(),
        transport: McpTransport::Sse,
        url: Some(format!("{}/sse", server.uri())),
        ..Default::default()
    };
    let transport = SseTransport::new(&config).expect("build transport");
    let request = JsonRpcRequest::new(7, "tools/call", serde_json::json!({}));
    let lifecycle = McpRequestLifecycle::uncoordinated(0);
    let error = SharedMcpTransportConn::send_and_recv(&transport, &request, &lifecycle)
        .await
        .expect_err("404 after a write must surface");
    assert!(matches!(
        error.downcast_ref::<McpTransportError>(),
        Some(McpTransportError::StaleSession { status: 404 })
    ));
    assert_eq!(lifecycle.outcome_unknown_epoch(), Some(0));

    let requests = server
        .received_requests()
        .await
        .expect("request recording enabled");
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method.as_str() == "POST" && request.url.path() == "/sse")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method.as_str() == "POST"
                && request.url.path() == "/messages")
            .count(),
        0,
        "a post-write 404 does not prove the first endpoint skipped execution"
    );
}

#[tokio::test]
async fn cancelled_direct_sse_request_removes_pending_waiter() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&server)
        .await;

    let config = McpServerConfig {
        name: "sse-cancel-direct".into(),
        transport: McpTransport::Sse,
        url: Some(format!("{}/sse", server.uri())),
        ..Default::default()
    };
    let transport = Arc::new(SseTransport::new(&config).expect("build transport"));
    let reader = zeroclaw_spawn::spawn!(std::future::pending::<()>());
    {
        let mut conn = transport.conn.lock().await;
        conn.stream_state = SseStreamState::Connected;
        conn.reader_task = Some(reader);
    }
    {
        let mut shared = transport.shared.lock().await;
        shared.message_url = Some(format!("{}/messages", server.uri()));
        shared.message_url_from_endpoint = true;
    }

    let task_transport = Arc::clone(&transport);
    let request = JsonRpcRequest::new(7, "tools/call", serde_json::json!({}));
    let lifecycle = McpRequestLifecycle::uncoordinated(0);
    let call = zeroclaw_spawn::spawn!(async move {
        SharedMcpTransportConn::send_and_recv(task_transport.as_ref(), &request, &lifecycle).await
    });
    timeout(Duration::from_secs(2), async {
        while transport.pending.lock().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("request did not register a waiter");

    call.abort();
    assert!(
        call.await
            .expect_err("call must be cancelled")
            .is_cancelled()
    );
    assert!(transport.pending.lock().is_empty());
    SharedMcpTransportConn::close(transport.as_ref())
        .await
        .expect("close transport");
}

#[tokio::test]
async fn sse_transport_reset_clears_session_and_endpoint_state() {
    let config = McpServerConfig {
        name: "test-sse".into(),
        transport: McpTransport::Sse,
        url: Some("http://localhost:1/sse".into()),
        ..Default::default()
    };
    let transport = SseTransport::new(&config).expect("build transport");
    transport.conn.lock().await.stream_state = SseStreamState::Connected;
    {
        let mut guard = transport.shared.lock().await;
        guard.message_url = Some("http://localhost:1/messages".into());
        guard.message_url_from_endpoint = true;
    }
    let (tx, _rx) = oneshot::channel();
    transport.pending.lock().insert(7, tx);

    SharedMcpTransportConn::reset(&transport)
        .await
        .expect("reset");

    assert_eq!(
        transport.conn.lock().await.stream_state,
        SseStreamState::Unknown
    );
    let guard = transport.shared.lock().await;
    assert!(guard.message_url.is_none());
    assert!(!guard.message_url_from_endpoint);
    drop(guard);
    assert!(transport.pending.lock().is_empty());
}

#[tokio::test]
async fn sse_transport_close_clears_reader_endpoint_and_pending_state() {
    let config = McpServerConfig {
        name: "test-sse-close".into(),
        transport: McpTransport::Sse,
        url: Some("http://localhost:1/sse".into()),
        ..Default::default()
    };
    let transport = SseTransport::new(&config).expect("build transport");
    let reader = zeroclaw_spawn::spawn!(std::future::pending::<()>());
    {
        let mut conn = transport.conn.lock().await;
        conn.stream_state = SseStreamState::Connected;
        conn.reader_task = Some(reader);
    }
    {
        let mut shared = transport.shared.lock().await;
        shared.message_url = Some("http://localhost:1/messages".into());
        shared.message_url_from_endpoint = true;
    }
    let (tx, rx) = oneshot::channel();
    transport.pending.lock().insert(7, tx);

    SharedMcpTransportConn::close(&transport)
        .await
        .expect("close");

    let conn = transport.conn.lock().await;
    assert_eq!(conn.stream_state, SseStreamState::Unknown);
    assert!(conn.reader_task.is_none());
    drop(conn);
    let shared = transport.shared.lock().await;
    assert!(shared.message_url.is_none());
    assert!(!shared.message_url_from_endpoint);
    drop(shared);
    assert!(transport.pending.lock().is_empty());
    assert!(rx.await.is_err(), "pending receiver must be released");
}

#[test]
fn apply_modern_post_headers_sets_method_name_and_version() {
    let request = JsonRpcRequest::new(
        1,
        "tools/call",
        serde_json::json!({"name": "echo", "arguments": {}}),
    );
    let req = apply_modern_post_headers(
        reqwest::Client::new().post("http://localhost/mcp"),
        &request,
        crate::mcp_era::MCP_MODERN_PROTOCOL_VERSION,
    )
    .build()
    .expect("build");
    assert_eq!(
        req.headers()
            .get(MCP_METHOD_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some("tools/call")
    );
    assert_eq!(
        req.headers()
            .get(MCP_NAME_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some("echo")
    );
    assert_eq!(
        req.headers()
            .get(MCP_PROTOCOL_VERSION_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some(crate::mcp_era::MCP_MODERN_PROTOCOL_VERSION)
    );
    assert!(req.headers().get(MCP_SESSION_ID_HEADER).is_none());
}

#[test]
fn apply_modern_post_headers_omits_name_for_list() {
    let request = JsonRpcRequest::new(1, "tools/list", serde_json::json!({}));
    let req = apply_modern_post_headers(
        reqwest::Client::new().post("http://localhost/mcp"),
        &request,
        crate::mcp_era::MCP_MODERN_PROTOCOL_VERSION,
    )
    .build()
    .expect("build");
    assert_eq!(
        req.headers()
            .get(MCP_METHOD_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some("tools/list")
    );
    assert!(req.headers().get(MCP_NAME_HEADER).is_none());
}

#[tokio::test]
async fn http_transport_modern_era_strips_configured_session_header() {
    use std::collections::HashMap;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"ok": true}
        })))
        .mount(&server)
        .await;

    let mut headers = HashMap::new();
    headers.insert(MCP_SESSION_ID_HEADER.to_string(), "from-config".into());
    headers.insert("X-Custom".to_string(), "keep".into());
    let config = McpServerConfig {
        name: "modern-http".into(),
        transport: McpTransport::Http,
        url: Some(server.uri()),
        headers,
        ..Default::default()
    };
    let transport = HttpTransport::new(&config).expect("build transport");
    let lifecycle = McpRequestLifecycle::uncoordinated_for_peer(0, &PeerProtocol::modern_default());
    let request = JsonRpcRequest::new(1, "tools/list", serde_json::json!({}));
    SharedMcpTransportConn::send_and_recv(&transport, &request, &lifecycle)
        .await
        .expect("modern POST");

    let received = server.received_requests().await.expect("requests");
    assert_eq!(received.len(), 1);
    assert!(
        received[0].headers.get(MCP_SESSION_ID_HEADER).is_none(),
        "configured Mcp-Session-Id must not ride on a modern POST"
    );
    assert_eq!(
        received[0]
            .headers
            .get("X-Custom")
            .and_then(|v| v.to_str().ok()),
        Some("keep")
    );
}

#[tokio::test]
async fn sse_transport_400_modern_error_is_jsonrpc_response() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
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

    let config = McpServerConfig {
        name: "test-sse".into(),
        transport: McpTransport::Sse,
        url: Some(server.uri()),
        ..Default::default()
    };
    let transport = SseTransport::new(&config).expect("build transport");
    let req = JsonRpcRequest::new(0, "server/discover", serde_json::json!({}));
    let lifecycle = McpRequestLifecycle::uncoordinated(0);
    let resp = transport
        .send_and_recv(&req, &lifecycle)
        .await
        .expect("modern 400 must surface as JSON-RPC");
    let error = resp.error.expect("error payload");
    assert_eq!(error.code, crate::mcp_era::UNSUPPORTED_PROTOCOL_VERSION);
}

#[tokio::test]
async fn http_transport_modern_era_does_not_send_or_store_session() {
    use wiremock::matchers::{header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header(MCP_METHOD_HEADER, "tools/call"))
        .and(header(MCP_NAME_HEADER, "echo"))
        .and(header(
            MCP_PROTOCOL_VERSION_HEADER,
            crate::mcp_era::MCP_MODERN_PROTOCOL_VERSION,
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header(MCP_SESSION_ID_HEADER, "must-not-store")
                .set_body_json(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {"ok": true}
                })),
        )
        .mount(&server)
        .await;

    let config = McpServerConfig {
        name: "modern-http".into(),
        transport: McpTransport::Http,
        url: Some(server.uri()),
        ..Default::default()
    };
    let transport = HttpTransport::new(&config).expect("build transport");
    *transport.session_id.lock() = Some("stale-legacy-session".into());

    let lifecycle = McpRequestLifecycle::uncoordinated_for_peer(0, &PeerProtocol::modern_default());
    let request = JsonRpcRequest::new(
        1,
        "tools/call",
        serde_json::json!({"name": "echo", "arguments": {}}),
    );
    let resp = SharedMcpTransportConn::send_and_recv(&transport, &request, &lifecycle)
        .await
        .expect("modern POST");
    assert_eq!(resp.result, Some(serde_json::json!({"ok": true})));
    assert_eq!(
        transport.session_id.lock().as_deref(),
        Some("stale-legacy-session"),
        "modern responses must not update Mcp-Session-Id"
    );

    let received = server.received_requests().await.expect("requests");
    assert_eq!(received.len(), 1);
    assert!(
        received[0].headers.get(MCP_SESSION_ID_HEADER).is_none(),
        "modern POST must not replay a leftover session id"
    );
}

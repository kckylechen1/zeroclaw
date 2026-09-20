use super::*;

#[test]
fn acp_server_config_defaults() {
    let cfg = AcpServerConfig::default();
    assert_eq!(cfg.max_sessions, 10);
    assert_eq!(cfg.session_timeout_secs, 3600);
}

#[test]
fn acp_server_config_deserialize() {
    let json = r#"{"max_sessions": 5, "session_timeout_secs": 1800}"#;
    let cfg: AcpServerConfig = serde_json::from_str(json).unwrap();
    assert_eq!(cfg.max_sessions, 5);
    assert_eq!(cfg.session_timeout_secs, 1800);
}

#[test]
fn acp_server_config_deserialize_partial() {
    let json = r#"{"max_sessions": 3}"#;
    let cfg: AcpServerConfig = serde_json::from_str(json).unwrap();
    assert_eq!(cfg.max_sessions, 3);
    assert_eq!(cfg.session_timeout_secs, 3600);
}

#[test]
fn json_rpc_request_parse() {
    let json = r#"{"jsonrpc":"2.0","method":"initialize","params":{},"id":1}"#;
    let req: JsonRpcRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.method, "initialize");
    assert_eq!(req.id, Some(Value::Number(1.into())));
}

#[test]
fn json_rpc_request_parse_notification() {
    let json = r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#;
    let req: JsonRpcRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.method, "session/update");
    assert!(req.id.is_none());
}

#[test]
fn json_rpc_response_serialize() {
    let resp = JsonRpcResponse {
        jsonrpc: "2.0",
        result: Some(serde_json::json!({"status": "ok"})),
        error: None,
        id: Value::Number(1.into()),
    };
    let json = serde_json::to_string(&resp).unwrap();
    let parsed: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["jsonrpc"], "2.0");
    assert!(parsed.get("result").is_some());
    assert!(parsed.get("error").is_none());
    assert_eq!(parsed["id"], 1);
}

#[tokio::test]
async fn rpc_request_timeout_drop_removes_pending_responder() {
    let (tx, mut rx) = mpsc::channel::<String>(16);
    let rpc = RpcOutbound::new(tx);

    let result = tokio::time::timeout(
        Duration::from_millis(10),
        rpc.request("session/request_permission", serde_json::json!({})),
    )
    .await;

    assert!(result.is_err());
    assert!(rx.recv().await.is_some());
    assert_eq!(rpc.pending_count(), 0);
}

#[test]
fn initialize_response_uses_acp_v1_shape() {
    let server = AcpServer::new(Config::default(), AcpServerConfig::default());
    let result = server
        .handle_initialize(&serde_json::json!({
            "protocolVersion": 1,
            "clientCapabilities": {},
            "clientInfo": {
                "name": "test-client",
                "version": "1.0.0"
            }
        }))
        .unwrap();

    assert_eq!(result["protocolVersion"], 1);
    assert_eq!(result["agentInfo"]["name"], "zeroclaw-acp");
    assert_eq!(result["agentInfo"]["title"], "ZeroClaw ACP");
    assert_eq!(result["agentInfo"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(result["authMethods"], serde_json::json!([]));
    assert_eq!(result["agentCapabilities"]["loadSession"], false);
    assert_eq!(
        result["agentCapabilities"]["promptCapabilities"]["image"],
        false
    );
    assert_eq!(
        result["agentCapabilities"]["mcpCapabilities"]["http"],
        false
    );
    assert!(result.get("serverInfo").is_none());
    assert!(result.get("capabilities").is_none());
}

#[test]
fn initialize_caches_client_elicitation_capabilities() {
    let server = AcpServer::new(Config::default(), AcpServerConfig::default());
    let _ = server
        .handle_initialize(&serde_json::json!({
            "protocolVersion": "1.0",
            "clientCapabilities": {
                "elicitation": { "form": {} }
            }
        }))
        .unwrap();
    let caps = *server.client_elicitation_caps.read().unwrap();
    assert!(caps.form);
    assert!(!caps.url);
}

#[test]
fn initialize_without_elicitation_leaves_default_caps() {
    let server = AcpServer::new(Config::default(), AcpServerConfig::default());
    let _ = server
        .handle_initialize(&serde_json::json!({
            "protocolVersion": "1.0",
            "clientCapabilities": {}
        }))
        .unwrap();
    let caps = *server.client_elicitation_caps.read().unwrap();
    assert!(!caps.form);
    assert!(!caps.url);
}

#[test]
fn initialize_advertises_load_session_when_store_present() {
    let cwd = tempfile::tempdir().unwrap();
    let store =
        Arc::new(zeroclaw_infra::acp_session_store::AcpSessionStore::new(cwd.path()).unwrap());
    let server = AcpServer::new_with_store(
        make_test_config(cwd.path()),
        AcpServerConfig::default(),
        store,
    );
    let result = server.handle_initialize(&serde_json::json!({})).unwrap();
    assert_eq!(result["agentCapabilities"]["loadSession"], true);
    assert_eq!(
        result["agentCapabilities"]["sessionCapabilities"]["resume"],
        serde_json::json!({})
    );
    assert_eq!(
        result["agentCapabilities"]["sessionCapabilities"]["close"],
        serde_json::json!({})
    );
}

#[test]
fn session_new_defaults_to_launch_cwd_when_client_omits_cwd() {
    let config = Config {
        data_dir: PathBuf::from("/not/the/project"),
        ..Default::default()
    };
    let server = AcpServer::new(config, AcpServerConfig::default());
    let expected = std::env::current_dir().unwrap();
    let config = server.config_snapshot();

    assert_eq!(
        server.requested_session_cwd(&serde_json::json!({}), &config),
        expected
    );
}

#[test]
fn session_new_respects_client_cwd_when_present() {
    let server = AcpServer::new(Config::default(), AcpServerConfig::default());
    let cwd = std::env::current_dir().unwrap();
    let config = server.config_snapshot();

    assert_eq!(
        server.requested_session_cwd(&serde_json::json!({"cwd": cwd}), &config),
        cwd
    );
}

#[tokio::test]
async fn session_new_does_not_wait_for_configured_mcp_servers() {
    let cwd = tempfile::tempdir().unwrap();
    let mut config = Config {
        data_dir: cwd.path().to_path_buf(),
        providers: {
            let mut p = zeroclaw_config::providers::Providers::default();
            p.models.openrouter.insert(
                "default".to_string(),
                zeroclaw_config::schema::OpenRouterModelProviderConfig {
                    base: zeroclaw_config::schema::ModelProviderConfig {
                        model: Some("test-model".to_string()),
                        ..Default::default()
                    },
                },
            );
            p
        },
        mcp: zeroclaw_config::schema::McpConfig {
            enabled: true,
            servers: vec![zeroclaw_config::schema::McpServerConfig {
                name: "slow".to_string(),
                transport: zeroclaw_config::schema::McpTransport::Stdio,
                command: "/bin/sh".to_string(),
                args: vec!["-c".to_string(), "sleep 60".to_string()],
                ..Default::default()
            }],
            ..Default::default()
        },
        ..Default::default()
    };
    config.risk_profiles.insert(
        "default".to_string(),
        zeroclaw_config::schema::RiskProfileConfig::default(),
    );
    config.runtime_profiles.insert(
        "default".to_string(),
        zeroclaw_config::schema::RuntimeProfileConfig::default(),
    );
    config.agents.insert(
        "test-agent".to_string(),
        dispatchable_test_agent("openrouter.default"),
    );
    let server = AcpServer::new(config, AcpServerConfig::default());

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        server.handle_session_new(&serde_json::json!({
            "cwd": cwd.path().to_string_lossy(),
            "agentAlias": "test-agent",
            "mcpServers": []
        })),
    )
    .await
    .expect("session/new should not block on configured MCP startup")
    .expect("session/new should create a session");

    assert!(result["sessionId"].as_str().is_some());
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn session_new_agent_init_failure_log_is_attributed_and_redacted() {
    let _writer_guard = zeroclaw_log::__private_test_writer_lock();
    let _hook_guard = zeroclaw_log::__private_test_hook_lock();
    zeroclaw_log::try_install_capture_subscriber();
    let mut rx = zeroclaw_log::subscribe_or_install();
    while rx.try_recv().is_ok() {}

    const EXPOSED_PREFIX: &str = "sk-ant-z";
    let cwd = tempfile::tempdir().unwrap();
    let mut config = Config {
        data_dir: cwd.path().to_path_buf(),
        providers: {
            let mut providers = zeroclaw_config::providers::Providers::default();
            providers.models.openrouter.insert(
                "default".to_string(),
                zeroclaw_config::schema::OpenRouterModelProviderConfig {
                    base: zeroclaw_config::schema::ModelProviderConfig {
                        api_key: Some("sk-ant-zeroclaw_test_credential".to_string()),
                        model: Some("test-model".to_string()),
                        ..Default::default()
                    },
                },
            );
            providers
        },
        ..Default::default()
    };
    config.risk_profiles.insert(
        "default".to_string(),
        zeroclaw_config::schema::RiskProfileConfig::default(),
    );
    config.agents.insert(
        "test-agent".to_string(),
        dispatchable_test_agent("openrouter.default"),
    );
    let server = AcpServer::new(config, AcpServerConfig::default());

    let error = server
        .handle_session_new(&serde_json::json!({
            "cwd": cwd.path().to_string_lossy(),
            "agentAlias": "test-agent",
        }))
        .await
        .expect_err("the mismatched credential must fail agent construction");
    assert!(
        error.message.contains("API key prefix mismatch"),
        "the RPC error must retain the agent construction failure: {}",
        error.message
    );
    assert!(error.message.contains("[REDACTED]"));
    assert!(
        !error.message.contains(EXPOSED_PREFIX),
        "the RPC error must not expose the credential fragment: {}",
        error.message
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let event = loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "agent init failure event was not emitted"
        );
        match tokio::time::timeout(remaining.min(Duration::from_millis(50)), rx.recv()).await {
            Ok(Ok(value))
                if value.get("message").and_then(Value::as_str)
                    == Some("ACP session/new failed: agent init error") =>
            {
                break value;
            }
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                panic!("log broadcast closed before the agent init failure event")
            }
            Err(_elapsed) => {}
        }
    };

    assert_eq!(event["severity_text"], "ERROR");
    assert_eq!(event["event"]["category"], "channel");
    assert_eq!(event["event"]["action"], "fail");
    assert_eq!(event["event"]["outcome"], "failure");
    assert_eq!(event["zeroclaw"]["channel_type"], "acp");
    assert_eq!(event["zeroclaw"]["agent_alias"], "test-agent");
    assert_eq!(event["zeroclaw"]["model_provider"], "openrouter.default");
    assert_eq!(event["zeroclaw"]["model"], "test-model");
    assert!(
        event["zeroclaw"]["session_key"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "the generated session key must be harvested as attribution: {event}"
    );
    assert_eq!(
        event["attributes"]["workspace_dir"],
        std::fs::canonicalize(cwd.path())
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );
    let logged_error = event["attributes"]["error"]
        .as_str()
        .expect("the failure event must retain sanitized error detail");
    assert!(logged_error.contains("API key prefix mismatch"));
    assert!(logged_error.contains("openrouter"));
    assert!(logged_error.contains("[REDACTED]"));
    assert!(
        !logged_error.contains(EXPOSED_PREFIX),
        "the persisted event must not contain the credential fragment: {logged_error}"
    );
}

/// Spin up a wiremock server speaking the minimum MCP HTTP handshake
/// (`initialize` → `notifications/initialized` → `tools/list`) advertising a
/// single tool. HTTP transport keeps the test cross-platform (no stdio
/// scripts). Mirrors the runtime crate'shelper.
async fn start_mock_mcp_http_server(tool_name: &str) -> wiremock::MockServer {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            serde_json::json!({"method": "initialize"}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Mcp-Session-Id", "sess-1")
                .set_body_json(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "remote", "version": "0.1.0"}
                    }
                })),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            serde_json::json!({"method": "notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            serde_json::json!({"method": "tools/list"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {"tools": [{
                "name": tool_name,
                "description": "List finance records",
                "inputSchema": {"type": "object"}
            }]}
        })))
        .mount(&server)
        .await;
    server
}

/// `make_test_config` plus an MCP server (`remote`, HTTP transport at
/// `mock_uri`) granted to `test-agent` through the `b1` mcp_bundle.
fn make_mcp_granting_test_config(cwd: &std::path::Path, mock_uri: String) -> Config {
    use zeroclaw_config::schema::{McpBundleConfig, McpServerConfig, McpTransport};

    let mut cfg = make_test_config(cwd);
    cfg.mcp.enabled = true;
    cfg.mcp.deferred_loading = false;
    cfg.mcp.servers = vec![McpServerConfig {
        name: "remote".into(),
        transport: McpTransport::Http,
        url: Some(mock_uri),
        ..Default::default()
    }];
    cfg.mcp_bundles.insert(
        "b1".into(),
        McpBundleConfig {
            servers: vec!["remote".into()],
            exclude: vec![],
        },
    );
    cfg.agents
        .get_mut("test-agent")
        .expect("test-agent must exist")
        .mcp_bundles = vec!["b1".into()];
    cfg
}

#[test]
fn agent_acp_enable_mcp_defaults_off() {
    assert!(
        !zeroclaw_config::schema::AliasedAgentConfig::default().acp_enable_mcp,
        "MCP must stay opt-in per agent so session/new is prompt by default (#8193)"
    );
}

#[tokio::test]
async fn session_new_skips_mcp_by_default() {
    let cwd = tempfile::tempdir().unwrap();
    let server = start_mock_mcp_http_server("records.list").await;
    let config = make_mcp_granting_test_config(cwd.path(), server.uri());
    let acp = AcpServer::new(config, AcpServerConfig::default());

    acp.handle_session_new(&serde_json::json!({
        "cwd": cwd.path().to_string_lossy(),
        "agentAlias": "test-agent"
    }))
    .await
    .expect("session/new must succeed");

    let requests = server
        .received_requests()
        .await
        .expect("mock records requests");
    assert!(
        requests.is_empty(),
        "default ACP session must not connect to granted MCP servers; got {} request(s)",
        requests.len()
    );
}

#[tokio::test]
async fn session_new_loads_mcp_bundles_when_agent_opts_in() {
    let cwd = tempfile::tempdir().unwrap();
    let server = start_mock_mcp_http_server("records.list").await;
    let mut config = make_mcp_granting_test_config(cwd.path(), server.uri());
    config
        .agents
        .get_mut("test-agent")
        .expect("test-agent must exist")
        .acp_enable_mcp = true;
    let acp = AcpServer::new(config, AcpServerConfig::default());

    acp.handle_session_new(&serde_json::json!({
        "cwd": cwd.path().to_string_lossy(),
        "agentAlias": "test-agent"
    }))
    .await
    .expect("session/new must succeed");

    let requests = server
        .received_requests()
        .await
        .expect("mock records requests");
    assert!(
        requests.iter().any(|r| {
            std::str::from_utf8(&r.body)
                .map(|b| b.contains("tools/list"))
                .unwrap_or(false)
        }),
        "agent with acp_enable_mcp must list tools from granted MCP servers; \
             got {} request(s)",
        requests.len()
    );
}

#[tokio::test]
async fn session_new_auto_selects_sole_configured_agent_when_alias_omitted() {
    let cwd = tempfile::tempdir().unwrap();
    let mut config = Config {
        data_dir: cwd.path().to_path_buf(),
        providers: {
            let mut p = zeroclaw_config::providers::Providers::default();
            p.models.openrouter.insert(
                "default".to_string(),
                zeroclaw_config::schema::OpenRouterModelProviderConfig {
                    base: zeroclaw_config::schema::ModelProviderConfig {
                        api_key: Some("test-key".to_string()),
                        model: Some("test-model".to_string()),
                        ..Default::default()
                    },
                },
            );
            p
        },
        ..Default::default()
    };
    config.risk_profiles.insert(
        "default".to_string(),
        zeroclaw_config::schema::RiskProfileConfig::default(),
    );
    config.runtime_profiles.insert(
        "default".to_string(),
        zeroclaw_config::schema::RuntimeProfileConfig::default(),
    );
    config.agents.insert(
        "only-agent".to_string(),
        dispatchable_test_agent("openrouter.default"),
    );
    let server = AcpServer::new(config, AcpServerConfig::default());

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        server.handle_session_new(&serde_json::json!({
            "cwd": cwd.path().to_string_lossy(),
            "mcpServers": []
        })),
    )
    .await
    .expect("session/new should not block")
    .expect("session/new should auto-select the sole configured agent");

    assert!(result["sessionId"].as_str().is_some());
}

#[tokio::test]
async fn session_new_requires_alias_when_multiple_agents_configured() {
    let mut config = Config::default();
    config.agents.insert(
        "agent-one".to_string(),
        zeroclaw_config::schema::AliasedAgentConfig::default(),
    );
    config.agents.insert(
        "agent-two".to_string(),
        zeroclaw_config::schema::AliasedAgentConfig::default(),
    );
    let server = AcpServer::new(config, AcpServerConfig::default());

    let err = server
        .handle_session_new(&serde_json::json!({"mcpServers": []}))
        .await
        .expect_err("session/new without agentAlias should fail when multiple agents exist");

    assert_eq!(err.code, INVALID_PARAMS);
    assert!(
        err.message.contains("agentAlias"),
        "error should mention agentAlias, got: {}",
        err.message
    );
}

#[tokio::test]
async fn session_new_uses_config_default_agent_when_alias_omitted_and_multiple_agents() {
    let cwd = tempfile::tempdir().unwrap();
    let mut config = Config {
        data_dir: cwd.path().to_path_buf(),
        providers: {
            let mut p = zeroclaw_config::providers::Providers::default();
            p.models.openrouter.insert(
                "default".to_string(),
                zeroclaw_config::schema::OpenRouterModelProviderConfig {
                    base: zeroclaw_config::schema::ModelProviderConfig {
                        api_key: Some("test-key".to_string()),
                        model: Some("test-model".to_string()),
                        ..Default::default()
                    },
                },
            );
            p
        },
        ..Default::default()
    };
    config.risk_profiles.insert(
        "default".to_string(),
        zeroclaw_config::schema::RiskProfileConfig::default(),
    );
    config.runtime_profiles.insert(
        "default".to_string(),
        zeroclaw_config::schema::RuntimeProfileConfig::default(),
    );
    config.agents.insert(
        "agent-alpha".to_string(),
        dispatchable_test_agent("openrouter.default"),
    );
    config.agents.insert(
        "agent-beta".to_string(),
        dispatchable_test_agent("openrouter.default"),
    );
    config.acp.default_agent = Some("agent-alpha".to_string());
    let server = AcpServer::new(config, AcpServerConfig::default());

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        server.handle_session_new(&serde_json::json!({
            "cwd": cwd.path().to_string_lossy(),
            "mcpServers": []
        })),
    )
    .await
    .expect("should not block")
    .expect("should select agent-alpha from config.acp.default_agent");

    assert!(result["sessionId"].as_str().is_some());
}

#[tokio::test]
async fn session_new_explicit_alias_overrides_config_default_agent() {
    let cwd = tempfile::tempdir().unwrap();
    let mut config = Config {
        data_dir: cwd.path().to_path_buf(),
        providers: {
            let mut p = zeroclaw_config::providers::Providers::default();
            p.models.openrouter.insert(
                "default".to_string(),
                zeroclaw_config::schema::OpenRouterModelProviderConfig {
                    base: zeroclaw_config::schema::ModelProviderConfig {
                        api_key: Some("test-key".to_string()),
                        model: Some("test-model".to_string()),
                        ..Default::default()
                    },
                },
            );
            p
        },
        ..Default::default()
    };
    config.risk_profiles.insert(
        "default".to_string(),
        zeroclaw_config::schema::RiskProfileConfig::default(),
    );
    config.runtime_profiles.insert(
        "default".to_string(),
        zeroclaw_config::schema::RuntimeProfileConfig::default(),
    );
    config.agents.insert(
        "agent-alpha".to_string(),
        dispatchable_test_agent("openrouter.default"),
    );
    config.agents.insert(
        "agent-beta".to_string(),
        dispatchable_test_agent("openrouter.default"),
    );
    config.acp.default_agent = Some("agent-alpha".to_string());
    let server = AcpServer::new(config, AcpServerConfig::default());

    // Explicit alias should win over config default
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        server.handle_session_new(&serde_json::json!({
            "agentAlias": "agent-beta",
            "cwd": cwd.path().to_string_lossy(),
            "mcpServers": []
        })),
    )
    .await
    .expect("should not block")
    .expect("should use agent-beta despite default_agent = agent-alpha");

    assert!(result["sessionId"].as_str().is_some());
}

/// `make_test_config` plus `agent-alpha`/`agent-beta`, for exercising the
/// connection-default slot of the `session/new` alias precedence chain.
fn dispatchable_test_agent(model_provider: &str) -> zeroclaw_config::schema::AliasedAgentConfig {
    zeroclaw_config::schema::AliasedAgentConfig {
        model_provider: model_provider.into(),
        risk_profile: "default".into(),
        runtime_profile: "default".into(),
        ..Default::default()
    }
}

fn two_agent_config(cwd: &std::path::Path) -> Config {
    let mut cfg = make_test_config(cwd);
    for alias in ["agent-alpha", "agent-beta"] {
        cfg.agents.insert(
            alias.to_string(),
            dispatchable_test_agent("anthropic.default"),
        );
    }
    cfg
}

async fn session_agent_alias(server: &AcpServer, session_id: &str) -> String {
    let sessions = server.sessions.lock().await;
    let session = sessions.get(session_id).expect("session must exist");
    let session = session.lock().await;
    session.agent_alias.clone()
}

#[tokio::test]
async fn session_new_uses_connection_default_agent_when_alias_omitted() {
    let cwd = tempfile::tempdir().unwrap();
    let server = AcpServer::new(two_agent_config(cwd.path()), AcpServerConfig::default())
        .with_connection_default_agent(Some("agent-beta".to_string()));

    let result = server
        .handle_session_new(&serde_json::json!({
            "cwd": cwd.path().to_string_lossy(),
            "mcpServers": []
        }))
        .await
        .expect("session/new should use the connection default agent");

    let session_id = result["sessionId"].as_str().unwrap();
    assert_eq!(session_agent_alias(&server, session_id).await, "agent-beta");
}

#[tokio::test]
async fn session_new_explicit_alias_overrides_connection_default_agent() {
    let cwd = tempfile::tempdir().unwrap();
    let server = AcpServer::new(two_agent_config(cwd.path()), AcpServerConfig::default())
        .with_connection_default_agent(Some("agent-beta".to_string()));

    let result = server
        .handle_session_new(&serde_json::json!({
            "agentAlias": "agent-alpha",
            "cwd": cwd.path().to_string_lossy(),
            "mcpServers": []
        }))
        .await
        .expect("explicit agentAlias should win over the connection default");

    let session_id = result["sessionId"].as_str().unwrap();
    assert_eq!(
        session_agent_alias(&server, session_id).await,
        "agent-alpha"
    );
}

#[tokio::test]
async fn session_new_connection_default_agent_overrides_config_default_agent() {
    let cwd = tempfile::tempdir().unwrap();
    let mut config = two_agent_config(cwd.path());
    config.acp.default_agent = Some("agent-alpha".to_string());
    let server = AcpServer::new(config, AcpServerConfig::default())
        .with_connection_default_agent(Some("agent-beta".to_string()));

    let result = server
        .handle_session_new(&serde_json::json!({
            "cwd": cwd.path().to_string_lossy(),
            "mcpServers": []
        }))
        .await
        .expect("connection default should win over [acp].default_agent");

    let session_id = result["sessionId"].as_str().unwrap();
    assert_eq!(session_agent_alias(&server, session_id).await, "agent-beta");
}

#[tokio::test]
async fn session_new_unknown_connection_default_agent_errors_like_explicit_alias() {
    let cwd = tempfile::tempdir().unwrap();
    let server = AcpServer::new(two_agent_config(cwd.path()), AcpServerConfig::default())
        .with_connection_default_agent(Some("ghost".to_string()));

    let err = server
        .handle_session_new(&serde_json::json!({
            "cwd": cwd.path().to_string_lossy(),
            "mcpServers": []
        }))
        .await
        .expect_err("an unconfigured connection default must fail session/new");

    assert_eq!(err.code, INVALID_PARAMS);
    assert!(
        err.message.contains("Unknown agent"),
        "error should reuse the explicit-alias validation message, got: {}",
        err.message
    );
}

#[tokio::test]
async fn session_new_blank_connection_default_agent_is_treated_as_absent() {
    let cwd = tempfile::tempdir().unwrap();
    let mut config = two_agent_config(cwd.path());
    config.acp.default_agent = Some("agent-alpha".to_string());
    let server = AcpServer::new(config, AcpServerConfig::default())
        .with_connection_default_agent(Some("  ".to_string()));

    let result = server
        .handle_session_new(&serde_json::json!({
            "cwd": cwd.path().to_string_lossy(),
            "mcpServers": []
        }))
        .await
        .expect("blank connection default should fall through to config default");

    let session_id = result["sessionId"].as_str().unwrap();
    assert_eq!(
        session_agent_alias(&server, session_id).await,
        "agent-alpha"
    );
}

#[tokio::test]
async fn session_load_restore_ignores_connection_default_when_persisted_agent_deleted() {
    let cwd = tempfile::tempdir().unwrap();
    let store =
        Arc::new(zeroclaw_infra::acp_session_store::AcpSessionStore::new(cwd.path()).unwrap());

    let session_id = "sess-restore-ignores-conn-default";
    store
        .create_session(session_id, "ghost-agent", &cwd.path().to_string_lossy())
        .unwrap();

    let (writer_tx, _writer_rx) = tokio::sync::mpsc::channel::<String>(64);
    let mut config = two_agent_config(cwd.path());
    config.acp.default_agent = Some("agent-alpha".to_string());
    let server = AcpServer::new_with_writer_and_store(
        config,
        AcpServerConfig::default(),
        writer_tx,
        Arc::clone(&store),
    )
    .with_connection_default_agent(Some("agent-beta".to_string()));

    server
        .handle_session_load(&serde_json::json!({
            "sessionId": session_id,
            "cwd": cwd.path().to_string_lossy()
        }))
        .await
        .expect("session/load must succeed for a deleted persisted agent");

    // Operator `[acp].default_agent` wins; `?agent=` must not rebind restore.
    assert_eq!(
        session_agent_alias(&server, session_id).await,
        "agent-alpha"
    );
}

#[tokio::test]
async fn session_new_disabled_connection_default_agent_errors_like_explicit_alias() {
    let cwd = tempfile::tempdir().unwrap();
    let mut config = two_agent_config(cwd.path());
    config.agents.get_mut("agent-beta").unwrap().enabled = false;
    let server = AcpServer::new(config, AcpServerConfig::default())
        .with_connection_default_agent(Some("agent-beta".to_string()));

    let err = server
        .handle_session_new(&serde_json::json!({
            "cwd": cwd.path().to_string_lossy(),
            "mcpServers": []
        }))
        .await
        .expect_err("a disabled connection default must fail session/new");

    assert_eq!(err.code, INVALID_PARAMS);
    assert!(
        err.message.contains("not enabled for dispatch"),
        "expected disabled-agent error, got: {}",
        err.message
    );
}

#[tokio::test]
async fn session_new_disabled_explicit_alias_errors() {
    let cwd = tempfile::tempdir().unwrap();
    let mut config = two_agent_config(cwd.path());
    config.agents.get_mut("agent-beta").unwrap().enabled = false;
    let server = AcpServer::new(config, AcpServerConfig::default());

    let err = server
        .handle_session_new(&serde_json::json!({
            "agentAlias": "agent-beta",
            "cwd": cwd.path().to_string_lossy(),
            "mcpServers": []
        }))
        .await
        .expect_err("explicit disabled alias must fail session/new");

    assert_eq!(err.code, INVALID_PARAMS);
    assert!(err.message.contains("not enabled for dispatch"));
}

#[tokio::test]
async fn session_load_restore_skips_missing_config_default_to_sole_agent() {
    let cwd = tempfile::tempdir().unwrap();
    let store =
        Arc::new(zeroclaw_infra::acp_session_store::AcpSessionStore::new(cwd.path()).unwrap());

    let session_id = "sess-restore-missing-config-default";
    store
        .create_session(session_id, "ghost-agent", &cwd.path().to_string_lossy())
        .unwrap();

    let (writer_tx, _writer_rx) = tokio::sync::mpsc::channel::<String>(64);
    let mut config = make_test_config(cwd.path());
    config.agents.clear();
    config.agents.insert(
        "agent-beta".to_string(),
        dispatchable_test_agent("anthropic.default"),
    );
    // Missing config default is skipped; sole configured agent applies.
    // Connection default stays out of the restore chain.
    config.acp.default_agent = Some("agent-alpha".to_string());
    let server = AcpServer::new_with_writer_and_store(
        config,
        AcpServerConfig::default(),
        writer_tx,
        Arc::clone(&store),
    )
    .with_connection_default_agent(Some("ghost".to_string()));

    server
        .handle_session_load(&serde_json::json!({
            "sessionId": session_id,
            "cwd": cwd.path().to_string_lossy()
        }))
        .await
        .expect("session/load must skip a missing config default");

    assert_eq!(session_agent_alias(&server, session_id).await, "agent-beta");
}

#[tokio::test]
async fn session_resume_restore_skips_disabled_persisted_owner() {
    let cwd = tempfile::tempdir().unwrap();
    let store =
        Arc::new(zeroclaw_infra::acp_session_store::AcpSessionStore::new(cwd.path()).unwrap());

    let session_id = "sess-resume-disabled-owner";
    store
        .create_session(session_id, "agent-alpha", &cwd.path().to_string_lossy())
        .unwrap();

    let (writer_tx, _writer_rx) = tokio::sync::mpsc::channel::<String>(64);
    let mut config = two_agent_config(cwd.path());
    config.agents.insert(
        "agent-gamma".to_string(),
        dispatchable_test_agent("anthropic.default"),
    );
    config.agents.get_mut("agent-alpha").unwrap().enabled = false;
    config.acp.default_agent = Some("agent-beta".to_string());
    let server = AcpServer::new_with_writer_and_store(
            config,
            AcpServerConfig::default(),
            writer_tx,
            Arc::clone(&store),
        )
        // Tempting transport rebind — must lose to operator default.
        .with_connection_default_agent(Some("agent-gamma".to_string()));

    server
        .handle_session_resume(&serde_json::json!({
            "sessionId": session_id,
            "cwd": cwd.path().to_string_lossy()
        }))
        .await
        .expect("session/resume must skip a disabled persisted owner");

    assert_eq!(session_agent_alias(&server, session_id).await, "agent-beta");
}

#[tokio::test]
async fn session_resume_restore_ignores_connection_default_when_persisted_agent_deleted() {
    let cwd = tempfile::tempdir().unwrap();
    let store =
        Arc::new(zeroclaw_infra::acp_session_store::AcpSessionStore::new(cwd.path()).unwrap());

    let session_id = "sess-resume-ignores-conn-default";
    store
        .create_session(session_id, "ghost-agent", &cwd.path().to_string_lossy())
        .unwrap();

    let (writer_tx, _writer_rx) = tokio::sync::mpsc::channel::<String>(64);
    let mut config = two_agent_config(cwd.path());
    config.acp.default_agent = Some("agent-alpha".to_string());
    let server = AcpServer::new_with_writer_and_store(
        config,
        AcpServerConfig::default(),
        writer_tx,
        Arc::clone(&store),
    )
    .with_connection_default_agent(Some("agent-beta".to_string()));

    server
        .handle_session_resume(&serde_json::json!({
            "sessionId": session_id,
            "cwd": cwd.path().to_string_lossy()
        }))
        .await
        .expect("session/resume must succeed for a deleted persisted agent");

    assert_eq!(
        session_agent_alias(&server, session_id).await,
        "agent-alpha"
    );
}

#[test]
fn json_rpc_error_response_serialize() {
    let resp = JsonRpcResponse {
        jsonrpc: "2.0",
        result: None,
        error: Some(JsonRpcError {
            code: METHOD_NOT_FOUND,
            message: "Method not found".to_string(),
            data: None,
        }),
        id: Value::Number(1.into()),
    };
    let json = serde_json::to_string(&resp).unwrap();
    let parsed: Value = serde_json::from_str(&json).unwrap();
    assert!(parsed.get("error").is_some());
    assert_eq!(parsed["error"]["code"], -32601);
    assert!(parsed.get("result").is_none());
}

#[test]
fn json_rpc_notification_serialize() {
    let notif = JsonRpcNotification {
        jsonrpc: "2.0",
        method: "session/update",
        params: serde_json::json!({
            "sessionId": "test-sid",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "hello" }
            }
        }),
    };
    let json = serde_json::to_string(&notif).unwrap();
    assert!(json.contains(r#""method":"session/update""#));
    assert!(json.contains(r#""sessionUpdate":"agent_message_chunk""#));
    assert!(json.contains(r#""text":"hello""#));
}

#[test]
fn test_prompt_parsing() {
    // String prompt
    let string_params = serde_json::json!({"prompt": "hello world"});
    let result = AcpServer::parse_prompt(&string_params).unwrap();
    assert_eq!(result, "hello world");

    // Array prompt (valid)
    let array_params = serde_json::json!({
        "prompt": [
            {"type": "text", "text": "part 1"},
            {"type": "text", "text": "part 2"}
        ]
    });
    let result = AcpServer::parse_prompt(&array_params).unwrap();
    assert_eq!(result, "part 1\n\npart 2");

    // Array prompt (empty or no text)
    let empty_array_params = serde_json::json!({"prompt": []});
    let result = AcpServer::parse_prompt(&empty_array_params);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code, INVALID_PARAMS);

    let no_text_params = serde_json::json!({
        "prompt": [
            {"type": "image", "data": "..."}
        ]
    });
    let result = AcpServer::parse_prompt(&no_text_params);
    assert!(result.is_err());

    // Array prompt with resource (file @-notation from ACP client)
    let resource_params = serde_json::json!({
        "prompt": [
            {"type": "text", "text": "analyze this file:"},
            {"type": "resource", "resource": {"uri": "file:///tmp/example.rs", "text": "fn main() { println!(\"hi\"); }", "mimeType": "text/rust"}}
        ]
    });
    let result = AcpServer::parse_prompt(&resource_params).unwrap();
    assert!(result.contains("analyze this file:"));
    assert!(result.contains("fn main() { println!(\"hi\"); }"));
}

#[test]
fn handle_initialize_default_model_absent_when_unconfigured() {
    let server = AcpServer::new(Config::default(), AcpServerConfig::default());
    let result = server.handle_initialize(&serde_json::json!({})).unwrap();
    assert!(
        result["_meta"]["zeroclaw"].get("defaultModel").is_none(),
        "defaultModel must be absent when no model_provider is configured, got: {}",
        result["_meta"]["zeroclaw"]["defaultModel"]
    );
}

#[test]
fn handle_initialize_default_model_reflects_configured_provider() {
    use zeroclaw_config::schema::{ModelProviderConfig, OllamaModelProviderConfig};
    let mut config = Config::default();
    config.providers.models.ollama.insert(
        "default".to_string(),
        OllamaModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("llama3.2".to_string()),
                ..Default::default()
            },
            ..OllamaModelProviderConfig::default()
        },
    );
    let server = AcpServer::new(config, AcpServerConfig::default());
    let result = server.handle_initialize(&serde_json::json!({})).unwrap();
    assert_eq!(result["_meta"]["zeroclaw"]["defaultModel"], "llama3.2");
}

#[test]
fn prompt_result_preserves_content_string_shape() {
    let result = AcpServer::prompt_result("test-sid".to_string(), "end_turn", "hello".into());
    assert_eq!(result["sessionId"], "test-sid");
    assert_eq!(result["stopReason"], "end_turn");
    assert_eq!(result["content"], "hello");
}

#[test]
fn cancelled_prompt_result_preserves_content_string_shape() {
    let with_partial = AcpServer::cancelled_prompt_result("test-sid".to_string(), "partial text");
    assert_eq!(with_partial["sessionId"], "test-sid");
    assert_eq!(with_partial["stopReason"], "cancelled");
    assert_eq!(
        with_partial["content"],
        format!(
            "partial text\n\n{}",
            zeroclaw_runtime::i18n::get_required_cli_string("turn-cancelled-client-rpc")
        )
    );

    let marker_only = AcpServer::cancelled_prompt_result("test-sid".to_string(), "");
    assert_eq!(
        marker_only["content"],
        zeroclaw_runtime::i18n::get_required_cli_string("turn-cancelled-client-rpc")
    );
}

#[test]
fn test_tool_call_and_update_serialization() {
    // Test tool_call (initial pending event)
    let tool_call_notif = JsonRpcNotification {
        jsonrpc: "2.0",
        method: "session/update",
        params: serde_json::json!({
            "sessionId": "test-sid",
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "tc-12345",
                "name": "shell",
                "title": "shell",
                "kind": "execute",
                "rawInput": {"command": "ls -la"},
                "status": "pending"
            }
        }),
    };
    let json1 = serde_json::to_string(&tool_call_notif).unwrap();
    assert!(json1.contains("\"sessionUpdate\":\"tool_call\""));
    assert!(json1.contains("\"toolCallId\":\"tc-12345\""));
    assert!(json1.contains("\"name\":\"shell\""));
    assert!(json1.contains("\"title\":\"shell\""));
    assert!(json1.contains("\"kind\":\"execute\""));
    assert!(json1.contains("\"status\":\"pending\""));
    assert!(json1.contains("\"rawInput\""));

    // Test tool_call_update completion payload
    let tool_update_notif = JsonRpcNotification {
        jsonrpc: "2.0",
        method: "session/update",
        params: serde_json::json!({
            "sessionId": "test-sid",
            "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "tc-12345",
                "name": "shell",
                "title": "shell",
                "kind": "execute",
                "status": "completed",
                "rawOutput": "file1.txt\nfile2.txt",
                "body": "file1.txt\nfile2.txt",
                "content": [{
                    "type": "content",
                    "content": {
                        "type": "text",
                        "text": "file1.txt\nfile2.txt"
                    }
                }]
            }
        }),
    };
    let json2 = serde_json::to_string(&tool_update_notif).unwrap();
    assert!(json2.contains("\"sessionUpdate\":\"tool_call_update\""));
    assert!(json2.contains("\"toolCallId\":\"tc-12345\""));
    assert!(json2.contains("\"name\":\"shell\""));
    assert!(json2.contains("\"status\":\"completed\""));
    assert!(json2.contains("\"rawOutput\""));
    assert!(json2.contains("\"body\""));
    assert!(json2.contains("\"content\""));
    assert!(json2.contains("\"type\":\"content\""));
    assert!(json2.contains("file1.txt"));
    // Verify matching toolCallId across events
    assert!(json1.contains("tc-12345") && json2.contains("tc-12345"));
}

#[test]
fn file_edit_raw_input_uses_acp_diff_field_names() {
    let call = notification_for_turn_event(
        "sid",
        &TurnEvent::ToolCall {
            id: "tc-1".to_string(),
            name: "file_edit".to_string(),
            args: serde_json::json!({
                "path": "src/foo.rs",
                "old_string": "let x = 1;",
                "new_string": "let x = 2;"
            }),
        },
    );
    let v = serde_json::to_value(call.unwrap()).unwrap();
    let raw = &v["params"]["update"]["rawInput"];
    assert_eq!(raw["path"], "src/foo.rs");
    assert_eq!(raw["oldText"], "let x = 1;");
    assert_eq!(raw["newText"], "let x = 2;");
    assert!(
        raw.get("old_string").is_none(),
        "old_string must not appear in rawInput"
    );
    assert!(
        raw.get("new_string").is_none(),
        "new_string must not appear in rawInput"
    );

    let content = &v["params"]["update"]["content"];
    assert!(content.is_array(), "file_edit must emit a content array");
    let diff = &content[0];
    assert_eq!(diff["type"], "diff");
    assert_eq!(diff["path"], "src/foo.rs");
    assert_eq!(diff["oldText"], "let x = 1;");
    assert_eq!(diff["newText"], "let x = 2;");
}

#[test]
fn file_write_raw_input_uses_acp_diff_field_names() {
    let call = notification_for_turn_event(
        "sid",
        &TurnEvent::ToolCall {
            id: "tc-2".to_string(),
            name: "file_write".to_string(),
            args: serde_json::json!({
                "path": "src/new.rs",
                "content": "fn main() {}"
            }),
        },
    );
    let v = serde_json::to_value(call.unwrap()).unwrap();
    let raw = &v["params"]["update"]["rawInput"];
    assert_eq!(raw["path"], "src/new.rs");
    assert_eq!(raw["newText"], "fn main() {}");
    assert!(
        raw.get("oldText").is_none(),
        "oldText must not appear in file_write rawInput"
    );
    assert!(
        raw.get("content").is_none(),
        "content must not appear in rawInput"
    );

    let content = &v["params"]["update"]["content"];
    assert!(content.is_array(), "file_write must emit a content array");
    let diff = &content[0];
    assert_eq!(diff["type"], "diff");
    assert_eq!(diff["path"], "src/new.rs");
    assert_eq!(diff["newText"], "fn main() {}");
    assert!(
        diff.get("oldText").is_none(),
        "oldText must be absent for file_write diff"
    );
}

#[test]
fn map_tool_kind_uses_explicit_tool_names() {
    assert_eq!(map_tool_kind("memory_forget"), "delete");
    assert_eq!(map_tool_kind("memory_purge"), "delete");
    assert_eq!(map_tool_kind("cron_run"), "execute");
    assert_eq!(map_tool_kind("file_read"), "other");
    assert_eq!(map_tool_kind("knowledge"), "other");
    assert_eq!(map_tool_kind("web_fetch"), "other");
    assert_eq!(map_tool_kind("file_write"), "edit");
    assert_eq!(map_tool_kind("unknown_tool"), "other");
}

#[test]
fn restore_trim_event_maps_to_extension_notification() {
    let notification = notification_for_turn_event(
        "restored-session",
        &TurnEvent::HistoryTrimmed {
            dropped_messages: 12,
            kept_turns: 3,
            reason: "message limit".to_string(),
        },
    )
    .expect("history trim must produce an ACP notification");
    let value = serde_json::to_value(notification).unwrap();

    assert_eq!(value["method"], "_zeroclaw/history_trimmed");
    assert_eq!(
        value["params"],
        serde_json::json!({
            "sessionId": "restored-session",
            "droppedMessages": 12,
            "keptTurns": 3,
            "reason": "message limit",
        })
    );
    assert!(value["params"].get("update").is_none());
    assert!(!value.to_string().contains("sessionUpdate"));
}

#[test]
fn turn_tool_events_include_client_visible_tool_fields() {
    let call = notification_for_turn_event(
        "test-sid",
        &TurnEvent::ToolCall {
            id: "tc-12345".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "ls -la"}),
        },
    );
    let call_value = serde_json::to_value(call.expect("ToolCall maps to a notification")).unwrap();
    assert_eq!(call_value["method"], "session/update");
    assert_eq!(call_value["params"]["update"]["sessionUpdate"], "tool_call");
    assert_eq!(call_value["params"]["update"]["toolCallId"], "tc-12345");
    assert_eq!(call_value["params"]["update"]["name"], "shell");
    assert_eq!(call_value["params"]["update"]["title"], "shell");
    assert_eq!(call_value["params"]["update"]["kind"], "execute");
    assert_eq!(
        call_value["params"]["update"]["rawInput"],
        serde_json::json!({"command": "ls -la"})
    );

    let result = notification_for_turn_event(
        "test-sid",
        &TurnEvent::ToolResult {
            id: "tc-12345".to_string(),
            name: "shell".to_string(),
            output: "file1.txt\nfile2.txt".to_string(),
        },
    );
    let result_value =
        serde_json::to_value(result.expect("ToolResult maps to a notification")).unwrap();
    assert_eq!(
        result_value["params"]["update"]["sessionUpdate"],
        "tool_call_update"
    );
    assert_eq!(result_value["params"]["update"]["toolCallId"], "tc-12345");
    assert_eq!(result_value["params"]["update"]["name"], "shell");
    assert_eq!(result_value["params"]["update"]["title"], "shell");
    assert_eq!(result_value["params"]["update"]["kind"], "execute");
    assert_eq!(result_value["params"]["update"]["status"], "completed");
    assert_eq!(
        result_value["params"]["update"]["rawOutput"],
        "file1.txt\nfile2.txt"
    );
    assert_eq!(
        result_value["params"]["update"]["body"],
        "file1.txt\nfile2.txt"
    );
    assert_eq!(
        result_value["params"]["update"]["content"][0]["content"]["text"],
        "file1.txt\nfile2.txt"
    );
}

#[test]
fn plan_event_projects_to_acp_plan_update() {
    use zeroclaw_api::plan::{PlanEntry, PlanPriority, PlanStatus};

    let event = TurnEvent::Plan {
        entries: vec![
            PlanEntry {
                content: "Analyze the existing codebase structure".to_string(),
                status: PlanStatus::Pending,
                priority: PlanPriority::High,
                active_form: None,
            },
            PlanEntry {
                content: "Create unit tests".to_string(),
                status: PlanStatus::InProgress,
                priority: PlanPriority::Medium,
                active_form: Some("Creating unit tests".to_string()),
            },
        ],
    };
    let notif =
        notification_for_turn_event("sess_abc", &event).expect("plan yields a notification");
    let v = serde_json::to_value(&notif).unwrap();

    assert_eq!(v["method"], "session/update");
    assert_eq!(v["params"]["sessionId"], "sess_abc");
    assert_eq!(v["params"]["update"]["sessionUpdate"], "plan");
    let entries = v["params"]["update"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[0]["content"],
        "Analyze the existing codebase structure"
    );
    assert_eq!(entries[0]["priority"], "high");
    assert_eq!(entries[0]["status"], "pending");
    // ACP-required fields always present on every entry:
    assert!(entries[1]["priority"].is_string());
    assert_eq!(entries[1]["status"], "in_progress");
    // ZeroClaw extension carried but additive:
    assert_eq!(entries[1]["activeForm"], "Creating unit tests");
}

#[tokio::test]
async fn session_stop_finds_session_during_active_prompt_turn() {
    let cwd = tempfile::tempdir().unwrap();
    let server = Arc::new(AcpServer::new(
        make_test_config(cwd.path()),
        AcpServerConfig::default(),
    ));

    // Create a real session via the normal path.
    let new_result = server
        .handle_session_new(&serde_json::json!({
            "cwd": cwd.path().to_string_lossy(),
            "agentAlias": "test-agent"
        }))
        .await
        .expect("session/new must succeed");
    let session_id = new_result["sessionId"].as_str().unwrap().to_string();

    // Grab the inner lock to simulate an in-flight prompt turn.
    let session_arc = {
        let sessions = server.sessions.lock().await;
        sessions.get(&session_id).cloned().unwrap()
    };
    let _guard = session_arc.lock().await;

    // session/stop should find the session in the outer map.  With the
    // inner lock held it blocks — confirm it does NOT immediately return
    // SESSION_NOT_FOUND.
    let server_clone = Arc::clone(&server);
    let sid_clone = session_id.clone();
    let stop_result = tokio::time::timeout(Duration::from_millis(100), async move {
        server_clone
            .handle_session_stop(&serde_json::json!({ "sessionId": sid_clone }))
            .await
    })
    .await;

    match stop_result {
        Err(_timeout) => {} // expected — blocked waiting for the inner lock
        Ok(Ok(_)) => panic!("stop returned Ok without the lock being released"),
        Ok(Err(e)) => {
            assert_ne!(
                e.code, SESSION_NOT_FOUND,
                "session/stop must not return SESSION_NOT_FOUND while a turn is in flight"
            );
        }
    }
}

#[tokio::test]
async fn session_new_persists_to_store() {
    let cwd = tempfile::tempdir().unwrap();
    let store =
        Arc::new(zeroclaw_infra::acp_session_store::AcpSessionStore::new(cwd.path()).unwrap());
    let server = Arc::new(AcpServer::new_with_store(
        make_test_config(cwd.path()),
        AcpServerConfig::default(),
        Arc::clone(&store),
    ));

    let result = server
        .handle_session_new(&serde_json::json!({
            "cwd": cwd.path().to_string_lossy()
        }))
        .await
        .expect("session/new must succeed");

    let session_id = result["sessionId"].as_str().unwrap();

    // Session must appear in the store
    let data = store.load_session(session_id).unwrap();
    assert!(
        data.is_some(),
        "session/new must persist to AcpSessionStore"
    );
}

#[tokio::test]
async fn session_new_without_store_still_works() {
    let cwd = tempfile::tempdir().unwrap();
    let server = Arc::new(AcpServer::new(
        make_test_config(cwd.path()),
        AcpServerConfig::default(),
    ));

    let result = server
        .handle_session_new(&serde_json::json!({
            "cwd": cwd.path().to_string_lossy()
        }))
        .await
        .expect("session/new must succeed without a store");

    let session_id = result["sessionId"].as_str().unwrap();
    assert!(server.sessions.lock().await.contains_key(session_id));
}

fn make_test_config(cwd: &std::path::Path) -> Config {
    let mut cfg = Config {
        data_dir: cwd.to_path_buf(),
        ..Default::default()
    };
    cfg.providers.models.anthropic.insert(
        "default".to_string(),
        zeroclaw_config::schema::AnthropicModelProviderConfig {
            base: zeroclaw_config::schema::ModelProviderConfig {
                model: Some("claude-haiku-4-5".to_string()),
                ..Default::default()
            },
        },
    );
    cfg.risk_profiles.insert(
        "default".to_string(),
        zeroclaw_config::schema::RiskProfileConfig::default(),
    );
    cfg.runtime_profiles.insert(
        "default".to_string(),
        zeroclaw_config::schema::RuntimeProfileConfig::default(),
    );
    cfg.agents.insert(
        "test-agent".to_string(),
        dispatchable_test_agent("anthropic.default"),
    );
    cfg
}

#[test]
fn gateway_backed_server_initialize_uses_reloaded_config() {
    let cwd = tempfile::tempdir().unwrap();
    let config = Arc::new(parking_lot::RwLock::new(make_test_config(cwd.path())));
    let (writer_tx, _writer_rx) = mpsc::channel::<String>(1);
    let server = AcpServer::new_with_live_config_and_writer(
        Arc::clone(&config),
        AcpServerConfig::default(),
        writer_tx,
    );

    config
        .write()
        .providers
        .models
        .anthropic
        .get_mut("default")
        .unwrap()
        .base
        .model = Some("reloaded-model".to_string());

    assert_eq!(
        server.handle_initialize(&serde_json::json!({})).unwrap()["_meta"]["zeroclaw"]["defaultModel"],
        "reloaded-model"
    );
}

/// `session/cancel` on an idle session (no active turn) must succeed silently.
#[tokio::test]
async fn session_cancel_idle_session_is_noop() {
    let cwd = tempfile::tempdir().unwrap();
    let server = Arc::new(AcpServer::new(
        make_test_config(cwd.path()),
        AcpServerConfig::default(),
    ));

    let new_result = server
        .handle_session_new(&serde_json::json!({
            "cwd": cwd.path().to_string_lossy(),
            "agentAlias": "test-agent"
        }))
        .await
        .expect("session/new must succeed");
    let session_id = new_result["sessionId"].as_str().unwrap().to_string();

    // No active turn — cancel must not error.
    let result = server
        .handle_session_cancel(&serde_json::json!({ "sessionId": session_id }))
        .await;
    assert!(result.is_ok(), "idle cancel must succeed: {result:?}");
}

#[tokio::test]
async fn session_cancel_unknown_session_is_noop() {
    let cwd = tempfile::tempdir().unwrap();
    let server = Arc::new(AcpServer::new(
        make_test_config(cwd.path()),
        AcpServerConfig::default(),
    ));

    let result = server
        .handle_session_cancel(&serde_json::json!({ "sessionId": "sess_does_not_exist" }))
        .await;
    assert!(
        result.is_ok(),
        "unknown-session cancel must succeed: {result:?}"
    );
}

#[tokio::test]
async fn session_cancel_accepts_snake_case_session_id() {
    let cwd = tempfile::tempdir().unwrap();
    let server = Arc::new(AcpServer::new(
        make_test_config(cwd.path()),
        AcpServerConfig::default(),
    ));

    let session_id = "sess_snake_case_cancel";
    let active_token = tokio_util::sync::CancellationToken::new();
    server
        .register_cancel_token(session_id, active_token.clone())
        .expect("active turn should register token");

    server
        .handle_session_cancel(&serde_json::json!({ "session_id": session_id }))
        .await
        .expect("snake_case session_id should cancel the active turn");

    assert!(active_token.is_cancelled());
}

#[tokio::test]
async fn register_cancel_token_rejects_concurrent_prompt_for_session() {
    let cwd = tempfile::tempdir().unwrap();
    let server = Arc::new(AcpServer::new(
        make_test_config(cwd.path()),
        AcpServerConfig::default(),
    ));

    let session_id = "sess_active_turn";
    let active_token = tokio_util::sync::CancellationToken::new();
    let queued_token = tokio_util::sync::CancellationToken::new();

    server
        .register_cancel_token(session_id, active_token.clone())
        .expect("first prompt should register its token");
    let err = server
        .register_cancel_token(session_id, queued_token.clone())
        .expect_err("second prompt must not overwrite active token");

    assert_eq!(err.code, SESSION_BUSY);
    assert!(
        err.message.contains("active prompt turn"),
        "error should explain why prompt was rejected: {}",
        err.message
    );

    server
        .handle_session_cancel(&serde_json::json!({ "sessionId": session_id }))
        .await
        .expect("cancel should still target active token");

    assert!(active_token.is_cancelled());
    assert!(
        !queued_token.is_cancelled(),
        "rejected prompt's token must not become the active cancel target"
    );
}

#[tokio::test]
async fn session_prompt_rejects_concurrent_turn_before_agent_starts() {
    let cwd = tempfile::tempdir().unwrap();
    let server = Arc::new(AcpServer::new(
        make_test_config(cwd.path()),
        AcpServerConfig::default(),
    ));

    let new_result = server
        .handle_session_new(&serde_json::json!({
            "cwd": cwd.path().to_string_lossy(),
            "agentAlias": "test-agent"
        }))
        .await
        .expect("session/new must succeed");
    let session_id = new_result["sessionId"].as_str().unwrap().to_string();
    let active_token = tokio_util::sync::CancellationToken::new();
    server
        .register_cancel_token(&session_id, active_token.clone())
        .expect("simulated active turn should register token");

    let err = server
        .handle_session_prompt(
            &serde_json::json!({
                "sessionId": session_id.clone(),
                "prompt": "queued prompt"
            }),
            &serde_json::json!(2),
        )
        .await
        .expect_err("concurrent prompt must be rejected before model_provider work starts");

    assert_eq!(err.code, SESSION_BUSY);
    server
        .handle_session_cancel(&serde_json::json!({ "sessionId": session_id }))
        .await
        .expect("cancel should still target the original active token");
    assert!(active_token.is_cancelled());
}

#[tokio::test]
async fn cancel_tokens_map_remove_works() {
    let cwd = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: cwd.path().to_path_buf(),
        ..Default::default()
    };
    let server = Arc::new(AcpServer::new(config, AcpServerConfig::default()));

    // Insert and remove a token directly.
    let session_id = "sess_token_leak_test".to_string();
    let token = tokio_util::sync::CancellationToken::new();
    server
        .cancel_tokens
        .lock()
        .expect("cancel_tokens lock poisoned")
        .insert(session_id.clone(), token);

    // Remove the token.
    server
        .cancel_tokens
        .lock()
        .expect("cancel_tokens lock poisoned")
        .remove(&session_id);

    let remaining = server
        .cancel_tokens
        .lock()
        .expect("cancel_tokens lock poisoned")
        .len();
    assert_eq!(remaining, 0, "cancel token must be removed after turn ends");
}

#[tokio::test]
async fn session_load_restores_history_and_streams_notifications() {
    use zeroclaw_api::model_provider::{ChatMessage, ConversationMessage};
    let cwd = tempfile::tempdir().unwrap();
    let store =
        Arc::new(zeroclaw_infra::acp_session_store::AcpSessionStore::new(cwd.path()).unwrap());

    let session_id = "sess-load-test";
    store
        .create_session(session_id, "test-agent", &cwd.path().to_string_lossy())
        .unwrap();
    store
        .append_turn(
            session_id,
            &[
                ConversationMessage::Chat(ChatMessage::user("hello")),
                ConversationMessage::Chat(ChatMessage::assistant("hi there")),
            ],
        )
        .unwrap();

    let (writer_tx, mut writer_rx) = tokio::sync::mpsc::channel::<String>(64);
    let server = Arc::new(AcpServer::new_with_writer_and_store(
        make_test_config(cwd.path()),
        AcpServerConfig::default(),
        writer_tx,
        Arc::clone(&store),
    ));

    let result = server
        .handle_session_load(&serde_json::json!({
            "sessionId": session_id,
            "cwd": cwd.path().to_string_lossy()
        }))
        .await
        .expect("session/load must succeed");

    assert_eq!(result, serde_json::json!({}));

    // Session must now be in the in-memory map
    assert!(server.sessions.lock().await.contains_key(session_id));

    // Collect notifications (non-blocking drain)
    let mut notifications = Vec::new();
    while let Ok(msg) = writer_rx.try_recv() {
        notifications.push(msg);
    }

    // Expect two session/update notifications: user then assistant
    assert_eq!(
        notifications.len(),
        2,
        "expected 2 notifications, got: {notifications:?}"
    );
    let n0: serde_json::Value = serde_json::from_str(&notifications[0]).unwrap();
    assert_eq!(
        n0["params"]["update"]["sessionUpdate"],
        "user_message_chunk"
    );
    assert_eq!(n0["params"]["update"]["content"]["text"], "hello");
    let n1: serde_json::Value = serde_json::from_str(&notifications[1]).unwrap();
    assert_eq!(
        n1["params"]["update"]["sessionUpdate"],
        "agent_message_chunk"
    );
    assert_eq!(n1["params"]["update"]["content"]["text"], "hi there");
}

#[tokio::test]
async fn session_load_replays_only_history_retained_after_restore_trim() {
    use zeroclaw_api::model_provider::{ChatMessage, ConversationMessage};

    let cwd = tempfile::tempdir().unwrap();
    let store =
        Arc::new(zeroclaw_infra::acp_session_store::AcpSessionStore::new(cwd.path()).unwrap());
    let session_id = "sess-load-trimmed-test";
    store
        .create_session(session_id, "test-agent", &cwd.path().to_string_lossy())
        .unwrap();
    store
        .append_turn(
            session_id,
            &[
                ConversationMessage::Chat(ChatMessage::user("old request")),
                ConversationMessage::Chat(ChatMessage::assistant("old answer")),
            ],
        )
        .unwrap();
    store
        .append_turn(
            session_id,
            &[
                ConversationMessage::Chat(ChatMessage::user("new request")),
                ConversationMessage::Chat(ChatMessage::assistant("new answer")),
            ],
        )
        .unwrap();

    let mut config = make_test_config(cwd.path());
    config
        .runtime_profiles
        .get_mut("default")
        .unwrap()
        .max_history_messages = Some(2);
    let (writer_tx, mut writer_rx) = tokio::sync::mpsc::channel::<String>(64);
    let server = Arc::new(AcpServer::new_with_writer_and_store(
        config,
        AcpServerConfig::default(),
        writer_tx,
        Arc::clone(&store),
    ));

    server
        .handle_session_load(&serde_json::json!({ "sessionId": session_id }))
        .await
        .expect("session/load must succeed");

    let mut notifications = Vec::new();
    while let Ok(message) = writer_rx.try_recv() {
        notifications.push(serde_json::from_str::<serde_json::Value>(&message).unwrap());
    }

    assert_eq!(
        notifications.len(),
        3,
        "unexpected replay: {notifications:?}"
    );
    assert_eq!(notifications[0]["method"], "_zeroclaw/history_trimmed");
    assert_eq!(
        notifications[1]["params"]["update"]["content"]["text"],
        "new request"
    );
    assert_eq!(
        notifications[2]["params"]["update"]["content"]["text"],
        "new answer"
    );
    assert!(
        !notifications.iter().any(|notification| {
            let text = &notification["params"]["update"]["content"]["text"];
            text == "old request" || text == "old answer"
        }),
        "trimmed messages must not be replayed to the client"
    );
}

#[tokio::test]
async fn session_load_returns_not_found_for_unknown_id() {
    let cwd = tempfile::tempdir().unwrap();
    let store =
        Arc::new(zeroclaw_infra::acp_session_store::AcpSessionStore::new(cwd.path()).unwrap());
    let (writer_tx, _rx) = tokio::sync::mpsc::channel::<String>(8);
    let server = AcpServer::new_with_writer_and_store(
        make_test_config(cwd.path()),
        AcpServerConfig::default(),
        writer_tx,
        store,
    );

    let err = server
        .handle_session_load(&serde_json::json!({ "sessionId": "ghost" }))
        .await
        .expect_err("unknown session must fail");

    assert_eq!(err.code, SESSION_NOT_FOUND);
}

#[tokio::test]
async fn session_load_rejects_already_active_session() {
    let cwd = tempfile::tempdir().unwrap();
    let store =
        Arc::new(zeroclaw_infra::acp_session_store::AcpSessionStore::new(cwd.path()).unwrap());
    let (writer_tx, _rx) = tokio::sync::mpsc::channel::<String>(8);
    let server = Arc::new(AcpServer::new_with_writer_and_store(
        make_test_config(cwd.path()),
        AcpServerConfig::default(),
        writer_tx,
        Arc::clone(&store),
    ));

    // Create and load the session once to put it in memory
    let session_id = "sess-already-active";
    store
        .create_session(session_id, "test-agent", &cwd.path().to_string_lossy())
        .unwrap();
    server
        .handle_session_load(&serde_json::json!({
            "sessionId": session_id,
            "cwd": cwd.path().to_string_lossy()
        }))
        .await
        .unwrap();

    // Second load must be rejected
    let err = server
        .handle_session_load(&serde_json::json!({ "sessionId": session_id }))
        .await
        .expect_err("session/load for active session must fail");

    assert_eq!(err.code, INVALID_PARAMS);
}

fn make_cross_agent_restore_config(cwd: &std::path::Path, mock_uri: String) -> Config {
    let mut cfg = make_mcp_granting_test_config(cwd, mock_uri);
    // ACP default agent: no bundle, MCP off.
    {
        let ta = cfg.agents.get_mut("test-agent").expect("test-agent exists");
        ta.mcp_bundles = vec![];
        ta.acp_enable_mcp = false;
    }
    // Session owner: granted the bundle and opted into ACP MCP.
    cfg.agents.insert(
        "finance".to_string(),
        zeroclaw_config::schema::AliasedAgentConfig {
            model_provider: "anthropic.default".into(),
            risk_profile: "default".into(),
            runtime_profile: "default".into(),
            mcp_bundles: vec!["b1".into()],
            acp_enable_mcp: true,
            ..Default::default()
        },
    );
    cfg.acp.default_agent = Some("test-agent".to_string());
    cfg
}

#[tokio::test]
async fn session_load_restores_owning_agent_and_its_mcp_optin() {
    let cwd = tempfile::tempdir().unwrap();
    let mcp = start_mock_mcp_http_server("records.list").await;
    let config = make_cross_agent_restore_config(cwd.path(), mcp.uri());

    let store =
        Arc::new(zeroclaw_infra::acp_session_store::AcpSessionStore::new(cwd.path()).unwrap());
    let session_id = "sess-cross-agent-load";
    store
        .create_session(session_id, "finance", &cwd.path().to_string_lossy())
        .unwrap();

    let (writer_tx, _rx) = tokio::sync::mpsc::channel::<String>(64);
    let server = Arc::new(AcpServer::new_with_writer_and_store(
        config,
        AcpServerConfig::default(),
        writer_tx,
        Arc::clone(&store),
    ));

    server
        .handle_session_load(&serde_json::json!({
            "sessionId": session_id,
            "cwd": cwd.path().to_string_lossy()
        }))
        .await
        .expect("session/load must succeed");

    let requests = mcp
        .received_requests()
        .await
        .expect("mock records requests");
    assert!(
        requests.iter().any(|r| std::str::from_utf8(&r.body)
            .map(|b| b.contains("tools/list"))
            .unwrap_or(false)),
        "restored session must rebuild from its owning agent `finance` (acp_enable_mcp=true) \
             and load its MCP bundles, not the ACP default `test-agent`; got {} request(s)",
        requests.len()
    );
}

#[tokio::test]
async fn session_resume_restores_owning_agent_and_its_mcp_optin() {
    let cwd = tempfile::tempdir().unwrap();
    let mcp = start_mock_mcp_http_server("records.list").await;
    let config = make_cross_agent_restore_config(cwd.path(), mcp.uri());

    let store =
        Arc::new(zeroclaw_infra::acp_session_store::AcpSessionStore::new(cwd.path()).unwrap());
    let session_id = "sess-cross-agent-resume";
    store
        .create_session(session_id, "finance", &cwd.path().to_string_lossy())
        .unwrap();

    let (writer_tx, _rx) = tokio::sync::mpsc::channel::<String>(64);
    let server = Arc::new(AcpServer::new_with_writer_and_store(
        config,
        AcpServerConfig::default(),
        writer_tx,
        Arc::clone(&store),
    ));

    server
        .handle_session_resume(&serde_json::json!({
            "sessionId": session_id,
            "cwd": cwd.path().to_string_lossy()
        }))
        .await
        .expect("session/resume must succeed");

    let requests = mcp
        .received_requests()
        .await
        .expect("mock records requests");
    assert!(
        requests.iter().any(|r| std::str::from_utf8(&r.body)
            .map(|b| b.contains("tools/list"))
            .unwrap_or(false)),
        "resumed session must rebuild from its owning agent `finance` (acp_enable_mcp=true) \
             and load its MCP bundles, not the ACP default `test-agent`; got {} request(s)",
        requests.len()
    );
}

#[test]
fn turn_cancelled_notification_is_styled_tool_call() {
    let note = AcpServer::turn_cancelled_notification("sess-c");
    let update = &note.params["update"];
    assert_eq!(update["sessionUpdate"], "tool_call");
    assert_eq!(update["name"], "turn-cancelled");
    assert_eq!(update["status"], "completed");
    assert!(
        update["content"][0]["content"]["text"]
            .as_str()
            .is_some_and(|t| !t.is_empty())
    );
}

#[tokio::test]
async fn session_resume_restores_without_replay() {
    use zeroclaw_api::model_provider::{ChatMessage, ConversationMessage};
    let cwd = tempfile::tempdir().unwrap();
    let store =
        Arc::new(zeroclaw_infra::acp_session_store::AcpSessionStore::new(cwd.path()).unwrap());

    let session_id = "sess-resume-test";
    store
        .create_session(session_id, "test-agent", &cwd.path().to_string_lossy())
        .unwrap();
    store
        .append_turn(
            session_id,
            &[ConversationMessage::Chat(ChatMessage::user("hello"))],
        )
        .unwrap();

    let (writer_tx, mut writer_rx) = tokio::sync::mpsc::channel::<String>(64);
    let server = Arc::new(AcpServer::new_with_writer_and_store(
        make_test_config(cwd.path()),
        AcpServerConfig::default(),
        writer_tx,
        Arc::clone(&store),
    ));

    let result = server
        .handle_session_resume(&serde_json::json!({
            "sessionId": session_id,
            "cwd": cwd.path().to_string_lossy()
        }))
        .await
        .expect("session/resume must succeed");

    // Result is empty object
    assert_eq!(result, serde_json::json!({}));

    // Session must be in memory
    assert!(server.sessions.lock().await.contains_key(session_id));

    // A resume with no stored plan must not emit notifications (transcript
    // is seeded into the agent, not replayed to the client as updates).
    assert!(
        writer_rx.try_recv().is_err(),
        "session/resume with no plan must not emit session/update notifications"
    );
}

#[tokio::test]
async fn session_resume_replays_stored_plan() {
    use zeroclaw_api::plan::{PlanEntry, PlanPriority, PlanStatus};
    let cwd = tempfile::tempdir().unwrap();
    let store =
        Arc::new(zeroclaw_infra::acp_session_store::AcpSessionStore::new(cwd.path()).unwrap());

    let session_id = "sess-resume-plan";
    store
        .create_session(session_id, "test-agent", &cwd.path().to_string_lossy())
        .unwrap();
    // A durable plan exists from a prior turn.
    store
        .set_plan(
            session_id,
            &[PlanEntry {
                content: "Resume me".to_string(),
                status: PlanStatus::InProgress,
                priority: PlanPriority::High,
                active_form: Some("Resuming".to_string()),
            }],
        )
        .unwrap();

    let (writer_tx, mut writer_rx) = tokio::sync::mpsc::channel::<String>(64);
    let server = Arc::new(AcpServer::new_with_writer_and_store(
        make_test_config(cwd.path()),
        AcpServerConfig::default(),
        writer_tx,
        Arc::clone(&store),
    ));

    server
        .handle_session_resume(&serde_json::json!({
            "sessionId": session_id,
            "cwd": cwd.path().to_string_lossy()
        }))
        .await
        .expect("session/resume must succeed");

    // The stored plan must be replayed as a native ACP `plan` update.
    let raw = writer_rx
        .try_recv()
        .expect("resume must emit a plan session/update");
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(v["method"], "session/update");
    assert_eq!(v["params"]["update"]["sessionUpdate"], "plan");
    assert_eq!(v["params"]["update"]["entries"][0]["content"], "Resume me");
    assert_eq!(v["params"]["update"]["entries"][0]["status"], "in_progress");
}

#[tokio::test]
async fn session_close_releases_memory_but_keeps_store_record() {
    let cwd = tempfile::tempdir().unwrap();
    let store =
        Arc::new(zeroclaw_infra::acp_session_store::AcpSessionStore::new(cwd.path()).unwrap());
    let server = Arc::new(AcpServer::new_with_store(
        make_test_config(cwd.path()),
        AcpServerConfig::default(),
        Arc::clone(&store),
    ));

    let new_result = server
        .handle_session_new(&serde_json::json!({
            "cwd": cwd.path().to_string_lossy()
        }))
        .await
        .expect("session/new must succeed");
    let session_id = new_result["sessionId"].as_str().unwrap().to_string();

    assert!(server.sessions.lock().await.contains_key(&session_id));

    let result = server
        .handle_session_close(&serde_json::json!({ "sessionId": &session_id }))
        .await
        .expect("session/close must succeed");

    assert_eq!(result, serde_json::json!({}));

    // Session gone from in-memory map
    assert!(!server.sessions.lock().await.contains_key(&session_id));

    // Session record still on disk
    let data = store.load_session(&session_id).unwrap();
    assert!(
        data.is_some(),
        "session/close must not delete the DB record"
    );
}

#[tokio::test]
async fn session_close_returns_not_found_for_unknown_session() {
    let cwd = tempfile::tempdir().unwrap();
    let server = AcpServer::new(make_test_config(cwd.path()), AcpServerConfig::default());

    let err = server
        .handle_session_close(&serde_json::json!({ "sessionId": "ghost" }))
        .await
        .expect_err("unknown session must fail");

    assert_eq!(err.code, SESSION_NOT_FOUND);
}

#[tokio::test]
async fn session_new_respects_max_sessions() {
    let cwd = tempfile::tempdir().unwrap();
    let server = Arc::new(AcpServer::new(
        make_test_config(cwd.path()),
        AcpServerConfig {
            max_sessions: 1,
            ..AcpServerConfig::default()
        },
    ));

    server
        .handle_session_new(&serde_json::json!({
            "cwd": cwd.path().to_string_lossy(),
            "agentAlias": "test-agent"
        }))
        .await
        .expect("first session/new must succeed under the limit");

    let err = server
        .handle_session_new(&serde_json::json!({
            "cwd": cwd.path().to_string_lossy(),
            "agentAlias": "test-agent"
        }))
        .await
        .expect_err("second session/new must fail at max_sessions");

    assert_eq!(
        err.code, SESSION_LIMIT_REACHED,
        "expected SESSION_LIMIT_REACHED, got: {err:?}"
    );
}

#[tokio::test]
async fn session_load_respects_max_sessions() {
    let cwd = tempfile::tempdir().unwrap();
    let store =
        Arc::new(zeroclaw_infra::acp_session_store::AcpSessionStore::new(cwd.path()).unwrap());

    // Pre-create a stored session that we'll attempt to load
    let stored_id = "sess-load-limit-test";
    store
        .create_session(stored_id, "test-agent", &cwd.path().to_string_lossy())
        .unwrap();

    let (writer_tx, _rx) = tokio::sync::mpsc::channel::<String>(8);
    let server = Arc::new(AcpServer::new_with_writer_and_store(
        make_test_config(cwd.path()),
        AcpServerConfig {
            max_sessions: 1,
            ..AcpServerConfig::default()
        },
        writer_tx,
        Arc::clone(&store),
    ));

    // Fill the one available slot via session/new
    server
        .handle_session_new(&serde_json::json!({
            "cwd": cwd.path().to_string_lossy()
        }))
        .await
        .expect("session/new must succeed when under limit");

    // Now session/load for the stored session must fail with SESSION_LIMIT_REACHED
    let err = server
        .handle_session_load(&serde_json::json!({ "sessionId": stored_id }))
        .await
        .expect_err("session/load must fail when max_sessions reached");

    assert_eq!(
        err.code, SESSION_LIMIT_REACHED,
        "expected SESSION_LIMIT_REACHED, got: {:?}",
        err
    );
}

#[tokio::test]
async fn session_resume_respects_max_sessions() {
    let cwd = tempfile::tempdir().unwrap();
    let store =
        Arc::new(zeroclaw_infra::acp_session_store::AcpSessionStore::new(cwd.path()).unwrap());

    // Pre-create a stored session that we'll attempt to resume
    let stored_id = "sess-resume-limit-test";
    store
        .create_session(stored_id, "test-agent", &cwd.path().to_string_lossy())
        .unwrap();

    let (writer_tx, _rx) = tokio::sync::mpsc::channel::<String>(8);
    let server = Arc::new(AcpServer::new_with_writer_and_store(
        make_test_config(cwd.path()),
        AcpServerConfig {
            max_sessions: 1,
            ..AcpServerConfig::default()
        },
        writer_tx,
        Arc::clone(&store),
    ));

    // Fill the one available slot via session/new
    server
        .handle_session_new(&serde_json::json!({
            "cwd": cwd.path().to_string_lossy()
        }))
        .await
        .expect("session/new must succeed when under limit");

    // Now session/resume for the stored session must fail with SESSION_LIMIT_REACHED
    let err = server
        .handle_session_resume(&serde_json::json!({ "sessionId": stored_id }))
        .await
        .expect_err("session/resume must fail when max_sessions reached");

    assert_eq!(
        err.code, SESSION_LIMIT_REACHED,
        "expected SESSION_LIMIT_REACHED, got: {:?}",
        err
    );
}

#[tokio::test]
async fn session_load_releases_reservation_on_store_error() {
    let cwd = tempfile::tempdir().unwrap();
    let store =
        Arc::new(zeroclaw_infra::acp_session_store::AcpSessionStore::new(cwd.path()).unwrap());

    let session_id = "sess-load-store-err";
    store
        .create_session(session_id, "test-agent", &cwd.path().to_string_lossy())
        .unwrap();

    // Drop the schema via a second connection to force a "no such table"
    // error on the store's next query_row call.
    let db_path = cwd.path().join("sessions/acp-sessions.db");
    {
        let second = rusqlite::Connection::open(&db_path).expect("second conn must open same db");
        second
            .execute_batch("DROP TABLE IF EXISTS acp_messages; DROP TABLE IF EXISTS acp_sessions;")
            .expect("schema drop must succeed on second conn");
    }

    let (writer_tx, _rx) = tokio::sync::mpsc::channel::<String>(8);
    let server = Arc::new(AcpServer::new_with_writer_and_store(
        make_test_config(cwd.path()),
        AcpServerConfig::default(),
        writer_tx,
        Arc::clone(&store),
    ));

    // First call: must fail with INTERNAL_ERROR (SQLite "no such table").
    let first_err = server
        .handle_session_load(&serde_json::json!({ "sessionId": session_id }))
        .await
        .expect_err("session/load must fail when store returns Err");
    assert_eq!(
        first_err.code, INTERNAL_ERROR,
        "expected INTERNAL_ERROR from store failure, got: {:?}",
        first_err
    );

    // Second call for the same session: must also fail with INTERNAL_ERROR,
    // NOT with INVALID_PARAMS ("already active"). A leaked reservation would
    // cause INVALID_PARAMS, proving the slot was never released.
    let second_err = server
        .handle_session_load(&serde_json::json!({ "sessionId": session_id }))
        .await
        .expect_err("second session/load must also fail");
    assert_eq!(
        second_err.code, INTERNAL_ERROR,
        "second load must fail with INTERNAL_ERROR, not INVALID_PARAMS (leaked slot); got: {:?}",
        second_err
    );
}

#[tokio::test]
async fn session_resume_releases_reservation_on_store_error() {
    let cwd = tempfile::tempdir().unwrap();
    let store =
        Arc::new(zeroclaw_infra::acp_session_store::AcpSessionStore::new(cwd.path()).unwrap());

    let session_id = "sess-resume-store-err";
    store
        .create_session(session_id, "test-agent", &cwd.path().to_string_lossy())
        .unwrap();

    let db_path = cwd.path().join("sessions/acp-sessions.db");
    {
        let second = rusqlite::Connection::open(&db_path).expect("second conn must open same db");
        second
            .execute_batch("DROP TABLE IF EXISTS acp_messages; DROP TABLE IF EXISTS acp_sessions;")
            .expect("schema drop must succeed on second conn");
    }

    let (writer_tx, _rx) = tokio::sync::mpsc::channel::<String>(8);
    let server = Arc::new(AcpServer::new_with_writer_and_store(
        make_test_config(cwd.path()),
        AcpServerConfig::default(),
        writer_tx,
        Arc::clone(&store),
    ));

    let first_err = server
        .handle_session_resume(&serde_json::json!({ "sessionId": session_id }))
        .await
        .expect_err("session/resume must fail when store returns Err");
    assert_eq!(
        first_err.code, INTERNAL_ERROR,
        "expected INTERNAL_ERROR from store failure, got: {:?}",
        first_err
    );

    let second_err = server
        .handle_session_resume(&serde_json::json!({ "sessionId": session_id }))
        .await
        .expect_err("second session/resume must also fail");
    assert_eq!(
        second_err.code, INTERNAL_ERROR,
        "second resume must fail with INTERNAL_ERROR, not INVALID_PARAMS (leaked slot); got: {:?}",
        second_err
    );
}

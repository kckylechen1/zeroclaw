    use super::*;
    use async_trait::async_trait;
    use serde_json::json;

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn memory_embeddings_use_provider_matches_base_ref_and_routes() {
        use zeroclaw_config::schema::{Config, EmbeddingRouteConfig};

        let mut config = Config::default();
        config.memory.embedding_provider = "openai.default".into();
        config.embedding_routes = vec![EmbeddingRouteConfig {
            hint: "semantic".into(),
            model_provider: "openrouter.alt".into(),
            model: "embed".into(),
            dimensions: Some(1024),
            api_key: None,
        }];

        // Base `[memory].embedding_provider` reference.
        assert!(memory_embeddings_use_provider(&config, "openai.default"));
        // Any `[[embedding_routes]]` reference.
        assert!(memory_embeddings_use_provider(&config, "openrouter.alt"));
        // An unrelated provider must not trigger a memory-embedder refresh.
        assert!(!memory_embeddings_use_provider(
            &config,
            "anthropic.default"
        ));
    }

    #[test]
    fn agent_alias_from_model_provider_prop_matches_only_the_bound_provider_field() {
        // The config pane and other `config/set agents.<alias>.model_provider`
        // callers write this path; it must map back to the alias so the live
        // session refresh fires. The zerocode picker takes the `session/configure`
        // path instead and is not a caller here.
        assert_eq!(
            agent_alias_from_model_provider_prop("agents.fred.model_provider"),
            Some("fred".to_string())
        );
        // Any other agent field must not trigger a provider rebuild.
        assert_eq!(
            agent_alias_from_model_provider_prop("agents.fred.risk_profile"),
            None
        );
        // A provider-profile edit is handled by the other refresh path, not this one.
        assert_eq!(
            agent_alias_from_model_provider_prop("providers.models.anthropic.default.model"),
            None
        );
        // Empty alias is rejected.
        assert_eq!(
            agent_alias_from_model_provider_prop("agents..model_provider"),
            None
        );
    }

    #[test]
    fn agent_scoped_refresh_selects_only_edited_agent_without_override() {
        use crate::rpc::session::SessionOverrides;
        let no_override = SessionOverrides::default();
        let with_override = SessionOverrides {
            model_provider: Some("anthropic.other".to_string()),
            ..Default::default()
        };

        // A session bound to the edited agent with no override is rebuilt.
        assert!(agent_scoped_refresh_selects("fred", "fred", &no_override));
        // A session belonging to a different agent is never rebuilt, even
        // when it resolves to the same provider.
        assert!(!agent_scoped_refresh_selects("fred", "wilma", &no_override));
        // The edited agent's own session is left untouched when it carries a
        // `model_provider` override.
        assert!(!agent_scoped_refresh_selects(
            "fred",
            "fred",
            &with_override
        ));
        // A different agent with an override is likewise excluded.
        assert!(!agent_scoped_refresh_selects(
            "fred",
            "wilma",
            &with_override
        ));
    }

    #[test]
    fn provider_scoped_refresh_selects_inheritors_and_matching_overrides() {
        use crate::rpc::session::SessionOverrides;
        let no_override = SessionOverrides::default();
        let matching_override = SessionOverrides {
            model_provider: Some("anthropic.default".to_string()),
            ..Default::default()
        };
        let other_override = SessionOverrides {
            model_provider: Some("openai.default".to_string()),
            ..Default::default()
        };

        // No override: inherits the agent provider, so it is a candidate
        // (final config match is resolved by the caller).
        assert!(provider_scoped_refresh_selects(
            "anthropic.default",
            &no_override
        ));
        // Override that names the edited provider is a candidate.
        assert!(provider_scoped_refresh_selects(
            "anthropic.default",
            &matching_override
        ));
        // Override that names a different provider is excluded.
        assert!(!provider_scoped_refresh_selects(
            "anthropic.default",
            &other_override
        ));
    }

    #[test]
    fn session_initializes_mcp_for_chat_but_not_acp() {
        use crate::rpc::types::ChatMode;
        // Chat sessions must initialize MCP so the Zerocode TUI sees the same
        // MCP tools (and the deferred-loading `tool_search`) the gateway
        // already exposes for the agent
        assert!(
            session_should_initialize_mcp(&ChatMode::Chat),
            "Chat sessions must eagerly initialize MCP"
        );
        // ACP (Code) sessions intentionally skip eager MCP init to keep
        // `session/new` prompt.
        assert!(
            !session_should_initialize_mcp(&ChatMode::Acp),
            "ACP sessions must skip eager MCP init"
        );
    }

    /// Spin up a wiremock server that speaks the minimum MCP HTTP handshake
    /// (`initialize` → `notifications/initialized` → `tools/list`) and advertises
    /// a single tool. The dotted `tool_name` exercises spec-valid names that
    /// must survive `<server>__<tool>` prefixing
    async fn start_mock_mcp_http_server(tool_name: &str) -> wiremock::MockServer {
        use wiremock::matchers::{body_partial_json, method};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "initialize"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Mcp-Session-Id", "sess-1")
                    .set_body_json(json!({
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
                "result": {"tools": [{
                    "name": tool_name,
                    "description": "List domains",
                    "inputSchema": {"type": "object"}
                }]}
            })))
            .mount(&server)
            .await;
        server
    }

    /// `make_acp_test_config` plus an MCP server granted to `test-agent` via an
    /// `mcp_bundles` grant, pointed at `mock_uri`. `deferred` selects
    /// deferred-loading (`tool_search`) vs eager registration.
    fn make_mcp_granting_config(
        tmp: &tempfile::TempDir,
        mock_uri: String,
        deferred: bool,
    ) -> zeroclaw_config::schema::Config {
        use zeroclaw_config::schema::{McpBundleConfig, McpServerConfig, McpTransport};

        let mut config = make_acp_test_config(tmp);
        config.mcp.enabled = true;
        config.mcp.deferred_loading = deferred;
        config.mcp.servers = vec![McpServerConfig {
            name: "remote".into(),
            transport: McpTransport::Http,
            url: Some(mock_uri),
            ..Default::default()
        }];
        config.mcp_bundles.insert(
            "b1".into(),
            McpBundleConfig {
                servers: vec!["remote".into()],
                exclude: vec![],
            },
        );
        config
            .agents
            .get_mut("test-agent")
            .expect("test-agent must exist")
            .mcp_bundles = vec!["b1".into()];
        config
    }

    #[tokio::test]
    async fn chat_session_new_exposes_tool_search_in_deferred_mcp_mode() {
        let tmp = tempfile::TempDir::new().unwrap();
        let server = start_mock_mcp_http_server("domains.list").await;
        let config = make_mcp_granting_config(&tmp, server.uri(), true);
        let (dispatcher, sessions) = make_acp_test_dispatcher(config);

        let params = json!({
            "agent_alias": "test-agent",
            "chat_mode": "chat",
            "session_id": "chat-mcp-deferred-001"
        });
        let result = dispatcher.handle_session_new_for_test(&params).await;
        assert!(
            result.is_ok(),
            "session/new should succeed; got: {:?}",
            result.err()
        );

        let agent_arc = sessions
            .get_agent("chat-mcp-deferred-001")
            .await
            .expect("session must be registered after session/new");
        let agent = agent_arc.lock().await;
        let names = agent.tool_names();
        assert!(
            names.contains(&"tool_search"),
            "Chat session with deferred MCP must expose `tool_search`; tools: {names:?}"
        );
    }

    #[tokio::test]
    async fn chat_session_new_excluded_tool_search_is_dropped_in_deferred_mcp_mode() {
        let tmp = tempfile::TempDir::new().unwrap();
        let server = start_mock_mcp_http_server("domains.list").await;
        let mut config = make_mcp_granting_config(&tmp, server.uri(), true);
        // Deny the deferred-MCP discovery tool by name.
        config
            .risk_profiles
            .get_mut("test-profile")
            .expect("test-profile must exist")
            .excluded_tools = vec!["tool_search".into()];
        let (dispatcher, sessions) = make_acp_test_dispatcher(config);

        let params = json!({
            "agent_alias": "test-agent",
            "chat_mode": "chat",
            "session_id": "chat-mcp-deferred-excl-001"
        });
        let result = dispatcher.handle_session_new_for_test(&params).await;
        assert!(
            result.is_ok(),
            "session/new should succeed; got: {:?}",
            result.err()
        );

        let agent_arc = sessions
            .get_agent("chat-mcp-deferred-excl-001")
            .await
            .expect("session must be registered after session/new");
        let agent = agent_arc.lock().await;
        let names = agent.tool_names();
        assert!(
            !names.contains(&"tool_search"),
            "excluded_tools = [\"tool_search\"] must drop the deferred tool_search \
             wrapper (excluded_tools always subtracts); tools: {names:?}"
        );
        // The registry and prompt surfaces must move together: the system prompt
        // must not instruct the model to call a tool the policy just removed.
        let prompt = agent
            .system_prompt_for_test()
            .expect("system prompt must render");
        assert!(
            !prompt.contains("tool_search"),
            "excluded tool_search must not be advertised in the system prompt; prompt: {prompt}"
        );
        assert!(
            !prompt.contains("## Deferred Tools"),
            "excluded tool_search must suppress the deferred-tools section entirely; prompt: {prompt}"
        );
        assert!(
            !prompt.contains("remote__domains.list"),
            "excluded tool_search must not leak the deferred stub it would have activated; prompt: {prompt}"
        );
    }

    #[tokio::test]
    async fn chat_session_new_advertises_deferred_mcp_section_in_system_prompt() {
        let tmp = tempfile::TempDir::new().unwrap();
        let server = start_mock_mcp_http_server("domains.list").await;
        let config = make_mcp_granting_config(&tmp, server.uri(), true);
        let (dispatcher, sessions) = make_acp_test_dispatcher(config);

        let params = json!({
            "agent_alias": "test-agent",
            "chat_mode": "chat",
            "session_id": "chat-mcp-deferred-prompt-001"
        });
        let result = dispatcher.handle_session_new_for_test(&params).await;
        assert!(
            result.is_ok(),
            "session/new should succeed; got: {:?}",
            result.err()
        );

        let agent_arc = sessions
            .get_agent("chat-mcp-deferred-prompt-001")
            .await
            .expect("session must be registered after session/new");
        let agent = agent_arc.lock().await;
        let prompt = agent
            .system_prompt_for_test()
            .expect("system prompt must render");
        assert!(
            prompt.contains("## Deferred Tools"),
            "system prompt must include the deferred-tools section; prompt: {prompt}"
        );
        assert!(
            prompt.contains("tool_search"),
            "system prompt must instruct the model to call `tool_search`; prompt: {prompt}"
        );
        assert!(
            prompt.contains("remote__domains.list"),
            "system prompt must advertise the dotted `<server>__<tool>` stub; prompt: {prompt}"
        );
    }

    #[tokio::test]
    async fn chat_session_new_tool_search_returns_granted_mcp_tool_in_deferred_mode() {
        let tmp = tempfile::TempDir::new().unwrap();
        let server = start_mock_mcp_http_server("domains.list").await;
        let config = make_mcp_granting_config(&tmp, server.uri(), true);
        let (dispatcher, sessions) = make_acp_test_dispatcher(config);

        let params = json!({
            "agent_alias": "test-agent",
            "chat_mode": "chat",
            "session_id": "chat-mcp-deferred-search-001"
        });
        let result = dispatcher.handle_session_new_for_test(&params).await;
        assert!(
            result.is_ok(),
            "session/new should succeed; got: {:?}",
            result.err()
        );

        let agent_arc = sessions
            .get_agent("chat-mcp-deferred-search-001")
            .await
            .expect("session must be registered after session/new");
        let agent = agent_arc.lock().await;

        let tool_result = agent
            .execute_tool_for_test("tool_search", json!({ "query": "domains" }))
            .await
            .expect("deferred Chat session must expose `tool_search`")
            .expect("tool_search must execute without error");

        assert!(
            tool_result.success,
            "tool_search should succeed; error: {:?}",
            tool_result.error
        );
        assert!(
            tool_result.output.contains("remote__domains.list"),
            "tool_search must resolve the granted `<server>__<tool>` stub, not just \
             be present; output: {}",
            tool_result.output
        );
    }

    #[tokio::test]
    async fn chat_session_new_exposes_prefixed_mcp_tool_in_eager_mode() {
        let tmp = tempfile::TempDir::new().unwrap();
        let server = start_mock_mcp_http_server("domains.list").await;
        let config = make_mcp_granting_config(&tmp, server.uri(), false);
        let (dispatcher, sessions) = make_acp_test_dispatcher(config);

        let params = json!({
            "agent_alias": "test-agent",
            "chat_mode": "chat",
            "session_id": "chat-mcp-eager-001"
        });
        let result = dispatcher.handle_session_new_for_test(&params).await;
        assert!(
            result.is_ok(),
            "session/new should succeed; got: {:?}",
            result.err()
        );

        let agent_arc = sessions
            .get_agent("chat-mcp-eager-001")
            .await
            .expect("session must be registered after session/new");
        let agent = agent_arc.lock().await;
        let names = agent.tool_names();
        // Eager mode registers the MCP tool directly; the dotted suffix keeps
        // its `<server>__<tool>` prefix.
        assert!(
            names.contains(&"remote__domains.list"),
            "Chat session with eager MCP must expose `remote__domains.list`; tools: {names:?}"
        );
    }

    #[tokio::test]
    async fn chat_session_new_omits_mcp_tools_when_agent_has_no_bundles_deferred() {
        use zeroclaw_config::schema::AliasedAgentConfig;

        let tmp = tempfile::TempDir::new().unwrap();
        let server = start_mock_mcp_http_server("domains.list").await;
        let mut config = make_mcp_granting_config(&tmp, server.uri(), true);

        // Add a SECOND agent with no `mcp_bundles`. Reuse `test-agent`'s
        // model_provider/risk_profile so the agent is fully constructible
        // without touching providers/risk_profiles.
        let template = config
            .agents
            .get("test-agent")
            .cloned()
            .expect("test-agent must exist in make_mcp_granting_config");
        config.agents.insert(
            "unscoped-agent".to_string(),
            AliasedAgentConfig {
                enabled: true,
                model_provider: template.model_provider.clone(),
                risk_profile: template.risk_profile.clone(),
                mcp_bundles: Vec::new(), // explicit: no grant
                ..AliasedAgentConfig::default()
            },
        );

        let (dispatcher, sessions) = make_acp_test_dispatcher(config);

        let params = json!({
            "agent_alias": "unscoped-agent",
            "chat_mode": "chat",
            "session_id": "chat-mcp-unscoped-deferred-001"
        });
        let result = dispatcher.handle_session_new_for_test(&params).await;
        assert!(
            result.is_ok(),
            "session/new for an unscoped agent should still succeed; got: {:?}",
            result.err()
        );

        let agent_arc = sessions
            .get_agent("chat-mcp-unscoped-deferred-001")
            .await
            .expect("session must be registered after session/new");
        let agent = agent_arc.lock().await;
        let names = agent.tool_names();
        assert!(
            !names.contains(&"tool_search"),
            "Unscoped agent must NOT expose `tool_search` in deferred mode \
             (mcp_bundles is empty -> no MCP connection -> no deferred \
             registry -> no tool_search). Tools were: {names:?}"
        );
        // And, defensively, no prefixed MCP tool either.
        assert!(
            !names.iter().any(|n| n.contains("__")),
            "Unscoped agent must expose zero `<server>__<tool>` MCP tools; \
             tools were: {names:?}"
        );
    }

    #[tokio::test]
    async fn chat_session_new_omits_mcp_tools_when_agent_has_no_bundles_eager() {
        use zeroclaw_config::schema::AliasedAgentConfig;

        let tmp = tempfile::TempDir::new().unwrap();
        let server = start_mock_mcp_http_server("domains.list").await;
        let mut config = make_mcp_granting_config(&tmp, server.uri(), false);

        let template = config
            .agents
            .get("test-agent")
            .cloned()
            .expect("test-agent must exist in make_mcp_granting_config");
        config.agents.insert(
            "unscoped-agent".to_string(),
            AliasedAgentConfig {
                enabled: true,
                model_provider: template.model_provider.clone(),
                risk_profile: template.risk_profile.clone(),
                mcp_bundles: Vec::new(),
                ..AliasedAgentConfig::default()
            },
        );

        let (dispatcher, sessions) = make_acp_test_dispatcher(config);

        let params = json!({
            "agent_alias": "unscoped-agent",
            "chat_mode": "chat",
            "session_id": "chat-mcp-unscoped-eager-001"
        });
        let result = dispatcher.handle_session_new_for_test(&params).await;
        assert!(
            result.is_ok(),
            "session/new for an unscoped agent should still succeed; got: {:?}",
            result.err()
        );

        let agent_arc = sessions
            .get_agent("chat-mcp-unscoped-eager-001")
            .await
            .expect("session must be registered after session/new");
        let agent = agent_arc.lock().await;
        let names = agent.tool_names();
        assert!(
            !names.contains(&"remote__domains.list"),
            "Unscoped agent must NOT expose `remote__domains.list` in \
             eager mode (mcp_bundles is empty -> no MCP connection -> \
             no eager registration). Tools were: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.starts_with("remote__")),
            "No `remote__*` tool may leak to an unscoped agent; tools \
             were: {names:?}"
        );
    }

    #[tokio::test]
    async fn acp_session_new_skips_mcp_tools() {
        let tmp = tempfile::TempDir::new().unwrap();
        let server = start_mock_mcp_http_server("domains.list").await;
        // Deferred mode would register `tool_search` for a Chat session; an ACP
        // session must skip MCP init entirely regardless. ACP `session/new`
        // requires the persistence dispatcher (it touches the ACP store).
        let config = make_mcp_granting_config(&tmp, server.uri(), true);
        let data_dir = config.data_dir.clone();
        let (dispatcher, sessions, _chat_backend, _acp_store) =
            make_persistence_test_dispatcher(config, &data_dir);

        let params = json!({
            "agent_alias": "test-agent",
            "chat_mode": "acp",
            "session_id": "acp-mcp-001"
        });
        let result = dispatcher.handle_session_new_for_test(&params).await;
        assert!(
            result.is_ok(),
            "session/new should succeed; got: {:?}",
            result.err()
        );

        let agent_arc = sessions
            .get_agent("acp-mcp-001")
            .await
            .expect("session must be registered after session/new");
        let agent = agent_arc.lock().await;
        let names = agent.tool_names();
        assert!(
            !names.contains(&"tool_search") && !names.contains(&"remote__domains.list"),
            "ACP session must skip MCP init (no `tool_search`, no MCP tools); tools: {names:?}"
        );
    }

    /// Blocking regression: a fresh remote RPC client that reaches
    /// `logs/subscribe` (an unauthenticated surface — a new WSS client can
    /// `initialize` and subscribe without the gateway bearer) must never
    /// receive a pairing credential off the shared broadcast bus, while
    /// ordinary log frames still forward with the internal marker stripped.
    #[tokio::test]
    async fn logs_subscribe_fails_closed_on_pairing_credentials() {
        use zeroclaw_infra::session_queue::SessionActorQueue;
        let queue = Arc::new(SessionActorQueue::new(4, 10, 60));
        let sessions = Arc::new(crate::rpc::session::SessionStore::new(16, queue));
        let config = zeroclaw_config::schema::Config::default();
        let (event_tx, _rx0) = tokio::sync::broadcast::channel(16);
        let ctx = RpcContext::minimal_with_event_tx(config, sessions, event_tx.clone());
        let (writer_tx, mut writer_rx) = tokio::sync::mpsc::channel::<String>(64);
        let d = RpcDispatcher::new(ctx, writer_tx, "remote:wss=1,uid=anon".into());

        assert!(
            d.handle_logs_subscribe().await.is_ok(),
            "a fresh client should be able to subscribe"
        );

        // Marker-stamped credential frame (as `record_event` stamps a QR login
        // event) followed by an ordinary lifecycle frame.
        let credential = serde_json::json!({
            "source": "observability",
            "attributes": { "login": { "state": "qr", "qr_payload": "SECRET-QR-PAYLOAD" } },
            zeroclaw_log::EPHEMERAL_BROADCAST_MARKER: true,
        });
        let plain = serde_json::json!({
            "source": "observability",
            "type": "tool_call",
            "tool": "SENTINEL-LIVE",
        });
        event_tx.send(credential).expect("send credential frame");
        event_tx.send(plain).expect("send plain frame");

        // Collect forwarded notifications until the sentinel arrives or the
        // budget elapses.
        let mut seen = String::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, writer_rx.recv()).await {
                Ok(Some(msg)) => {
                    let hit = msg.contains("SENTINEL-LIVE");
                    seen.push_str(&msg);
                    if hit {
                        break;
                    }
                }
                _ => break,
            }
        }

        assert!(
            seen.contains("SENTINEL-LIVE"),
            "an ordinary lifecycle frame must still forward over logs/subscribe: {seen:?}"
        );
        assert!(
            !seen.contains("SECRET-QR-PAYLOAD"),
            "a remote RPC client must never obtain a pairing credential via logs/subscribe: {seen:?}"
        );
        assert!(
            !seen.contains(zeroclaw_log::EPHEMERAL_BROADCAST_MARKER),
            "the internal fail-closed marker must be stripped from forwarded frames: {seen:?}"
        );
    }

    fn make_cost_query_test_dispatcher(data_dir: &std::path::Path) -> RpcDispatcher {
        use zeroclaw_infra::session_queue::SessionActorQueue;
        let queue = Arc::new(SessionActorQueue::new(4, 10, 60));
        let sessions = Arc::new(crate::rpc::session::SessionStore::new(16, queue));
        let tracker = Arc::new(
            zeroclaw_config::cost::tracker::CostTracker::new(
                zeroclaw_config::schema::CostConfig {
                    enabled: true,
                    ..Default::default()
                },
                data_dir,
            )
            .unwrap(),
        );
        let config = zeroclaw_config::schema::Config {
            data_dir: data_dir.to_path_buf(),
            ..Default::default()
        };
        let ctx = RpcContext::minimal_with_cost_tracker(config, sessions, tracker);
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        RpcDispatcher::new(ctx, tx, "test-peer-costquery:pid=1".into())
    }

    #[test]
    fn cost_query_invalid_rfc3339_bound_is_invalid_params() {
        let tmp = tempfile::TempDir::new().unwrap();
        let d = make_cost_query_test_dispatcher(tmp.path());
        let err = d
            .handle_cost_query(&serde_json::json!({ "from": "not-a-real-date" }))
            .expect_err("an invalid RFC3339 bound must be rejected");
        assert_eq!(err.code, INVALID_PARAMS);
        assert!(err.message.contains("invalid date"), "got: {}", err.message);
    }

    #[test]
    fn cost_query_valid_bounds_reach_in_bounds_summary() {
        let tmp = tempfile::TempDir::new().unwrap();
        let d = make_cost_query_test_dispatcher(tmp.path());
        let res = d.handle_cost_query(&serde_json::json!({
            "from": "2026-01-01T00:00:00Z",
            "to": "2026-07-01T00:00:00Z"
        }));
        assert!(
            res.is_ok(),
            "a valid bounded cost/query must reach get_summary_in_bounds: {res:?}"
        );
    }

    fn make_cost_test_dispatcher(data_dir: &std::path::Path) -> RpcDispatcher {
        use zeroclaw_infra::session_queue::SessionActorQueue;
        let queue = Arc::new(SessionActorQueue::new(4, 10, 60));
        let sessions = Arc::new(crate::rpc::session::SessionStore::new(16, queue));
        let config = zeroclaw_config::schema::Config {
            data_dir: data_dir.to_path_buf(),
            ..Default::default()
        };
        let ctx = RpcContext::minimal(config, sessions);
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        RpcDispatcher::new(ctx, tx, "test-peer-cost:pid=1".into())
    }

    // cost/org: null only for a genuinely-absent snapshot; any other read failure
    // (unreadable file, a directory at the path, bad JSON) surfaces as an error so a
    // broken deployment is not mistaken for a vanilla one. (Audacity88/JordanTheJet,)
    #[test]
    fn cost_org_absent_returns_null() {
        let tmp = tempfile::TempDir::new().unwrap();
        let d = make_cost_test_dispatcher(tmp.path());
        assert_eq!(d.handle_cost_org().unwrap(), serde_json::Value::Null);
    }

    #[test]
    fn cost_org_present_returns_snapshot_verbatim() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("org_cost.json"),
            r#"{"org":"acme","billed_usd":12.5}"#,
        )
        .unwrap();
        let d = make_cost_test_dispatcher(tmp.path());
        let v = d.handle_cost_org().unwrap();
        assert_eq!(v["org"], serde_json::json!("acme"));
        assert_eq!(v["billed_usd"], serde_json::json!(12.5));
    }

    #[test]
    fn cost_org_invalid_json_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("org_cost.json"), "not valid json{").unwrap();
        let d = make_cost_test_dispatcher(tmp.path());
        assert!(d.handle_cost_org().is_err());
    }

    // The `sops/trigger-sources` RPC response must carry the full
    // ordered `SopTriggerSource` walk so authoring surfaces (web + zerocode)
    // render the picker from the backend list instead of reconstructing it.
    // Any new trigger source variant appears here automatically; a surface that
    // reconstructs its own list would drift while this guard would not, so the
    // contract is pinned at the transport boundary every surface reads from.
    #[test]
    fn sops_trigger_sources_rpc_carries_full_trigger_source_walk() {
        use crate::sop::types::SopTriggerSource;
        use strum::IntoEnumIterator;

        let tmp = tempfile::TempDir::new().unwrap();
        let d = make_cost_test_dispatcher(tmp.path());
        let value = d
            .handle_sops_trigger_sources()
            .expect("sops/trigger-sources must succeed on a default config");
        let sources: Vec<String> = value
            .get("sources")
            .and_then(|s| serde_json::from_value(s.clone()).ok())
            .expect("response must carry a `sources` array");
        let expected: Vec<String> = SopTriggerSource::iter().map(|s| s.to_string()).collect();
        assert_eq!(
            sources, expected,
            "RPC `sources` must equal the complete SopTriggerSource walk so \
             surfaces cannot drift by reconstructing their own list"
        );
    }

    #[tokio::test]
    async fn sops_run_rejects_malformed_payload_before_engine() {
        // Payload validation runs before the engine lookup, so a malformed JSON
        // string is rejected with INVALID_PARAMS even on a dispatcher with no
        // SOP engine wired. This pins the "surface a clear error on malformed
        // JSON rather than failing the run opaquely" contract.
        let tmp = tempfile::TempDir::new().unwrap();
        let d = make_cost_test_dispatcher(tmp.path());
        let err = d
            .handle_sops_run(&serde_json::json!({ "name": "any", "payload": "{not json" }))
            .await
            .expect_err("malformed payload must be rejected");
        assert_eq!(err.code, INVALID_PARAMS);
    }

    #[tokio::test]
    async fn sops_run_requires_engine() {
        // A well-formed request against a dispatcher with no SOP engine reports
        // the subsystem as unavailable rather than panicking or silently
        // succeeding.
        let tmp = tempfile::TempDir::new().unwrap();
        let d = make_cost_test_dispatcher(tmp.path());
        let err = d
            .handle_sops_run(&serde_json::json!({ "name": "any", "payload": "{\"k\":1}" }))
            .await
            .expect_err("missing engine must error");
        assert_eq!(err.code, INTERNAL_ERROR);
    }

    #[test]
    fn sops_runs_requires_engine() {
        // Listing runs against a dispatcher with no SOP engine reports the
        // subsystem as unavailable rather than returning a bogus empty list.
        let tmp = tempfile::TempDir::new().unwrap();
        let d = make_cost_test_dispatcher(tmp.path());
        let err = d
            .handle_sops_runs(&serde_json::json!({}))
            .expect_err("missing engine must error");
        assert_eq!(err.code, INTERNAL_ERROR);
    }

    #[test]
    fn sops_runs_accepts_optional_sop_filter() {
        // The request parses with or without the `sop` field; both fail only on
        // the engine guard, not on param parsing.
        let tmp = tempfile::TempDir::new().unwrap();
        let d = make_cost_test_dispatcher(tmp.path());
        let err = d
            .handle_sops_runs(&serde_json::json!({ "sop": "some-sop" }))
            .expect_err("missing engine must error");
        assert_eq!(err.code, INTERNAL_ERROR);
    }

    fn make_checkpoint_rpc_dispatcher(
        quorum: u32,
        members: &[&str],
        tui_id: &str,
    ) -> (
        RpcDispatcher,
        Arc<std::sync::Mutex<crate::sop::SopEngine>>,
        String,
        tempfile::TempDir,
    ) {
        use crate::sop::types::{
            Sop, SopAdmissionPolicy, SopEvent, SopExecutionMode, SopPriority, SopRunAction,
            SopStep, SopStepKind, SopTrigger, SopTriggerSource,
        };
        use std::collections::HashMap;
        use zeroclaw_config::schema::{
            ApprovalGroupConfig, ApprovalPolicyConfig, Config, SopApprovalConfig,
        };
        use zeroclaw_infra::session_queue::SessionActorQueue;

        let temp = tempfile::TempDir::new().unwrap();
        let sops_dir = temp.path().join("sops");
        let sop = Sop {
            name: "rpc-checkpoint".into(),
            description: "checkpoint RPC authorization test".into(),
            version: "1.0.0".into(),
            priority: SopPriority::Normal,
            execution_mode: SopExecutionMode::Deterministic,
            triggers: vec![SopTrigger::Manual],
            steps: vec![
                SopStep {
                    number: 1,
                    title: "authorize".into(),
                    kind: SopStepKind::Checkpoint,
                    policy: Some("prod".into()),
                    ..SopStep::default()
                },
                SopStep {
                    number: 2,
                    title: "continue".into(),
                    kind: SopStepKind::Execute,
                    ..SopStep::default()
                },
            ],
            cooldown_secs: 0,
            max_concurrent: 1,
            location: None,
            deterministic: false,
            admission_policy: SopAdmissionPolicy::Parallel,
            max_pending_approvals: 0,
            agent: None,
        };
        crate::sop::save_sop(&sops_dir, &sop).unwrap();
        let mut groups = HashMap::new();
        groups.insert(
            "release".to_string(),
            ApprovalGroupConfig {
                members: members.iter().map(|member| (*member).to_string()).collect(),
            },
        );
        let mut policies = HashMap::new();
        policies.insert(
            "prod".to_string(),
            ApprovalPolicyConfig {
                required_group: Some("release".into()),
                quorum,
                request_route: None,
                escalation_route: None,
            },
        );
        let mut config = Config::default();
        config.sop.sops_dir = Some(sops_dir.to_string_lossy().into_owned());
        config.sop.approval = SopApprovalConfig { groups, policies };

        let mut engine = crate::sop::SopEngine::new(config.sop.clone())
            .with_approval_broker(Arc::new(crate::sop::approval::ApprovalBroker::disabled()));
        engine.set_sops_for_test(vec![sop]);
        let action = engine
            .start_run(
                "rpc-checkpoint",
                SopEvent {
                    source: SopTriggerSource::Manual,
                    topic: None,
                    payload: None,
                    timestamp: crate::sop::engine::now_iso8601(),
                },
            )
            .unwrap();
        let run_id = match action {
            SopRunAction::CheckpointWait { run_id, .. } => run_id,
            other => panic!("expected checkpoint wait, got {other:?}"),
        };
        let engine = Arc::new(std::sync::Mutex::new(engine));
        let sessions = Arc::new(crate::rpc::session::SessionStore::new(
            16,
            Arc::new(SessionActorQueue::new(4, 10, 60)),
        ));
        let ctx = RpcContext::minimal_with_sop_engine(config, sessions, Arc::clone(&engine));
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let mut dispatcher = RpcDispatcher::new(ctx, tx, "local:test".into());
        dispatcher.set_tui_id_for_test(Some(tui_id.to_string()));
        (dispatcher, engine, run_id, temp)
    }

    #[tokio::test]
    async fn sops_decide_rpc_enforces_checkpoint_membership_and_quorum() {
        use crate::sop::types::SopRunStatus;

        let (unauthorized, engine, run_id, _temp) =
            make_checkpoint_rpc_dispatcher(1, &["cli:ZeroClawOperator"], "ZeroClawAgent");
        let error = unauthorized
            .handle_sops_decide(&json!({
                "name": "rpc-checkpoint",
                "run_id": run_id.clone(),
                "decision": "approve",
            }))
            .await
            .expect_err("unauthorized RPC principal must be rejected");
        assert_eq!(error.code, AUTH_REQUIRED);
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .get_run(&run_id)
                .map(|run| run.status),
            Some(SopRunStatus::PausedCheckpoint)
        );

        let (pending, engine, run_id, _temp) = make_checkpoint_rpc_dispatcher(
            2,
            &["cli:ZeroClawOperator", "cli:ZeroClawMaintainer"],
            "ZeroClawOperator",
        );
        pending
            .handle_sops_decide(&json!({
                "name": "rpc-checkpoint",
                "run_id": run_id.clone(),
                "decision": "approve",
            }))
            .await
            .expect("an authorized first vote returns the still-parked overlay");
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .get_run(&run_id)
                .map(|run| run.status),
            Some(SopRunStatus::PausedCheckpoint)
        );
    }

    #[tokio::test]
    async fn sops_decide_drives_resumed_execute_step() {
        use crate::sop::{
            Sop, SopEvent, SopExecutionMode, SopPriority, SopRunAction, SopRunStatus, SopStep,
            SopStepKind, SopTrigger, SopTriggerSource,
        };
        use std::sync::{Arc, Mutex};
        use zeroclaw_config::schema::{Config, SopConfig};
        use zeroclaw_infra::session_queue::SessionActorQueue;

        let tmp = tempfile::TempDir::new().expect("temporary SOP directory");
        let sops_dir = tmp.path().join("sops");
        let sop_config = SopConfig {
            sops_dir: Some(sops_dir.to_string_lossy().into_owned()),
            ..SopConfig::default()
        };
        let config = Config {
            data_dir: tmp.path().join("data"),
            config_path: tmp.path().join("config.toml"),
            sop: sop_config.clone(),
            ..Config::default()
        };
        let sop = Sop {
            name: "rpc-resumed-execute".to_string(),
            description: "RPC resume driver regression".to_string(),
            version: "1.0.0".to_string(),
            priority: SopPriority::Normal,
            execution_mode: SopExecutionMode::Supervised,
            triggers: vec![SopTrigger::Manual],
            steps: vec![SopStep {
                number: 1,
                title: "Execute after approval".to_string(),
                kind: SopStepKind::Execute,
                ..SopStep::default()
            }],
            cooldown_secs: 0,
            max_concurrent: 1,
            location: None,
            deterministic: false,
            agent: None,
            admission_policy: crate::sop::types::SopAdmissionPolicy::Parallel,
            max_pending_approvals: 0,
        };
        crate::sop::save_sop(&sops_dir, &sop).expect("save temporary SOP");

        let mut engine = crate::sop::SopEngine::new(sop_config);
        engine.reload(tmp.path());
        let engine = Arc::new(Mutex::new(engine));
        let run_id = {
            let mut guard = engine.lock().expect("engine lock");
            let action = guard
                .start_run(
                    "rpc-resumed-execute",
                    SopEvent {
                        source: SopTriggerSource::Manual,
                        topic: None,
                        payload: None,
                        timestamp: crate::sop::engine::now_iso8601(),
                    },
                )
                .expect("start approval-gated SOP");
            let SopRunAction::WaitApproval { run_id, .. } = action else {
                panic!("supervised Execute step must park for approval: {action:?}");
            };
            run_id
        };

        let queue = Arc::new(SessionActorQueue::new(4, 10, 60));
        let sessions = Arc::new(crate::rpc::session::SessionStore::new(16, queue));
        let ctx = RpcContext::minimal_with_sop_engine(config, sessions, Arc::clone(&engine));
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let dispatcher = RpcDispatcher::new(ctx, tx, "test-peer-rpc:pid=1".to_string());

        dispatcher
            .handle_sops_decide(&serde_json::json!({
                "name": "rpc-resumed-execute",
                "run_id": run_id,
                "decision": "approve",
            }))
            .await
            .expect("RPC approval must accept the parked run");

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let status = engine
                    .lock()
                    .expect("engine lock")
                    .get_run(&run_id)
                    .map(|run| run.status);
                if status == Some(SopRunStatus::Failed) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("RPC approval must schedule the resumed ExecuteStep");
    }

    #[tokio::test]
    async fn sops_decide_rejects_approval_mode_rejection() {
        use crate::sop::{
            Sop, SopEvent, SopExecutionMode, SopPriority, SopRunAction, SopRunStatus, SopStep,
            SopStepKind, SopTrigger, SopTriggerSource,
        };
        use std::sync::{Arc, Mutex};
        use zeroclaw_config::schema::{ApprovalMode, Config, SopConfig};
        use zeroclaw_infra::session_queue::SessionActorQueue;

        fn dispatcher_with_sop_engine(
            config: Config,
            engine: Arc<Mutex<crate::sop::SopEngine>>,
        ) -> RpcDispatcher {
            let queue = Arc::new(SessionActorQueue::new(4, 10, 60));
            let sessions = Arc::new(crate::rpc::session::SessionStore::new(16, queue));
            let ctx = RpcContext::minimal_with_sop_engine(config, sessions, engine);
            let (tx, _rx) = tokio::sync::mpsc::channel(64);
            let mut dispatcher = RpcDispatcher::new(ctx, tx, "test-peer-rpc:pid=1".to_string());
            dispatcher.set_tui_id_for_test(Some("alice".to_string()));
            dispatcher
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let sops_dir = tmp.path().join("sops");
        let sop_config = SopConfig {
            sops_dir: Some(sops_dir.to_string_lossy().into_owned()),
            default_execution_mode: "deterministic".to_string(),
            approval_mode: ApprovalMode::AgentTool,
            ..SopConfig::default()
        };
        let config = Config {
            data_dir: tmp.path().join("data"),
            config_path: tmp.path().join("config.toml"),
            sop: sop_config.clone(),
            ..Config::default()
        };

        let sop = Sop {
            name: "rpc-agent-tool-only".to_string(),
            description: "rpc approval-mode checkpoint".to_string(),
            version: "1.0.0".to_string(),
            priority: SopPriority::Normal,
            execution_mode: SopExecutionMode::Deterministic,
            triggers: vec![SopTrigger::Manual],
            steps: vec![SopStep {
                number: 1,
                title: "Policy gate".to_string(),
                kind: SopStepKind::Checkpoint,
                ..SopStep::default()
            }],
            cooldown_secs: 0,
            max_concurrent: 1,
            location: None,
            deterministic: true,
            agent: None,
            admission_policy: crate::sop::types::SopAdmissionPolicy::Parallel,
            max_pending_approvals: 0,
        };
        crate::sop::save_sop(&sops_dir, &sop).expect("save temp SOP");

        let mut engine = crate::sop::SopEngine::new(sop_config);
        engine.reload(tmp.path());
        let engine = Arc::new(Mutex::new(engine));
        let run_id = {
            let mut guard = engine.lock().expect("engine lock");
            let action = guard
                .start_run(
                    "rpc-agent-tool-only",
                    SopEvent {
                        source: SopTriggerSource::Manual,
                        topic: None,
                        payload: None,
                        timestamp: crate::sop::engine::now_iso8601(),
                    },
                )
                .expect("start approval-mode SOP");
            let SopRunAction::CheckpointWait { run_id, .. } = action else {
                panic!("approval-mode SOP must park at checkpoint, got {action:?}");
            };
            run_id
        };

        let dispatcher = dispatcher_with_sop_engine(config, Arc::clone(&engine));
        let err = dispatcher
            .handle_sops_decide(&serde_json::json!({
                "name": "rpc-agent-tool-only",
                "run_id": run_id,
                "decision": "approve",
            }))
            .await
            .expect_err("RPC principal must be rejected by approval_mode=agent_tool");
        assert_eq!(err.code, AUTH_REQUIRED);
        assert!(
            err.message.contains(&crate::i18n::get_required_cli_string(
                "sop-rpc-decision-unauthorized",
            )),
            "approval_mode rejection must surface, got: {}",
            err.message
        );
        let guard = engine.lock().expect("engine lock");
        assert_eq!(
            guard.get_run(&run_id).expect("run still active").status,
            SopRunStatus::PausedCheckpoint
        );
        assert!(
            !guard
                .run_events(&run_id)
                .unwrap_or_default()
                .iter()
                .any(|event| event.kind == "gate_resolved"),
            "rejected RPC decision must not append a gate_resolved row"
        );
    }

    #[tokio::test]
    async fn sops_decide_rejects_run_id_from_different_sop_before_broker_resolution() {
        use crate::sop::{
            Sop, SopEvent, SopExecutionMode, SopPriority, SopRunAction, SopRunStatus, SopStep,
            SopStepKind, SopTrigger, SopTriggerSource,
        };
        use std::sync::{Arc, Mutex};
        use zeroclaw_config::schema::{Config, SopConfig};
        use zeroclaw_infra::session_queue::SessionActorQueue;

        fn dispatcher_with_sop_engine(
            config: Config,
            engine: Arc<Mutex<crate::sop::SopEngine>>,
        ) -> RpcDispatcher {
            let queue = Arc::new(SessionActorQueue::new(4, 10, 60));
            let sessions = Arc::new(crate::rpc::session::SessionStore::new(16, queue));
            let ctx = RpcContext::minimal_with_sop_engine(config, sessions, engine);
            let (tx, _rx) = tokio::sync::mpsc::channel(64);
            let mut dispatcher = RpcDispatcher::new(ctx, tx, "test-peer-rpc:pid=1".to_string());
            dispatcher.set_tui_id_for_test(Some("alice".to_string()));
            dispatcher
        }

        fn checkpoint_sop(name: &str) -> Sop {
            Sop {
                name: name.to_string(),
                description: format!("{name} checkpoint"),
                version: "1.0.0".to_string(),
                priority: SopPriority::Normal,
                execution_mode: SopExecutionMode::Deterministic,
                triggers: vec![SopTrigger::Manual],
                steps: vec![SopStep {
                    number: 1,
                    title: "Gate".to_string(),
                    kind: SopStepKind::Checkpoint,
                    ..SopStep::default()
                }],
                cooldown_secs: 0,
                max_concurrent: 1,
                location: None,
                deterministic: true,
                agent: None,
                admission_policy: crate::sop::types::SopAdmissionPolicy::Parallel,
                max_pending_approvals: 0,
            }
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let sops_dir = tmp.path().join("sops");
        let sop_config = SopConfig {
            sops_dir: Some(sops_dir.to_string_lossy().into_owned()),
            default_execution_mode: "deterministic".to_string(),
            ..SopConfig::default()
        };
        let config = Config {
            data_dir: tmp.path().join("data"),
            config_path: tmp.path().join("config.toml"),
            sop: sop_config.clone(),
            ..Config::default()
        };

        crate::sop::save_sop(&sops_dir, &checkpoint_sop("rpc-a")).expect("save rpc-a");
        crate::sop::save_sop(&sops_dir, &checkpoint_sop("rpc-b")).expect("save rpc-b");

        let mut engine = crate::sop::SopEngine::new(sop_config);
        engine.reload(tmp.path());
        assert_eq!(engine.sops().len(), 2, "both temp SOPs should load");
        let engine = Arc::new(Mutex::new(engine));

        let run_id = {
            let mut guard = engine.lock().expect("engine lock");
            let action = guard
                .start_run(
                    "rpc-b",
                    SopEvent {
                        source: SopTriggerSource::Manual,
                        topic: None,
                        payload: None,
                        timestamp: crate::sop::engine::now_iso8601(),
                    },
                )
                .expect("start rpc-b SOP");
            let SopRunAction::CheckpointWait { run_id, .. } = action else {
                panic!("rpc-b must park at checkpoint, got {action:?}");
            };
            run_id
        };

        let dispatcher = dispatcher_with_sop_engine(config, Arc::clone(&engine));
        let err = dispatcher
            .handle_sops_decide(&serde_json::json!({
                "name": "rpc-a",
                "run_id": run_id,
                "decision": "approve",
            }))
            .await
            .expect_err("mismatched name/run_id must be rejected before broker resolution");
        assert_eq!(err.code, INVALID_PARAMS);
        assert!(
            err.message.contains("belongs to SOP 'rpc-b', not 'rpc-a'"),
            "mismatch rejection must name both SOPs, got: {}",
            err.message
        );

        let guard = engine.lock().expect("engine lock");
        let run = guard.get_run(&run_id).expect("rpc-b run still active");
        assert_eq!(run.sop_name, "rpc-b");
        assert_eq!(run.status, SopRunStatus::PausedCheckpoint);
        assert!(
            !guard
                .run_events(&run_id)
                .unwrap_or_default()
                .iter()
                .any(|event| event.kind == "gate_resolved"),
            "mismatched RPC decision must not append a gate_resolved row"
        );
    }

    #[test]
    fn cost_org_unreadable_non_notfound_errors() {
        // A directory at the snapshot path produces a non-NotFound read error; it must
        // surface as an RPC error, not masquerade as "no snapshot configured".
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("org_cost.json")).unwrap();
        let d = make_cost_test_dispatcher(tmp.path());
        assert!(
            d.handle_cost_org().is_err(),
            "an unreadable snapshot must not be reported as absent"
        );
    }

    fn make_approval_test_dispatcher() -> RpcDispatcher {
        use zeroclaw_infra::session_queue::SessionActorQueue;
        let queue = Arc::new(SessionActorQueue::new(4, 10, 60));
        let sessions = Arc::new(crate::rpc::session::SessionStore::new(16, queue));
        let ctx = RpcContext::minimal(zeroclaw_config::schema::Config::default(), sessions);
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        RpcDispatcher::new(ctx, tx, "test-peer-approval:pid=1".into())
    }

    #[test]
    fn method_from_wire_roundtrip() {
        for (method, wire) in Method::ALL {
            assert_eq!(
                Method::from_wire(wire),
                Some(*method),
                "from_wire({wire}) should resolve"
            );
            assert_eq!(method.wire_name(), *wire, "wire_name roundtrip for {wire}");
        }
    }

    #[test]
    fn method_from_wire_unknown() {
        assert_eq!(Method::from_wire("nonexistent/method"), None);
    }

    #[test]
    fn doctor_run_method_is_registered() {
        assert_eq!(Method::from_wire("doctor/run"), Some(Method::DoctorRun));
        assert_eq!(Method::DoctorRun.wire_name(), "doctor/run");
    }

    #[tokio::test]
    async fn config_reload_shuts_down_gateway_before_daemon_reload() {
        use zeroclaw_infra::session_queue::SessionActorQueue;

        let queue = Arc::new(SessionActorQueue::new(4, 10, 60));
        let sessions = Arc::new(crate::rpc::session::SessionStore::new(16, queue));
        let (gateway_shutdown_tx, mut gateway_shutdown_rx) = tokio::sync::watch::channel(false);
        let (reload_tx, mut reload_rx) = tokio::sync::watch::channel(false);
        let ctx = RpcContext::minimal_with_reload_controls(
            zeroclaw_config::schema::Config::default(),
            sessions,
            Some(gateway_shutdown_tx),
            Some(reload_tx),
        );
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let dispatcher = RpcDispatcher::new(ctx, tx, "test-peer-reload:pid=1".into());

        let result = dispatcher.handle_config_reload();
        assert!(
            result.is_ok(),
            "config/reload should accept reload-capable contexts"
        );

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            gateway_shutdown_rx.changed(),
        )
        .await
        .expect("gateway shutdown must be signalled before daemon reload")
        .expect("gateway shutdown sender should stay alive");
        assert!(*gateway_shutdown_rx.borrow_and_update());
        assert!(
            !*reload_rx.borrow(),
            "daemon reload must wait until the gateway listener has been asked to shut down"
        );

        tokio::time::timeout(std::time::Duration::from_secs(1), reload_rx.changed())
            .await
            .expect("daemon reload should follow gateway shutdown")
            .expect("reload sender should stay alive");
        assert!(*reload_rx.borrow_and_update());
    }

    #[tokio::test]
    async fn quickstart_apply_shuts_down_gateway_before_daemon_reload() {
        use zeroclaw_config::presets::{
            AgentIdentity, BuilderSubmission, MemoryChoice, ModelProviderChoice, SelectorChoice,
        };
        use zeroclaw_infra::session_queue::SessionActorQueue;

        let tmp = tempfile::TempDir::new().unwrap();
        let config = zeroclaw_config::schema::Config {
            data_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..zeroclaw_config::schema::Config::default()
        };
        std::fs::create_dir_all(&config.data_dir).unwrap();

        let queue = Arc::new(SessionActorQueue::new(4, 10, 60));
        let sessions = Arc::new(crate::rpc::session::SessionStore::new(16, queue));
        let (gateway_shutdown_tx, mut gateway_shutdown_rx) = tokio::sync::watch::channel(false);
        let (reload_tx, mut reload_rx) = tokio::sync::watch::channel(false);
        let ctx = RpcContext::minimal_with_reload_controls(
            config,
            sessions,
            Some(gateway_shutdown_tx),
            Some(reload_tx),
        );
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let dispatcher = RpcDispatcher::new(ctx, tx, "test-peer-quickstart-reload:pid=1".into());

        let submission = BuilderSubmission {
            model_provider: SelectorChoice::Fresh(ModelProviderChoice {
                provider_type: "anthropic".into(),
                alias: "anthropic".into(),
                model: "claude-sonnet-4-5".into(),
                fields: std::collections::HashMap::from([(
                    "api_key".to_string(),
                    "sk-test".to_string(),
                )]),
            }),
            risk_profile: SelectorChoice::Fresh("balanced".into()),
            runtime_profile: SelectorChoice::Fresh("balanced".into()),
            memory: SelectorChoice::Fresh(MemoryChoice::Sqlite),
            channels: vec![],
            peer_groups: vec![],
            agent: AgentIdentity {
                name: "quickstart_bot".into(),
                system_prompt: "You are helpful.".into(),
                personality_file: None,
                personality_files: vec![],
            },
        };

        let result = dispatcher
            .handle_quickstart_apply(&json!({ "submission": submission }))
            .await
            .expect("quickstart/apply should accept reload-capable contexts");
        assert_eq!(
            result["kind"], "applied",
            "quickstart/apply result: {result:#?}"
        );
        assert_eq!(result["daemon_restarted"], true);

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            gateway_shutdown_rx.changed(),
        )
        .await
        .expect("quickstart/apply must signal gateway shutdown before daemon reload")
        .expect("gateway shutdown sender should stay alive");
        assert!(*gateway_shutdown_rx.borrow_and_update());
        assert!(
            !*reload_rx.borrow(),
            "quickstart/apply daemon reload must wait until the gateway listener has been asked to shut down"
        );

        tokio::time::timeout(std::time::Duration::from_secs(1), reload_rx.changed())
            .await
            .expect("quickstart/apply daemon reload should follow gateway shutdown")
            .expect("reload sender should stay alive");
        assert!(*reload_rx.borrow_and_update());
    }

    #[test]
    fn doctor_summary_counts_each_severity_bucket() {
        let results = vec![
            DiagResult {
                severity: crate::doctor::Severity::Ok,
                category: "config".to_string(),
                message: "ok".to_string(),
            },
            DiagResult {
                severity: crate::doctor::Severity::Warn,
                category: "config".to_string(),
                message: "warning".to_string(),
            },
            DiagResult {
                severity: crate::doctor::Severity::Warn,
                category: "runtime".to_string(),
                message: "warning".to_string(),
            },
            DiagResult {
                severity: crate::doctor::Severity::Error,
                category: "workspace".to_string(),
                message: "error".to_string(),
            },
        ];

        let summary = doctor_summary(&results);

        assert_eq!(summary.ok, 1);
        assert_eq!(summary.warnings, 2);
        assert_eq!(summary.errors, 1);
    }

    #[test]
    fn method_all_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for (_, wire) in Method::ALL {
            assert!(seen.insert(*wire), "duplicate wire name: {wire}");
        }
    }

    #[test]
    fn session_approve_resolves_pending_request() {
        let dispatcher = make_approval_test_dispatcher();
        let (tx, mut rx) =
            tokio::sync::oneshot::channel::<zeroclaw_api::channel::ChannelApprovalResponse>();
        dispatcher
            .ctx
            .approval_pending
            .insert("req-allow".to_string(), tx);

        let result = dispatcher
            .handle_session_approve(&json!({
                "session_id": "sess-1",
                "request_id": "req-allow",
                "decision": "allow_once"
            }))
            .unwrap();

        assert_eq!(result["session_id"], "sess-1");
        assert_eq!(result["request_id"], "req-allow");
        assert_eq!(result["acknowledged"], true);
        assert_eq!(
            rx.try_recv().unwrap(),
            zeroclaw_api::channel::ChannelApprovalResponse::Approve
        );
        assert!(!dispatcher.ctx.approval_pending.contains("req-allow"));
    }

    #[test]
    fn session_approve_unknown_request_is_acknowledged_noop() {
        let dispatcher = make_approval_test_dispatcher();

        let result = dispatcher
            .handle_session_approve(&json!({
                "session_id": "sess-1",
                "request_id": "timed-out-req",
                "decision": "allow_once"
            }))
            .unwrap();

        assert_eq!(result["session_id"], "sess-1");
        assert_eq!(result["request_id"], "timed-out-req");
        assert_eq!(result["acknowledged"], true);
        assert!(!dispatcher.ctx.approval_pending.contains("timed-out-req"));
    }

    #[test]
    fn personality_templates_use_requested_agent_name_before_config_exists() {
        let req = PersonalityTemplatesParams {
            agent: Some(" bob ".to_string()),
        };
        let ctx = personality_template_context(&zeroclaw_config::schema::Config::default(), &req);

        assert_eq!(ctx.agent, "bob");
        assert!(ctx.include_memory);
    }

    #[test]
    fn personality_templates_without_agent_stay_generic_and_memoryless() {
        let req = PersonalityTemplatesParams { agent: None };
        let ctx = personality_template_context(&zeroclaw_config::schema::Config::default(), &req);

        assert_eq!(ctx.agent, "ZeroClaw");
        assert!(!ctx.include_memory);
    }

    #[test]
    fn chunk_notification() {
        let event = TurnEvent::Chunk {
            delta: "hello".into(),
        };
        let json = notification_for_turn_event("s1", &event, None).unwrap();
        let v = parse(&json);
        assert_eq!(v["jsonrpc"], JSONRPC_VERSION);
        assert_eq!(v["method"], notification::SESSION_UPDATE);
        assert_eq!(v["params"]["session_id"], "s1");
        assert_eq!(v["params"]["type"], "agent_message_chunk");
        assert_eq!(v["params"]["text"], "hello");
    }

    #[test]
    fn thinking_notification() {
        let event = TurnEvent::Thinking {
            delta: "hmm".into(),
        };
        let json = notification_for_turn_event("s1", &event, None).unwrap();
        let v = parse(&json);
        assert_eq!(v["params"]["type"], "agent_thought_chunk");
        assert_eq!(v["params"]["text"], "hmm");
    }

    #[test]
    fn tool_call_notification() {
        let event = TurnEvent::ToolCall {
            id: "tc_1".into(),
            name: "bash".into(),
            args: json!({"cmd": "ls"}),
        };
        let json = notification_for_turn_event("s1", &event, None).unwrap();
        let v = parse(&json);
        assert_eq!(v["params"]["type"], "tool_call");
        assert_eq!(v["params"]["tool_call_id"], "tc_1");
        assert_eq!(v["params"]["name"], "bash");
        assert_eq!(v["params"]["raw_input"]["cmd"], "ls");
    }

    #[test]
    fn tool_result_notification() {
        let event = TurnEvent::ToolResult {
            id: "tc_1".into(),
            name: "bash".into(),
            output: "file.txt".into(),
        };
        let json = notification_for_turn_event("s1", &event, None).unwrap();
        let v = parse(&json);
        assert_eq!(v["params"]["type"], "tool_result");
        assert_eq!(v["params"]["tool_call_id"], "tc_1");
        assert_eq!(v["params"]["raw_output"], "file.txt");
    }

    #[test]
    fn plan_turn_event_maps_to_plan_notification() {
        use zeroclaw_api::plan::{PlanEntry, PlanPriority, PlanStatus};

        let event = TurnEvent::Plan {
            entries: vec![PlanEntry {
                content: "Analyze codebase".to_string(),
                status: PlanStatus::InProgress,
                priority: PlanPriority::High,
                active_form: Some("Analyzing codebase".to_string()),
            }],
        };
        let json = notification_for_turn_event("sess-1", &event, None)
            .expect("plan yields a notification");
        let v = parse(&json);
        assert_eq!(v["method"], "session/update");
        assert_eq!(v["params"]["type"], "plan");
        assert_eq!(v["params"]["session_id"], "sess-1");
        assert_eq!(v["params"]["entries"][0]["content"], "Analyze codebase");
        assert_eq!(v["params"]["entries"][0]["status"], "in_progress");
        assert_eq!(v["params"]["entries"][0]["priority"], "high");
        assert_eq!(
            v["params"]["entries"][0]["activeForm"],
            "Analyzing codebase"
        );
    }

    #[test]
    fn empty_plan_turn_event_maps_to_empty_entries() {
        let event = TurnEvent::Plan { entries: vec![] };
        let json =
            notification_for_turn_event("sess-2", &event, None).expect("empty plan still notifies");
        let v = parse(&json);
        assert_eq!(v["params"]["type"], "plan");
        assert!(v["params"]["entries"].as_array().unwrap().is_empty());
    }

    #[test]
    fn resume_plan_notification_built_for_nonempty_plan() {
        use zeroclaw_api::plan::{PlanEntry, PlanPriority, PlanStatus};
        let entries = vec![PlanEntry {
            content: "Resume me".to_string(),
            status: PlanStatus::Pending,
            priority: PlanPriority::Medium,
            active_form: None,
        }];
        let json = plan_replay_notification("sess-9", &entries).expect("nonempty plan replays");
        let v = parse(&json);
        assert_eq!(v["method"], "session/update");
        assert_eq!(v["params"]["type"], "plan");
        assert_eq!(v["params"]["session_id"], "sess-9");
        assert_eq!(v["params"]["entries"][0]["content"], "Resume me");
    }

    #[test]
    fn resume_plan_notification_absent_for_empty_plan() {
        assert!(plan_replay_notification("sess-9", &[]).is_none());
    }

    async fn store_with_one_session(sid: &str) -> Arc<crate::rpc::session::SessionStore> {
        use zeroclaw_infra::session_queue::SessionActorQueue;
        let queue = Arc::new(SessionActorQueue::new(4, 10, 60));
        let sessions = Arc::new(crate::rpc::session::SessionStore::new(16, queue));
        let agent = crate::agent::agent::Agent::builder()
            .model_provider(Box::new(DummyModelProvider))
            .tools(vec![])
            .memory(Arc::new(zeroclaw_memory::NoneMemory::new("none")))
            .observer(Arc::new(crate::observability::noop::NoopObserver))
            .tool_dispatcher(Box::new(crate::agent::dispatcher::NativeToolDispatcher))
            .workspace_dir(std::env::temp_dir())
            .build()
            .expect("minimal Agent should build");
        let rpc_session = crate::rpc::session::RpcSession::new(
            agent,
            "test-agent",
            std::env::temp_dir().to_str().unwrap(),
            crate::rpc::types::ChatMode::Chat,
        );
        sessions.insert(sid.to_string(), rpc_session).await.unwrap();
        sessions
    }

    #[tokio::test]
    async fn plan_event_is_stored_before_emitting() {
        use zeroclaw_api::plan::{PlanEntry, PlanPriority, PlanStatus};
        let sid = "persist-plan-sess";
        let store = store_with_one_session(sid).await;

        let entries = vec![PlanEntry {
            content: "A".to_string(),
            status: PlanStatus::InProgress,
            priority: PlanPriority::High,
            active_form: None,
        }];
        let event = TurnEvent::Plan {
            entries: entries.clone(),
        };
        persist_plan_if_any(&store, None, sid, &event).await;
        assert_eq!(store.get_plan(sid).await.unwrap(), entries);
    }

    #[tokio::test]
    async fn non_plan_event_does_not_touch_stored_plan() {
        let sid = "no-touch-sess";
        let store = store_with_one_session(sid).await;
        persist_plan_if_any(&store, None, sid, &TurnEvent::Chunk { delta: "hi".into() }).await;
        assert!(store.get_plan(sid).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn plan_event_persists_to_durable_acp_store() {
        use zeroclaw_api::plan::{PlanEntry, PlanPriority, PlanStatus};
        let sid = "durable-plan-sess";
        let sessions = store_with_one_session(sid).await;

        let tmp = tempfile::TempDir::new().unwrap();
        let acp =
            Arc::new(zeroclaw_infra::acp_session_store::AcpSessionStore::new(tmp.path()).unwrap());
        acp.create_session(sid, "alpha", tmp.path().to_str().unwrap())
            .unwrap();

        let entries = vec![PlanEntry {
            content: "Durable".to_string(),
            status: PlanStatus::Pending,
            priority: PlanPriority::Low,
            active_form: None,
        }];
        let event = TurnEvent::Plan {
            entries: entries.clone(),
        };
        persist_plan_if_any(&sessions, Some(&acp), sid, &event).await;

        // In-memory cache updated…
        assert_eq!(sessions.get_plan(sid).await.unwrap(), entries);
        // …and durable store updated (survives daemon restart / eviction).
        assert_eq!(acp.get_plan(sid).unwrap(), entries);
    }

    #[test]
    fn approval_request_notification() {
        let event = TurnEvent::ApprovalRequest {
            request_id: "ar_1".into(),
            tool_name: "bash".into(),
            arguments_summary: "rm -rf /".into(),
            timeout_secs: 30,
        };
        let json = notification_for_turn_event("s1", &event, None).unwrap();
        let v = parse(&json);
        assert_eq!(v["params"]["type"], "approval_request");
        assert_eq!(v["params"]["request_id"], "ar_1");
        assert_eq!(v["params"]["tool_name"], "bash");
        assert_eq!(v["params"]["timeout_secs"], 30);
    }

    #[test]
    fn history_trimmed_notification() {
        let event = TurnEvent::HistoryTrimmed {
            dropped_messages: 12,
            kept_turns: 1,
            reason: "context token budget exceeded".into(),
        };
        let json = notification_for_turn_event("s1", &event, None).unwrap();
        let v = parse(&json);
        assert_eq!(v["method"], "session/update");
        assert_eq!(v["params"]["type"], "history_trimmed");
        assert_eq!(v["params"]["session_id"], "s1");
        assert_eq!(v["params"]["dropped_messages"], 12);
        assert_eq!(v["params"]["kept_turns"], 1);
        assert_eq!(v["params"]["reason"], "context token budget exceeded");
    }

    #[test]
    fn usage_event_emits_context_usage_notification() {
        let event = TurnEvent::Usage {
            input_tokens: Some(100),
            cached_input_tokens: None,
            output_tokens: Some(50),
            cost_usd: Some(0.01),
        };
        let json = notification_for_turn_event("s1", &event, Some(32_000)).unwrap();
        let v = parse(&json);
        assert_eq!(v["params"]["type"], "context_usage");
        assert_eq!(v["params"]["session_id"], "s1");
        // Context size is the prompt the model just consumed = input_tokens.
        // Output tokens are the model's reply, not part of the prompt size.
        // cached_input_tokens is a *subset* of input_tokens per the
        // TokenUsage contract and must NOT be added (double-counts).
        assert_eq!(v["params"]["input_tokens"], 100);
        assert_eq!(v["params"]["max_context_tokens"], 32_000);
    }

    /// Regression: Zerocode's context meter must read the runtime-profile
    /// `max_context_tokens` budget, not the provider model-window helper.
    /// The model-window path falls back to 32_000 when `context_window` is
    /// unset, which made the meter ignore a profile set to e.g. 128_000.
    #[test]
    fn context_usage_max_tokens_uses_runtime_profile_budget() {
        use std::collections::HashMap;
        use zeroclaw_config::schema::{AliasedAgentConfig, Config, RuntimeProfileConfig};

        let mut runtime_profiles = HashMap::new();
        runtime_profiles.insert(
            "coding".to_string(),
            RuntimeProfileConfig {
                max_context_tokens: Some(128_000),
                ..RuntimeProfileConfig::default()
            },
        );

        let mut agents = HashMap::new();
        agents.insert(
            "coder".to_string(),
            AliasedAgentConfig {
                enabled: true,
                runtime_profile: "coding".into(),
                // No provider context_window configured — the broken path
                // would fall back to 32_000 here.
                ..AliasedAgentConfig::default()
            },
        );

        let cfg = Config {
            agents,
            runtime_profiles,
            ..Config::default()
        };

        assert_eq!(
            context_usage_max_tokens(&cfg, "coder"),
            128_000,
            "context meter must use runtime_profiles.<name>.max_context_tokens"
        );
        assert_eq!(
            cfg.effective_model_context_window("coder"),
            32_000,
            "sanity: model-window helper still defaults to 32k without provider context_window"
        );
    }

    /// Boundary regression: prove the corrected ceiling survives the *wire*
    /// path, not just the config helper. This threads
    /// `context_usage_max_tokens(&cfg, alias)` through the exact
    /// `notification_for_turn_event` serialization the RPC dispatch emits, and
    /// asserts the on-the-wire `context_usage.max_context_tokens` reads the
    /// runtime-profile budget (128_000) rather than the model-window fallback
    /// (32_000). This closes the "helper is right but does the emitted payload
    /// carry it?" gap without needing a live daemon smoke.
    #[test]
    fn context_usage_notification_wire_reports_runtime_profile_budget() {
        use std::collections::HashMap;
        use zeroclaw_config::schema::{AliasedAgentConfig, Config, RuntimeProfileConfig};

        let mut runtime_profiles = HashMap::new();
        runtime_profiles.insert(
            "coding".to_string(),
            RuntimeProfileConfig {
                max_context_tokens: Some(128_000),
                ..RuntimeProfileConfig::default()
            },
        );

        let mut agents = HashMap::new();
        agents.insert(
            "coder".to_string(),
            AliasedAgentConfig {
                enabled: true,
                runtime_profile: "coding".into(),
                // No provider context_window: the broken path would emit 32_000.
                ..AliasedAgentConfig::default()
            },
        );

        let cfg = Config {
            agents,
            runtime_profiles,
            ..Config::default()
        };

        // Resolve the ceiling exactly as RPC dispatch does, then emit it
        // through the real wire serializer.
        let max_ctx = context_usage_max_tokens(&cfg, "coder");
        let event = TurnEvent::Usage {
            input_tokens: Some(100),
            cached_input_tokens: None,
            output_tokens: Some(50),
            cost_usd: Some(0.01),
        };
        let json = notification_for_turn_event("s1", &event, Some(max_ctx)).unwrap();
        let v = parse(&json);

        assert_eq!(v["params"]["type"], "context_usage");
        assert_eq!(
            v["params"]["max_context_tokens"], 128_000,
            "emitted context_usage must carry the runtime-profile budget, not the 32k model-window fallback"
        );
    }

    #[test]
    fn usage_event_without_input_tokens_emits_null() {
        let event = TurnEvent::Usage {
            input_tokens: None,
            cached_input_tokens: None,
            output_tokens: Some(50),
            cost_usd: None,
        };
        let json = notification_for_turn_event("s1", &event, None).unwrap();
        let v = parse(&json);
        assert_eq!(v["params"]["type"], "context_usage");
        // No input_tokens reported → field omitted (skip_serializing_if).
        assert!(
            v["params"].get("input_tokens").is_none(),
            "absent input_tokens should not be synthesized from output_tokens"
        );
    }

    #[test]
    fn usage_event_does_not_double_count_cached_subset() {
        let event = TurnEvent::Usage {
            input_tokens: Some(25_000),
            cached_input_tokens: Some(15_000),
            output_tokens: Some(200),
            cost_usd: None,
        };
        let json = notification_for_turn_event("s1", &event, Some(200_000)).unwrap();
        let v = parse(&json);
        assert_eq!(v["params"]["type"], "context_usage");
        assert_eq!(
            v["params"]["input_tokens"], 25_000,
            "input_tokens is reported as-is — cached subset must not be added"
        );
    }

    #[test]
    fn usage_event_only_cached_tokens_emits_null() {
        // Edge case: provider reports only cached without input total.
        // Without a known total this is ambiguous, so we don't synthesize one.
        let event = TurnEvent::Usage {
            input_tokens: None,
            cached_input_tokens: Some(80_000),
            output_tokens: Some(100),
            cost_usd: None,
        };
        let json = notification_for_turn_event("s1", &event, Some(100_000)).unwrap();
        let v = parse(&json);
        assert!(
            v["params"].get("input_tokens").is_none(),
            "cached-only is ambiguous; do not fabricate a total"
        );
    }

    #[test]
    fn parse_params_valid() {
        let v = json!({"session_id": "s1"});
        let p: SessionIdParams = parse_params(&v).unwrap();
        assert_eq!(p.session_id, "s1");
    }

    #[test]
    fn parse_params_missing_required() {
        let v = json!({});
        let err = parse_params::<SessionIdParams>(&v).unwrap_err();
        assert_eq!(err.code, INVALID_PARAMS);
    }

    #[test]
    fn to_result_roundtrip() {
        let r = InitializeResult {
            protocol_version: 1,
            server_version: "0.1.0".into(),
            tui_id: None,
            tui_sig: None,
            capabilities: vec![],
        };
        let val = to_result(r).unwrap();
        assert_eq!(val["protocol_version"], 1);
        assert_eq!(val["server_version"], "0.1.0");
    }

    #[test]
    fn status_runtime_context_reports_config_root_and_local_endpoint() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = zeroclaw_config::schema::Config {
            config_path: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
            ..zeroclaw_config::schema::Config::default()
        };
        std::fs::create_dir_all(&config.data_dir).unwrap();

        let context = status_runtime_context(&config, RuntimeConfigKind::Temporary);

        assert_eq!(context.config_dir, tmp.path().display().to_string());
        assert_eq!(
            context.config_file,
            tmp.path().join("config.toml").display().to_string()
        );
        assert_eq!(context.config_kind, RuntimeConfigKind::Temporary);
        assert_eq!(
            context.local_ipc_endpoint,
            crate::rpc::local::socket_path(&config)
                .display()
                .to_string()
        );

        config.config_path = std::path::PathBuf::from("/opt/zeroclaw/config.toml");
        assert_eq!(
            status_runtime_context(&config, RuntimeConfigKind::Custom).config_kind,
            RuntimeConfigKind::Custom
        );
    }

    #[tokio::test]
    async fn handle_status_includes_runtime_context_fields() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = zeroclaw_config::schema::Config {
            config_path: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
            ..zeroclaw_config::schema::Config::default()
        };
        let (dispatcher, _sessions) = make_acp_test_dispatcher(config.clone());

        let value = dispatcher.handle_status().await.expect("status result");
        let status: StatusResult = serde_json::from_value(value).expect("status shape");

        assert_eq!(
            status.config_dir.as_deref(),
            Some(tmp.path().to_str().unwrap())
        );
        assert_eq!(
            status.config_file.as_deref(),
            Some(tmp.path().join("config.toml").to_str().unwrap())
        );
        assert_eq!(status.config_kind, Some(RuntimeConfigKind::Temporary));
        assert_eq!(
            status.local_ipc_endpoint.as_deref(),
            Some(crate::rpc::local::socket_path(&config).to_str().unwrap())
        );
    }

    /// Cover the `initialize` parsing path that caches the TUI's
    /// `clientCapabilities.elicitation` block so the per-session
    /// `RpcApprovalChannel` can route `request_choice` over
    /// `elicitation/create`. Source-of-truth check: the dispatcher
    /// is the canonical owner; the test reads the field directly.
    #[tokio::test]
    async fn handle_initialize_caches_elicitation_form_capability() {
        let (mut dispatcher, _sessions) =
            make_acp_test_dispatcher(zeroclaw_config::schema::Config::default());
        let params = serde_json::json!({
            "protocol_version": RPC_PROTOCOL_VERSION,
            "clientCapabilities": { "elicitation": { "form": {} } }
        });
        let result = dispatcher.handle_initialize(&params).await;
        assert!(result.is_ok(), "initialize should succeed; got {result:?}");
        assert!(dispatcher.client_elicitation_caps.form);
        assert!(!dispatcher.client_elicitation_caps.url);
    }

    #[tokio::test]
    async fn handle_initialize_without_elicitation_leaves_caps_unset() {
        let (mut dispatcher, _sessions) =
            make_acp_test_dispatcher(zeroclaw_config::schema::Config::default());
        let params = serde_json::json!({
            "protocol_version": RPC_PROTOCOL_VERSION,
        });
        let _ = dispatcher.handle_initialize(&params).await.unwrap();
        assert!(!dispatcher.client_elicitation_caps.form);
        assert!(!dispatcher.client_elicitation_caps.url);
    }

    #[tokio::test]
    async fn handle_initialize_empty_elicitation_object_is_form_only() {
        // RFD backward-compat: `"elicitation": {}` advertises form-only.
        let (mut dispatcher, _sessions) =
            make_acp_test_dispatcher(zeroclaw_config::schema::Config::default());
        let params = serde_json::json!({
            "protocol_version": RPC_PROTOCOL_VERSION,
            "clientCapabilities": { "elicitation": {} }
        });
        let _ = dispatcher.handle_initialize(&params).await.unwrap();
        assert!(dispatcher.client_elicitation_caps.form);
        assert!(!dispatcher.client_elicitation_caps.url);
    }

    use zeroclaw_tools::MEMORY_TOOL_NAMES as MEMORY_TOOLS;

    fn make_acp_test_config(tmp: &tempfile::TempDir) -> zeroclaw_config::schema::Config {
        use std::collections::HashMap;
        use zeroclaw_config::schema::{AliasedAgentConfig, RiskProfileConfig};

        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut providers = zeroclaw_config::providers::Providers::default();
        {
            let base = providers
                .models
                .ensure("openai", "test-provider")
                .expect("`openai` slot must exist");
            base.api_key = Some("test-key".into());
            base.model = Some("test-model".into());
            base.uri = Some("http://127.0.0.1:1".into());
        }

        let mut agents = HashMap::new();
        agents.insert(
            "test-agent".to_string(),
            AliasedAgentConfig {
                enabled: true,
                model_provider: "openai.test-provider".into(),
                risk_profile: "test-profile".into(),
                ..Default::default()
            },
        );

        let mut risk_profiles = HashMap::new();
        risk_profiles.insert("test-profile".to_string(), RiskProfileConfig::default());

        zeroclaw_config::schema::Config {
            data_dir: workspace_dir,
            config_path: tmp.path().join("config.toml"),
            providers,
            agents,
            risk_profiles,
            ..zeroclaw_config::schema::Config::default()
        }
    }

    fn make_acp_test_dispatcher(
        config: zeroclaw_config::schema::Config,
    ) -> (RpcDispatcher, Arc<crate::rpc::session::SessionStore>) {
        make_acp_test_dispatcher_with_events(config, None)
    }

    fn make_acp_test_dispatcher_with_events(
        config: zeroclaw_config::schema::Config,
        event_tx: Option<tokio::sync::broadcast::Sender<Value>>,
    ) -> (RpcDispatcher, Arc<crate::rpc::session::SessionStore>) {
        use zeroclaw_infra::session_queue::SessionActorQueue;
        let queue = Arc::new(SessionActorQueue::new(4, 10, 60));
        let sessions = Arc::new(crate::rpc::session::SessionStore::new(16, queue));
        let ctx = RpcContext::minimal(config, Arc::clone(&sessions));
        let mut ctx = Arc::try_unwrap(ctx)
            .ok()
            .expect("minimal test context should be uniquely owned");
        ctx.event_tx = event_tx;
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let dispatcher = RpcDispatcher::new(Arc::new(ctx), tx, "test-peer".into());
        (dispatcher, sessions)
    }

    #[tokio::test]
    async fn cron_trigger_rpc_persists_run_history() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = make_acp_test_config(&tmp);
        config
            .risk_profiles
            .entry("test-profile".into())
            .or_default()
            .allowed_commands = vec!["echo".into()];
        let job = crate::cron::add_shell_job_with_approval(
            &config,
            "test-agent",
            Some("rpc-trigger".into()),
            crate::cron::Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo rpc-trigger-ok",
            None,
            true,
        )
        .expect("test cron job should be created");
        let (dispatcher, _sessions) = make_acp_test_dispatcher(config.clone());

        let value = dispatcher
            .handle_cron_trigger(&json!({ "id": job.id }))
            .await
            .expect("cron/trigger should succeed");

        assert_eq!(value["id"], job.id);
        assert_eq!(value["success"], true);
        assert_eq!(value["status"], "ok");
        assert!(
            value["output"]
                .as_str()
                .unwrap_or("")
                .contains("rpc-trigger-ok")
        );

        let updated = crate::cron::get_job(&config, &job.id).expect("job should still exist");
        assert_eq!(updated.last_status.as_deref(), Some("ok"));
        assert!(
            updated
                .last_output
                .as_deref()
                .is_some_and(|output| output.contains("rpc-trigger-ok"))
        );

        let runs =
            crate::cron::list_runs(&config, &job.id, 10).expect("RPC trigger should persist runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "ok");
        assert!(
            runs[0]
                .output
                .as_deref()
                .unwrap_or("")
                .contains("rpc-trigger-ok")
        );
    }

    #[tokio::test]
    async fn cron_trigger_rpc_reports_degraded_status_and_broadcasts() {
        crate::cron::scheduler::register_delivery_fn(Box::new(
            |_config, channel, _target, _thread_id, _output| {
                Box::pin(async move {
                    if channel == "fail-delivery" {
                        anyhow::bail!("synthetic delivery failure");
                    }
                    Ok(())
                })
            },
        ));

        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = make_acp_test_config(&tmp);
        config
            .risk_profiles
            .entry("test-profile".into())
            .or_default()
            .allowed_commands = vec!["echo".into()];
        let job = crate::cron::add_shell_job_with_approval(
            &config,
            "test-agent",
            Some("rpc-trigger-degraded".into()),
            crate::cron::Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo rpc-trigger-degraded",
            Some(crate::cron::DeliveryConfig {
                mode: "announce".into(),
                channel: Some("fail-delivery".into()),
                to: Some("123456".into()),
                thread_id: None,
                best_effort: true,
            }),
            true,
        )
        .expect("test cron job should be created");
        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(8);
        let (dispatcher, _sessions) =
            make_acp_test_dispatcher_with_events(config.clone(), Some(event_tx));

        let value = dispatcher
            .handle_cron_trigger(&json!({ "id": job.id }))
            .await
            .expect("cron/trigger should succeed");

        assert_eq!(value["id"], job.id);
        assert_eq!(value["success"], true);
        assert_eq!(value["status"], "degraded");
        assert!(
            value["output"]
                .as_str()
                .unwrap_or("")
                .contains("delivery failed:")
        );
        assert!(value["duration_ms"].as_i64().is_some());
        assert!(value["started_at"].as_str().unwrap_or("").contains('T'));
        assert!(value["finished_at"].as_str().unwrap_or("").contains('T'));

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), event_rx.recv())
            .await
            .expect("cron trigger should broadcast")
            .expect("broadcast channel should stay open");
        assert_eq!(event["type"], "cron_result");
        assert_eq!(event["job_id"], job.id);
        assert_eq!(event["success"], true);
        assert_eq!(event["manual"], true);
        assert!(
            event["output"]
                .as_str()
                .unwrap_or("")
                .contains("delivery failed:")
        );

        let runs =
            crate::cron::list_runs(&config, &job.id, 10).expect("RPC trigger should persist runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "degraded");
    }

    #[tokio::test]
    async fn acp_session_new_exposes_no_memory_tools() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = make_acp_test_config(&tmp);
        let (dispatcher, sessions) = make_acp_test_dispatcher(config);

        let params = json!({
            "agent_alias": "test-agent",
            "exclude_memory": true,
            "session_id": "acp-test-session-001"
        });

        let result = dispatcher.handle_session_new_for_test(&params).await;
        assert!(
            result.is_ok(),
            "session/new should succeed; got: {:?}",
            result.err()
        );

        let agent_arc = sessions
            .get_agent("acp-test-session-001")
            .await
            .expect("session must be registered in the store after session/new");

        let agent = agent_arc.lock().await;
        let tool_names = agent.tool_names();

        for &mem_tool in MEMORY_TOOLS {
            assert!(
                !tool_names.contains(&mem_tool),
                "ACP session must NOT expose `{mem_tool}` — found in tool list: {tool_names:?}"
            );
        }
    }

    #[tokio::test]
    async fn acp_chat_mode_strips_memory_tools_without_exclude_flag() {
        // The server must derive memory exclusion from `chat_mode: acp`, not
        // trust the wire `exclude_memory` flag. A Code session that omits the
        // flag entirely must still come up with no memory tools.
        let tmp = tempfile::TempDir::new().unwrap();
        let config = make_acp_test_config(&tmp);
        let data_dir = config.data_dir.clone();
        let (dispatcher, sessions, _chat_backend, _acp_store) =
            make_persistence_test_dispatcher(config, &data_dir);

        let params = json!({
            "agent_alias": "test-agent",
            "chat_mode": "acp",
            "session_id": "acp-no-flag-session-001"
        });

        let result = dispatcher.handle_session_new_for_test(&params).await;
        assert!(
            result.is_ok(),
            "session/new should succeed; got: {:?}",
            result.err()
        );

        let agent_arc = sessions
            .get_agent("acp-no-flag-session-001")
            .await
            .expect("session must be registered in the store after session/new");

        let agent = agent_arc.lock().await;
        let tool_names = agent.tool_names();

        for &mem_tool in MEMORY_TOOLS {
            assert!(
                !tool_names.contains(&mem_tool),
                "ACP chat_mode must strip `{mem_tool}` even without exclude_memory — \
                 tool list: {tool_names:?}"
            );
        }
    }

    #[tokio::test]
    async fn non_acp_session_new_exposes_memory_tools() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = make_acp_test_config(&tmp);
        let (dispatcher, sessions) = make_acp_test_dispatcher(config);

        let params = json!({
            "agent_alias": "test-agent",
            "exclude_memory": false,
            "session_id": "chat-test-session-001"
        });

        let result = dispatcher.handle_session_new_for_test(&params).await;
        assert!(
            result.is_ok(),
            "session/new should succeed; got: {:?}",
            result.err()
        );

        let agent_arc = sessions
            .get_agent("chat-test-session-001")
            .await
            .expect("session must be registered in the store after session/new");

        let agent = agent_arc.lock().await;
        let tool_names = agent.tool_names();

        let has_any_memory_tool = MEMORY_TOOLS.iter().any(|&t| tool_names.contains(&t));
        assert!(
            has_any_memory_tool,
            "non-ACP session MUST expose at least one memory tool — tool list: {tool_names:?}"
        );
    }

    // -----------------------------------------------------------------------
    // chat_mode persistence routing: ACP vs Chat must not cross stores
    // -----------------------------------------------------------------------

    use zeroclaw_infra::session_backend::SessionBackend;

    fn make_persistence_test_dispatcher(
        config: zeroclaw_config::schema::Config,
        data_dir: &std::path::Path,
    ) -> (
        RpcDispatcher,
        Arc<crate::rpc::session::SessionStore>,
        Arc<zeroclaw_infra::session_sqlite::SqliteSessionBackend>,
        Arc<zeroclaw_infra::acp_session_store::AcpSessionStore>,
    ) {
        use zeroclaw_infra::session_queue::SessionActorQueue;
        let queue = Arc::new(SessionActorQueue::new(4, 10, 60));
        let sessions = Arc::new(crate::rpc::session::SessionStore::new(16, queue));
        let chat_backend =
            Arc::new(zeroclaw_infra::session_sqlite::SqliteSessionBackend::new(data_dir).unwrap());
        let acp_store =
            Arc::new(zeroclaw_infra::acp_session_store::AcpSessionStore::new(data_dir).unwrap());
        let ctx = RpcContext::for_persistence_tests(
            config,
            Arc::clone(&sessions),
            Some(chat_backend.clone() as Arc<dyn zeroclaw_infra::session_backend::SessionBackend>),
            Some(Arc::clone(&acp_store)),
        );
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let dispatcher = RpcDispatcher::new(ctx, tx, "test-peer".into());
        (dispatcher, sessions, chat_backend, acp_store)
    }

    #[tokio::test]
    async fn seed_trim_event_is_forwarded_exactly_once() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = make_acp_test_config(&tmp);
        let (dispatcher, mut rx, _sessions) = make_dispatcher_with_capture(config);
        let event = TurnEvent::HistoryTrimmed {
            dropped_messages: 4,
            kept_turns: 1,
            reason: "message cap".into(),
        };

        dispatcher
            .forward_seed_event("restored-session", Some(event))
            .await;

        let raw = rx
            .try_recv()
            .expect("restored history trim must notify the active client");
        let notification: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(notification["method"], notification::SESSION_UPDATE);
        assert_eq!(notification["params"]["session_id"], "restored-session");
        assert_eq!(notification["params"]["dropped_messages"], 4);
        assert!(
            rx.try_recv().is_err(),
            "one seed trim must emit exactly one notification"
        );
    }

    #[tokio::test]
    async fn acp_persistence_appends_complete_pretrim_delta_at_cap() {
        use zeroclaw_api::model_provider::ConversationMessage;

        let tmp = tempfile::TempDir::new().unwrap();
        let store =
            Arc::new(zeroclaw_infra::acp_session_store::AcpSessionStore::new(tmp.path()).unwrap());
        let sid = "trim-at-cap";
        store.create_session(sid, "agent", "/tmp").unwrap();
        let existing = (0..50)
            .map(|index| ConversationMessage::Chat(ChatMessage::user(format!("old-{index}"))))
            .collect::<Vec<_>>();
        store.append_turn(sid, &existing).unwrap();

        let new_messages = vec![
            ConversationMessage::Chat(ChatMessage::user("new-user")),
            ConversationMessage::Chat(ChatMessage::assistant("new-assistant")),
        ];
        let outcome = Ok(TurnOutcome::Completed {
            text: "new-assistant".into(),
            messages: new_messages.clone(),
        });

        assert_eq!(persist_acp_turn(&store, sid, &outcome).await, None);

        let restored = store.load_session(sid).unwrap().unwrap();
        assert_eq!(restored.messages.len(), 52);
        assert_eq!(
            serde_json::to_value(&restored.messages[50..]).unwrap(),
            serde_json::to_value(&new_messages).unwrap()
        );
    }

    #[tokio::test]
    async fn acp_persistence_skips_empty_and_failed_turns() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store =
            Arc::new(zeroclaw_infra::acp_session_store::AcpSessionStore::new(tmp.path()).unwrap());
        let sid = "no-turn-delta";
        store.create_session(sid, "agent", "/tmp").unwrap();

        let empty = Ok(TurnOutcome::Cancelled {
            partial_text: String::new(),
            messages: Vec::new(),
        });
        assert_eq!(persist_acp_turn(&store, sid, &empty).await, None);

        let failed = Err(crate::rpc::turn::TurnError::AgentError("failed".into()));
        assert_eq!(persist_acp_turn(&store, sid, &failed).await, None);
        assert!(
            store
                .load_session(sid)
                .unwrap()
                .unwrap()
                .messages
                .is_empty()
        );
    }

    fn make_agent_rename_test_config(tmp: &tempfile::TempDir) -> zeroclaw_config::schema::Config {
        use zeroclaw_config::multi_agent::{AccessMode, AgentAlias, PeerGroupConfig};
        use zeroclaw_config::schema::{AliasedAgentConfig, DelegateTargetConfig};

        let mut config = zeroclaw_config::schema::Config {
            config_path: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
            ..Default::default()
        };
        config.heartbeat.enabled = true;
        config.heartbeat.agent = "alpha".to_string();
        config.acp.default_agent = Some("alpha".to_string());

        let mut alpha = AliasedAgentConfig {
            delegates: vec![DelegateTargetConfig::bounded("alpha")],
            ..Default::default()
        };
        alpha
            .workspace
            .access
            .insert(AgentAlias::new("alpha"), AccessMode::Read);
        config.agents.insert("alpha".to_string(), alpha);

        let mut reviewer = AliasedAgentConfig {
            delegates: vec![DelegateTargetConfig::bounded("alpha")],
            ..Default::default()
        };
        reviewer
            .workspace
            .read_memory_from
            .push(AgentAlias::new("alpha"));
        config.agents.insert("reviewer".to_string(), reviewer);

        let mut group = PeerGroupConfig::default();
        group.agents.push(AgentAlias::new("alpha"));
        config.peer_groups.insert("crew".to_string(), group);

        config
    }

    #[tokio::test]
    async fn config_map_key_rename_uses_agent_cascade() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = make_agent_rename_test_config(&tmp);
        let data_dir = config.data_dir.clone();
        let (dispatcher, _sessions, _chat_backend, _acp_store) =
            make_persistence_test_dispatcher(config, &data_dir);

        let result = dispatcher
            .handle_config_map_key_rename(&json!({
                "path": "agents",
                "from": "alpha",
                "to": "beta"
            }))
            .await
            .expect("agent rename must succeed");

        assert_eq!(result["renamed"], true);
        assert_eq!(result["path"], "agents");
        assert_eq!(result["from"], "alpha");
        assert_eq!(result["to"], "beta");
        assert!(
            result.get("warnings").is_none(),
            "test stores should make owned-state cascade warning-free: {result:?}"
        );

        let config = dispatcher.ctx.config.read().clone();
        assert!(!config.agents.contains_key("alpha"));
        assert!(config.agents.contains_key("beta"));
        assert_eq!(config.heartbeat.agent, "beta");
        assert_eq!(config.acp.default_agent.as_deref(), Some("beta"));
        assert_eq!(
            config.agents["beta"].delegates,
            vec![zeroclaw_config::schema::DelegateTargetConfig::bounded(
                "beta"
            )]
        );
        assert!(
            config.agents["beta"]
                .workspace
                .access
                .contains_key(&zeroclaw_config::multi_agent::AgentAlias::new("beta"))
        );
        assert_eq!(
            config.agents["reviewer"].delegates,
            vec![zeroclaw_config::schema::DelegateTargetConfig::bounded(
                "beta"
            )]
        );
        assert_eq!(
            config.agents["reviewer"].workspace.read_memory_from,
            vec![zeroclaw_config::multi_agent::AgentAlias::new("beta")]
        );
        assert_eq!(
            config.peer_groups["crew"].agents,
            vec![zeroclaw_config::multi_agent::AgentAlias::new("beta")]
        );

        let written = std::fs::read_to_string(&config.config_path).unwrap();
        assert!(written.contains("[agents.beta]"), "{written}");
        assert!(!written.contains("[agents.alpha]"), "{written}");
        assert!(written.contains("agent = \"beta\""), "{written}");
        assert!(written.contains("default_agent = \"beta\""), "{written}");
    }

    #[tokio::test]
    async fn config_map_key_rename_resumes_committed_agent_rename_side_effects() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = make_agent_rename_test_config(&tmp);
        let old_workspace = config.agent_workspace_dir("alpha");
        std::fs::create_dir_all(&old_workspace).unwrap();
        std::fs::write(old_workspace.join("marker.txt"), "lagged workspace").unwrap();

        zeroclaw_config::alias_refs::rename_with_cascade(
            &mut config,
            &zeroclaw_config::alias_refs::AliasKind::Agent,
            "alpha",
            "beta",
        )
        .expect("seed config already committed to beta");
        let new_workspace = config.agent_workspace_dir("beta");
        let data_dir = config.data_dir.clone();
        let (dispatcher, _sessions, _chat_backend, _acp_store) =
            make_persistence_test_dispatcher(config, &data_dir);

        let result = dispatcher
            .handle_config_map_key_rename(&json!({
                "path": "agents",
                "from": "alpha",
                "to": "beta"
            }))
            .await
            .expect("re-issued rename must converge lagging side effects");

        assert_eq!(result["renamed"], true);
        assert_eq!(result["from"], "alpha");
        assert_eq!(result["to"], "beta");
        assert!(
            !old_workspace.exists(),
            "old workspace should be moved on resume"
        );
        assert!(
            new_workspace.join("marker.txt").exists(),
            "workspace residue should converge onto the renamed alias"
        );
    }

    #[test]
    fn config_alias_rename_future_is_small_enough_for_rpc_task_stack() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = make_agent_rename_test_config(&tmp);
        let data_dir = config.data_dir.clone();
        let (dispatcher, _sessions, _chat_backend, _acp_store) =
            make_persistence_test_dispatcher(config, &data_dir);

        let params = json!({
            "path": "agents",
            "from": "alpha",
            "to": "beta"
        });
        let future = dispatcher.handle_config_map_key_rename(&params);
        let future_size = std::mem::size_of_val(&future);
        drop(future);

        assert!(
            future_size < 16 * 1024,
            "agent alias rename future is {future_size} bytes; keep large config snapshots \
             out of the RPC task stack"
        );
    }

    #[tokio::test]
    async fn config_map_key_rename_refuses_active_agent_sessions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = make_acp_test_config(&tmp);
        let data_dir = config.data_dir.clone();
        let (dispatcher, sessions, _chat_backend, _acp_store) =
            make_persistence_test_dispatcher(config, &data_dir);

        dispatcher
            .handle_session_new_for_test(&json!({
                "agent_alias": "test-agent",
                "session_id": "live-agent-session"
            }))
            .await
            .expect("session/new should succeed");
        assert_eq!(sessions.count_by_agent().await.get("test-agent"), Some(&1));

        let err = dispatcher
            .handle_config_map_key_rename(&json!({
                "path": "agents",
                "from": "test-agent",
                "to": "renamed-agent"
            }))
            .await
            .expect_err("agent rename must refuse active sessions");

        assert_eq!(err.code, INVALID_PARAMS);
        assert!(
            err.message.contains("active RPC session"),
            "unexpected error message: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn acp_session_new_writes_to_acp_store_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = make_acp_test_config(&tmp);
        let data_dir = config.data_dir.clone();
        let (dispatcher, _sessions, chat_backend, acp_store) =
            make_persistence_test_dispatcher(config, &data_dir);

        let sid = "acp-routing-001";
        let params = json!({
            "agent_alias": "test-agent",
            "exclude_memory": true,
            "chat_mode": "acp",
            "session_id": sid,
        });

        dispatcher
            .handle_session_new_for_test(&params)
            .await
            .expect("session/new should succeed");

        assert!(
            acp_store.load_session(sid).unwrap().is_some(),
            "ACP session must be persisted to acp_session_store"
        );

        assert!(
            chat_backend.load(&format!("rpc_{sid}")).is_empty(),
            "ACP session must NOT touch chat session_backend"
        );
    }

    #[tokio::test]
    async fn session_messages_falls_back_to_acp_store_for_acp_sessions() {
        use serde_json::from_value;
        use zeroclaw_api::model_provider::{ChatMessage, ConversationMessage};
        use zeroclaw_providers::{ToolCall, ToolResultMessage};

        let tmp = tempfile::TempDir::new().unwrap();
        let config = make_acp_test_config(&tmp);
        let data_dir = config.data_dir.clone();
        let (dispatcher, _sessions, chat_backend, acp_store) =
            make_persistence_test_dispatcher(config, &data_dir);

        let sid = "acp-resume-7799";
        acp_store
            .create_session(sid, "test-agent", "/tmp/ws")
            .expect("ACP session row");
        acp_store
            .append_turn(
                sid,
                &[
                    ConversationMessage::Chat(ChatMessage {
                        role: "user".into(),
                        content: "hello from prior turn".into(),
                    }),
                    ConversationMessage::AssistantToolCalls {
                        text: Some("let me check the logs".into()),
                        tool_calls: vec![ToolCall {
                            id: "tc-1".into(),
                            name: "shell".into(),
                            arguments: r#"{"command":"tail log"}"#.into(),
                            extra_content: None,
                        }],
                        reasoning_content: None,
                    },
                    ConversationMessage::ToolResults(vec![ToolResultMessage {
                        tool_call_id: "tc-1".into(),
                        content: "log contents".into(),
                        tool_name: String::new(),
                    }]),
                    ConversationMessage::AssistantToolCalls {
                        text: None,
                        tool_calls: vec![ToolCall {
                            id: "tc-2".into(),
                            name: "shell".into(),
                            arguments: r#"{"command":"grep err"}"#.into(),
                            extra_content: None,
                        }],
                        reasoning_content: None,
                    },
                    ConversationMessage::ToolResults(vec![ToolResultMessage {
                        tool_call_id: "tc-2".into(),
                        content: "no errors".into(),
                        tool_name: String::new(),
                    }]),
                    ConversationMessage::Chat(ChatMessage {
                        role: "assistant".into(),
                        content: "ack from prior turn".into(),
                    }),
                ],
            )
            .expect("append turn");

        // Sanity: the unified backend really is empty for this id under any
        // candidate key. If this ever changes the test below stops being a
        // regression for the ACP-store fallback.
        for key in [sid.to_string(), format!("rpc_{sid}"), format!("gw_{sid}")] {
            assert!(
                chat_backend.load(&key).is_empty(),
                "precondition: unified backend has no rows for {key}"
            );
        }

        let result = dispatcher
            .handle_session_messages_for_test(&json!({ "session_id": sid }))
            .await
            .expect("session/messages should succeed");
        let parsed: SessionMessagesResult =
            from_value(result).expect("SessionMessagesResult shape");

        assert_eq!(parsed.session_id, sid);
        assert_eq!(
            parsed.total, 3,
            "ACP-backed sessions must report their full replayable message count"
        );
        assert_eq!(
            parsed.messages.len(),
            3,
            "ACP-backed sessions must replay their persisted messages, not a blank transcript"
        );
        assert_eq!(parsed.messages[0].role, "user");
        assert_eq!(parsed.messages[0].content, "hello from prior turn");
        assert_eq!(parsed.messages[1].role, "assistant");
        assert_eq!(
            parsed.messages[1].content, "let me check the logs",
            "assistant narration on an AssistantToolCalls row must be preserved \
             when flattening for session/messages — the agent stores it ONLY \
             on that row, so dropping it would lose visible turns from the \
             replayed transcript"
        );
        assert_eq!(parsed.messages[2].role, "assistant");
        assert_eq!(parsed.messages[2].content, "ack from prior turn");
    }

    #[tokio::test]
    async fn reaped_acp_session_rehydrates_to_working_instead_of_failing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = make_acp_test_config(&tmp);
        let data_dir = config.data_dir.clone();
        let (dispatcher, sessions, _chat_backend, acp_store) =
            make_persistence_test_dispatcher(config, &data_dir);

        let sid = "acp-reaped-001";
        dispatcher
            .handle_session_new_for_test(&json!({
                "agent_alias": "test-agent",
                "exclude_memory": true,
                "chat_mode": "acp",
                "session_id": sid,
            }))
            .await
            .expect("session/new should succeed");

        assert!(
            sessions.get_agent(sid).await.is_some(),
            "freshly created session must be live in memory"
        );
        assert!(
            acp_store.load_session(sid).unwrap().is_some(),
            "durable row must exist for the rehydrate source"
        );

        // Simulate the reaper tearing the in-memory session down while the
        // durable row survives.
        assert!(
            sessions.remove(sid).await,
            "reap must remove the in-memory session"
        );
        assert!(
            sessions.get_agent(sid).await.is_none(),
            "post-reap the session must be absent from memory"
        );

        let recovered = dispatcher.rehydrate_reaped_session(sid).await;
        assert!(
            recovered.is_some(),
            "a reaped session with a live durable row must rehydrate to a \
             working agent, not fail; failing here is the irrecoverable hang"
        );
        assert!(
            sessions.get_agent(sid).await.is_some(),
            "after rehydrate the session must be live in memory again so the \
             next prompt lands on a working session"
        );
    }

    #[tokio::test]
    async fn acp_resume_recovers_persisted_cwd() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = make_acp_test_config(&tmp);
        let data_dir = config.data_dir.clone();
        let (dispatcher, _sessions, _chat_backend, acp_store) =
            make_persistence_test_dispatcher(config, &data_dir);

        let sid = "acp-cwd-resume-001";
        let original_cwd = tmp.path().join("project-dir").to_string_lossy().to_string();

        // First create the session with an explicit cwd.
        let created = dispatcher
            .handle_session_new_for_test(&json!({
                "agent_alias": "test-agent",
                "exclude_memory": true,
                "chat_mode": "acp",
                "session_id": sid,
                "cwd": original_cwd,
            }))
            .await
            .expect("initial session/new should succeed");
        assert_eq!(created["workspace_dir"], original_cwd);
        assert_eq!(
            acp_store.load_session(sid).unwrap().unwrap().workspace_dir,
            original_cwd
        );

        // Resume with NO cwd: the daemon must report the persisted cwd, not the
        // agent workspace dir.
        let resumed = dispatcher
            .handle_session_new_for_test(&json!({
                "agent_alias": "test-agent",
                "exclude_memory": true,
                "chat_mode": "acp",
                "session_id": sid,
            }))
            .await
            .expect("resume session/new should succeed");
        assert_eq!(
            resumed["workspace_dir"], original_cwd,
            "resume must keep the retained session's cwd, not default it"
        );
    }

    #[tokio::test]
    async fn reaped_acp_session_rehydrates_without_memory_tools() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = make_acp_test_config(&tmp);
        let data_dir = config.data_dir.clone();
        let (dispatcher, sessions, _chat_backend, _acp_store) =
            make_persistence_test_dispatcher(config, &data_dir);

        let sid = "acp-reaped-mem-001";
        dispatcher
            .handle_session_new_for_test(&json!({
                "agent_alias": "test-agent",
                "chat_mode": "acp",
                "session_id": sid,
            }))
            .await
            .expect("session/new should succeed");

        // Reap the in-memory session, leaving the durable row to rehydrate from.
        assert!(sessions.remove(sid).await, "reap must remove the session");

        let recovered = dispatcher
            .rehydrate_reaped_session(sid)
            .await
            .expect("a reaped ACP session must rehydrate to a working agent");

        let agent = recovered.lock().await;
        let tool_names = agent.tool_names();
        for &mem_tool in MEMORY_TOOLS {
            assert!(
                !tool_names.contains(&mem_tool),
                "rehydrated ACP session must NOT expose `{mem_tool}` — found in tool list: {tool_names:?}"
            );
        }
    }

    #[tokio::test]
    async fn killed_acp_session_does_not_rehydrate_from_durable_store() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = make_acp_test_config(&tmp);
        let data_dir = config.data_dir.clone();
        let (dispatcher, sessions, _chat_backend, acp_store) =
            make_persistence_test_dispatcher(config, &data_dir);

        let sid = "acp-killed-001";
        dispatcher
            .handle_session_new_for_test(&json!({
                "agent_alias": "test-agent",
                "exclude_memory": true,
                "chat_mode": "acp",
                "session_id": sid,
            }))
            .await
            .expect("session/new should succeed");

        assert!(
            sessions.get_agent(sid).await.is_some(),
            "freshly created session must be live in memory"
        );
        assert!(
            acp_store.load_session(sid).unwrap().is_some(),
            "durable row must exist before kill"
        );

        dispatcher
            .handle_session_kill(&json!({ "session_id": sid }))
            .await
            .expect("session/kill should succeed");

        assert!(
            sessions.get_agent(sid).await.is_none(),
            "session/kill must remove the live in-memory agent"
        );
        assert!(
            acp_store.load_session(sid).unwrap().is_some(),
            "session/kill must preserve durable history"
        );

        let recovered = dispatcher.rehydrate_reaped_session(sid).await;
        assert!(
            recovered.is_none(),
            "admin-killed ACP sessions must stay killed instead of rehydrating \
             from durable history on the next prompt"
        );
        assert!(
            sessions.get_agent(sid).await.is_none(),
            "failed rehydrate must leave the session absent from memory"
        );
    }

    #[tokio::test]
    async fn killed_acp_session_new_resume_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = make_acp_test_config(&tmp);
        let data_dir = config.data_dir.clone();
        let (dispatcher, sessions, _chat_backend, acp_store) =
            make_persistence_test_dispatcher(config, &data_dir);

        let sid = "acp-killed-resume-001";
        dispatcher
            .handle_session_new_for_test(&json!({
                "agent_alias": "test-agent",
                "exclude_memory": true,
                "chat_mode": "acp",
                "session_id": sid,
            }))
            .await
            .expect("session/new should create the original ACP session");
        dispatcher
            .handle_session_kill(&json!({ "session_id": sid }))
            .await
            .expect("session/kill should succeed");

        let resumed = dispatcher
            .handle_session_new_for_test(&json!({
                "agent_alias": "test-agent",
                "exclude_memory": true,
                "chat_mode": "acp",
                "session_id": sid,
            }))
            .await;

        assert!(
            resumed.is_err(),
            "session/new must not revive a killed ACP session"
        );
        assert!(
            sessions.get_agent(sid).await.is_none(),
            "rejected resume must leave the killed session absent from memory"
        );
        assert!(
            acp_store.load_session(sid).unwrap().is_some(),
            "rejected resume must preserve durable history"
        );
    }

    #[tokio::test]
    async fn acp_session_new_resume_rejects_agent_alias_mismatch() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = make_acp_test_config(&tmp);
        let data_dir = config.data_dir.clone();
        let (dispatcher, sessions, _chat_backend, acp_store) =
            make_persistence_test_dispatcher(config, &data_dir);

        let sid = "acp-alias-mismatch-001";
        acp_store
            .create_session(sid, "test-agent", "/tmp/test-agent")
            .expect("test should seed durable ACP session");

        let resumed = dispatcher
            .handle_session_new_for_test(&json!({
                "agent_alias": "test-agent-2",
                "exclude_memory": true,
                "chat_mode": "acp",
                "session_id": sid,
            }))
            .await;

        let err = resumed.expect_err("session/new must reject ACP alias mismatches");
        assert_eq!(err.code, INVALID_PARAMS);
        assert!(
            sessions.get_agent(sid).await.is_none(),
            "rejected mismatched resume must not create a live session"
        );
        assert!(
            acp_store.load_session(sid).unwrap().is_some(),
            "rejected mismatched resume must preserve durable history"
        );
    }

    /// chat_mode omitted (or =chat) creates rows via session_backend,
    /// acp-sessions.db stays empty for that session_id.
    #[tokio::test]
    async fn chat_session_new_writes_to_chat_backend_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = make_acp_test_config(&tmp);
        let data_dir = config.data_dir.clone();
        let (dispatcher, _sessions, chat_backend, acp_store) =
            make_persistence_test_dispatcher(config, &data_dir);

        let sid = "chat-routing-001";
        let params = json!({
            "agent_alias": "test-agent",
            "session_id": sid,
        });

        dispatcher
            .handle_session_new_for_test(&params)
            .await
            .expect("session/new should succeed");

        assert!(
            acp_store.load_session(sid).unwrap().is_none(),
            "Chat session must NOT touch acp_session_store"
        );

        let key = format!("rpc_{sid}");
        let metadata = chat_backend.list_sessions_with_metadata();
        let entry = metadata
            .iter()
            .find(|m| m.key == key)
            .expect("Chat session must be registered in session_backend metadata");
        assert_eq!(
            entry.agent_alias.as_deref(),
            Some("test-agent"),
            "Chat session must stamp its agent_alias in session_backend (got: {:?})",
            entry.agent_alias
        );
    }

    // ── config/set secret-routing ────────────────────────────────

    fn make_config_set_test_dispatcher(config: zeroclaw_config::schema::Config) -> RpcDispatcher {
        use zeroclaw_infra::session_queue::SessionActorQueue;
        let queue = Arc::new(SessionActorQueue::new(4, 10, 60));
        let sessions = Arc::new(crate::rpc::session::SessionStore::new(16, queue));
        let ctx = RpcContext::minimal(config, Arc::clone(&sessions));
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let mut dispatcher = RpcDispatcher::new(ctx, tx, "test-peer".into());
        dispatcher.authenticated = true;
        dispatcher
    }

    fn make_secret_test_config(tmp: &tempfile::TempDir) -> zeroclaw_config::schema::Config {
        let mut cfg = zeroclaw_config::schema::Config {
            config_path: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
            ..Default::default()
        };
        cfg.create_map_key("providers.models.anthropic", "default")
            .expect("create anthropic.default");
        cfg
    }

    // `make_config_set_test_dispatcher` takes the `Config` by value and adds no
    // isolation of its own, and a successful `config/set` falls through to
    // `flush_config()` -> `save_dirty()`. Always hand it a TempDir-rooted config
    // (`make_secret_test_config`), never a bare `Config::default()`.

    #[tokio::test]
    async fn config_set_does_not_materialize_resource_keyed_rate_alias() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dispatcher = make_config_set_test_dispatcher(make_secret_test_config(&tmp));
        let res = dispatcher
            .handle_config_set(&json!({
                "prop": "cost.rates.providers.models.openai.gpt-5.input_per_mtok",
                "value": 1.5
            }))
            .await;
        assert!(res.is_err(), "unknown rate path must not be auto-created");
        assert!(
            dispatcher
                .ctx
                .config
                .read()
                .cost
                .rates
                .providers
                .models
                .openai
                .is_empty()
        );
    }

    #[tokio::test]
    async fn config_set_on_dotted_resource_id_does_not_plant_phantom_sibling() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut cfg = make_secret_test_config(&tmp);
        cfg.create_map_key("cost.rates.providers.models.openai", "gpt-4.1")
            .expect("create the dotted resource id");
        let dispatcher = make_config_set_test_dispatcher(cfg);
        let res = dispatcher
            .handle_config_set(&json!({
                "prop": "cost.rates.providers.models.openai.gpt-4.1.input_per_mtok",
                "value": 1.5
            }))
            .await;
        assert!(res.is_ok(), "editing a real dotted rate must work: {res:?}");
        assert_eq!(
            dispatcher
                .ctx
                .config
                .read()
                .get_map_keys("cost.rates.providers.models.openai")
                .expect("known section"),
            vec!["gpt-4.1".to_string()],
        );
    }

    #[tokio::test]
    async fn config_set_still_materializes_operator_chosen_alias() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dispatcher = make_config_set_test_dispatcher(make_secret_test_config(&tmp));
        let res = dispatcher
            .handle_config_set(&json!({
                "prop": "channels.telegram.newbot.bot_token",
                "value": "tok"
            }))
            .await;
        assert!(
            res.is_ok(),
            "operator-chosen alias must still vivify: {res:?}"
        );
        assert!(
            dispatcher
                .ctx
                .config
                .read()
                .channels
                .telegram
                .contains_key("newbot")
        );
    }

    #[tokio::test]
    async fn config_set_writes_real_secret_through_set_prop() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dispatcher = make_config_set_test_dispatcher(make_secret_test_config(&tmp));
        let params = json!({
            "prop": "providers.models.anthropic.default.api_key",
            "value": "sk-real-test-key"
        });
        let res = dispatcher.handle_config_set(&params).await;
        assert!(res.is_ok(), "config/set must accept a real secret: {res:?}");
        let cfg = dispatcher.ctx.config.read().clone();
        let stored = cfg
            .providers
            .models
            .anthropic
            .get("default")
            .and_then(|e| e.base.api_key.clone());
        assert_eq!(
            stored.as_deref(),
            Some("sk-real-test-key"),
            "real secret must land in memory as plaintext"
        );
    }

    #[tokio::test]
    async fn config_set_refreshes_memory_embedder_on_provider_change() {
        use zeroclaw_infra::session_queue::SessionActorQueue;

        let tmp = tempfile::TempDir::new().unwrap();
        let mut cfg = zeroclaw_config::schema::Config {
            config_path: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
            ..Default::default()
        };
        cfg.create_map_key("providers.models.openai", "default")
            .expect("create openai.default");
        // Memory embeddings resolve from openai.default.
        cfg.memory.embedding_provider = "openai.default".into();
        cfg.memory.embedding_model = "text-embedding-3-small".into();
        cfg.memory.embedding_dimensions = 1536;

        // Long-lived handle constructed with the Noop embedder (dims 0), exactly
        // the stale state the bug leaves behind.
        let mem = Arc::new(zeroclaw_memory::SqliteMemory::new("default", tmp.path()).unwrap());
        assert_eq!(mem.embedder_dimensions(), 0, "starts on the Noop embedder");

        let queue = Arc::new(SessionActorQueue::new(4, 10, 60));
        let sessions = Arc::new(crate::rpc::session::SessionStore::new(16, queue));
        let ctx = RpcContext::minimal_with_memory(
            cfg,
            Arc::clone(&sessions),
            Arc::clone(&mem) as Arc<dyn zeroclaw_api::memory_traits::Memory>,
        );
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let mut dispatcher = RpcDispatcher::new(ctx, tx, "test-peer".into());
        dispatcher.authenticated = true;

        let params = json!({
            "prop": "providers.models.openai.default.api_key",
            "value": "sk-rotated-key"
        });
        let res = dispatcher.handle_config_set(&params).await;
        assert!(res.is_ok(), "config/set must succeed: {res:?}");

        assert_eq!(
            mem.embedder_dimensions(),
            1536,
            "config/set on the memory embedding provider must hot-swap the live \
             handle's embedder to the resolved provider (#8359)"
        );
    }

    #[tokio::test]
    async fn config_set_routes_memory_embeds_to_new_endpoint_and_key() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use zeroclaw_api::memory_traits::{Memory, MemoryCategory};
        use zeroclaw_infra::session_queue::SessionActorQueue;

        let mock_a = MockServer::start().await;
        let mock_b = MockServer::start().await;
        let embed_body = serde_json::json!({ "data": [{ "embedding": [0.1, 0.2, 0.3] }] });
        for server in [&mock_a, &mock_b] {
            Mock::given(method("POST"))
                .and(path("/v1/embeddings"))
                .respond_with(ResponseTemplate::new(200).set_body_json(embed_body.clone()))
                .mount(server)
                .await;
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let mut cfg = zeroclaw_config::schema::Config {
            config_path: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
            ..Default::default()
        };
        cfg.create_map_key("providers.models.openai", "default")
            .expect("create openai.default");
        cfg.set_prop_persistent("providers.models.openai.default.uri", &mock_a.uri())
            .expect("set initial uri");
        cfg.set_prop_persistent("providers.models.openai.default.api_key", "key-a")
            .expect("set initial key");
        cfg.memory.embedding_provider = "openai.default".into();
        cfg.memory.embedding_model = "text-embedding-3-small".into();
        cfg.memory.embedding_dimensions = 3;

        // Long-lived handle built via the real factory → embedder points at A.
        let mem: Arc<dyn Memory> = Arc::from(
            zeroclaw_memory::create_memory_with_storage_and_routes(
                &cfg.memory,
                &cfg.embedding_routes,
                cfg.resolve_active_storage(),
                &cfg.data_dir,
                None,
                Some(&cfg.providers.models),
            )
            .expect("build memory"),
        );

        let queue = Arc::new(SessionActorQueue::new(4, 10, 60));
        let sessions = Arc::new(crate::rpc::session::SessionStore::new(16, queue));
        let ctx = RpcContext::minimal_with_memory(cfg, Arc::clone(&sessions), Arc::clone(&mem));
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let mut dispatcher = RpcDispatcher::new(ctx, tx, "test-peer".into());
        dispatcher.authenticated = true;

        // Rotate the provider profile's endpoint + key through config/set.
        for (prop, value) in [
            ("providers.models.openai.default.uri", mock_b.uri()),
            (
                "providers.models.openai.default.api_key",
                "key-b".to_string(),
            ),
        ] {
            let res = dispatcher
                .handle_config_set(&json!({ "prop": prop, "value": value }))
                .await;
            assert!(res.is_ok(), "config/set {prop} must succeed: {res:?}");
        }

        // Next embed must go to the NEW endpoint with the NEW key.
        mem.store("k1", "hello wiremock", MemoryCategory::Core, None)
            .await
            .expect("store");

        let b_reqs = mock_b
            .received_requests()
            .await
            .expect("request recording enabled");
        let hit = b_reqs
            .iter()
            .find(|r| r.url.path() == "/v1/embeddings")
            .expect("new endpoint (mock B) must receive the embed after config/set");
        let auth = hit
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert_eq!(auth, "Bearer key-b", "embed must carry the rotated api key");

        let a_reqs = mock_a.received_requests().await.unwrap_or_default();
        assert!(
            a_reqs.iter().all(|r| r.url.path() != "/v1/embeddings"),
            "stale endpoint (mock A) must not receive embeds after the refresh"
        );
    }

    #[tokio::test]
    async fn config_set_refreshes_live_agent_session_memory() {
        use zeroclaw_api::memory_traits::Memory;
        use zeroclaw_infra::session_queue::SessionActorQueue;

        let tmp = tempfile::TempDir::new().unwrap();
        let mut cfg = zeroclaw_config::schema::Config {
            config_path: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
            ..Default::default()
        };
        cfg.create_map_key("providers.models.openai", "default")
            .expect("create openai.default");
        cfg.memory.embedding_provider = "openai.default".into();
        cfg.memory.embedding_model = "text-embedding-3-small".into();
        cfg.memory.embedding_dimensions = 1536;

        // The agent's memory: AgentScopedMemory wrapping a concrete SQLite
        // backend (Noop, dims 0) — the stale state config/set must repair.
        let sqlite = Arc::new(zeroclaw_memory::SqliteMemory::new("agent", tmp.path()).unwrap());
        assert_eq!(sqlite.embedder_dimensions(), 0);
        let scoped: Arc<dyn Memory> = Arc::new(zeroclaw_memory::AgentScopedMemory::new(
            Arc::clone(&sqlite) as Arc<dyn Memory>,
            "agent-uuid",
            Vec::<String>::new(),
        ));

        let agent = crate::agent::agent::Agent::builder()
            .model_provider(Box::new(DummyModelProvider))
            .tools(vec![])
            .memory(scoped)
            .observer(Arc::new(crate::observability::noop::NoopObserver))
            .tool_dispatcher(Box::new(crate::agent::dispatcher::NativeToolDispatcher))
            .workspace_dir(std::env::temp_dir())
            .build()
            .expect("agent builds");

        let queue = Arc::new(SessionActorQueue::new(4, 10, 60));
        let sessions = Arc::new(crate::rpc::session::SessionStore::new(16, queue));
        sessions
            .insert(
                "s1".into(),
                crate::rpc::session::RpcSession::new(
                    agent,
                    "agent",
                    ".",
                    crate::rpc::types::ChatMode::Chat,
                ),
            )
            .await
            .unwrap();

        let ctx = RpcContext::minimal(cfg, Arc::clone(&sessions));
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let mut dispatcher = RpcDispatcher::new(ctx, tx, "test-peer".into());
        dispatcher.authenticated = true;

        // Full RPC path: this schedules the live-agent memory refresh.
        let res = dispatcher
            .handle_config_set(&json!({
                "prop": "providers.models.openai.default.api_key",
                "value": "sk-rotated"
            }))
            .await;
        assert!(res.is_ok(), "config/set must succeed: {res:?}");

        // The agent refresh is spawned; wait (bounded) for it to land.
        let mut dims = 0;
        for _ in 0..200 {
            dims = sqlite.embedder_dimensions();
            if dims == 1536 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            dims, 1536,
            "config/set must refresh the live session's per-agent memory embedder \
             through the AgentScopedMemory wrapper (#8359)"
        );
    }

    #[tokio::test]
    async fn config_set_rejects_masked_secret_value() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut cfg = make_secret_test_config(&tmp);
        cfg.providers
            .models
            .anthropic
            .get_mut("default")
            .unwrap()
            .base
            .api_key = Some("sk-live-secret".into());
        let dispatcher = make_config_set_test_dispatcher(cfg);

        for masked in [zeroclaw_config::traits::MASKED_SECRET, "****", ""] {
            let params = json!({
                "prop": "providers.models.anthropic.default.api_key",
                "value": masked
            });
            let res = dispatcher.handle_config_set(&params).await;
            assert!(
                res.is_err(),
                "config/set must refuse masked/empty secret (`{masked}`), got: {res:?}"
            );
        }

        let cfg_after = dispatcher.ctx.config.read().clone();
        let stored = cfg_after
            .providers
            .models
            .anthropic
            .get("default")
            .and_then(|e| e.base.api_key.clone());
        assert_eq!(
            stored.as_deref(),
            Some("sk-live-secret"),
            "live secret must NOT be clobbered by a masked write"
        );
    }

    #[tokio::test]
    async fn config_set_handles_dynamic_http_request_secret_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dispatcher = make_config_set_test_dispatcher(zeroclaw_config::schema::Config {
            config_path: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
            ..Default::default()
        });

        let params = json!({
            "prop": "http_request.secrets.api_token",
            "value": "Bearer runtime-secret"
        });
        let res = dispatcher.handle_config_set(&params).await;
        assert!(
            res.is_ok(),
            "config/set must accept a real dynamic http_request secret: {res:?}"
        );
        let cfg = dispatcher.ctx.config.read().clone();
        assert_eq!(
            cfg.http_request
                .secrets
                .get("api_token")
                .map(String::as_str),
            Some("Bearer runtime-secret")
        );

        for masked in [zeroclaw_config::traits::MASKED_SECRET, "****", ""] {
            let params = json!({
                "prop": "http_request.secrets.next_token",
                "value": masked
            });
            let res = dispatcher.handle_config_set(&params).await;
            assert!(
                res.is_err(),
                "config/set must reject masked/empty dynamic secret (`{masked}`), got: {res:?}"
            );
        }
        let cfg_after = dispatcher.ctx.config.read().clone();
        assert!(
            !cfg_after.http_request.secrets.contains_key("next_token"),
            "masked dynamic writes must not materialize a secret key"
        );
    }

    #[tokio::test]
    async fn config_set_non_secret_field_still_uses_set_prop() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dispatcher = make_config_set_test_dispatcher(make_secret_test_config(&tmp));
        let params = json!({
            "prop": "providers.models.anthropic.default.model",
            "value": "claude-sonnet-4-5"
        });
        let res = dispatcher.handle_config_set(&params).await;
        assert!(res.is_ok(), "non-secret set must succeed: {res:?}");
        let cfg = dispatcher.ctx.config.read().clone();
        let stored = cfg
            .providers
            .models
            .anthropic
            .get("default")
            .and_then(|e| e.base.model.clone());
        assert_eq!(stored.as_deref(), Some("claude-sonnet-4-5"));
    }

    #[tokio::test]
    async fn config_set_persists_mcp_server_field_to_disk() {
        use zeroclaw_config::schema::{McpServerConfig, McpTransport};

        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");

        // Seed an on-disk file with an existing `[[mcp.servers]]`
        // entry so `save_dirty` exercises its incremental path
        // (the new-file fallback to full `save` would mask the
        // dirty-path bug because it serializes the whole struct).
        let seed = format!(
            "schema_version = {}\n\n\
             [[mcp.servers]]\n\
             name = \"fs\"\n\
             transport = \"stdio\"\n\
             command = \"/usr/bin/mcp-fs\"\n",
            zeroclaw_config::migration::CURRENT_SCHEMA_VERSION
        );
        std::fs::write(&config_path, &seed).unwrap();

        let mut cfg = zeroclaw_config::schema::Config {
            config_path: config_path.clone(),
            data_dir: tmp.path().join("data"),
            ..Default::default()
        };
        cfg.mcp.servers.push(McpServerConfig {
            name: "fs".into(),
            transport: McpTransport::Stdio,
            command: "/usr/bin/mcp-fs".into(),
            ..Default::default()
        });
        let dispatcher = make_config_set_test_dispatcher(cfg);

        // The exact wire shape the dashboard / TUI send for a
        // per-field edit on an `[[mcp.servers]]` entry.
        let params = json!({
            "prop": "mcp.servers.fs.command",
            "value": "/usr/local/bin/mcp-fs"
        });
        let res = dispatcher.handle_config_set(&params).await;
        assert!(
            res.is_ok(),
            "config/set on a per-field mcp.servers path must succeed: {res:?}"
        );

        // In-memory landed (this is what the UI sees — and what was
        // working before; the bug was strictly on the save side).
        let in_memory = dispatcher
            .ctx
            .config
            .read()
            .mcp
            .servers
            .iter()
            .find(|s| s.name == "fs")
            .map(|s| s.command.clone());
        assert_eq!(
            in_memory.as_deref(),
            Some("/usr/local/bin/mcp-fs"),
            "in-memory mutation must land — this part already worked"
        );

        // The regression: the same value must reach disk.
        let written = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            written.contains("/usr/local/bin/mcp-fs"),
            "config/set on `mcp.servers.fs.command` must persist to disk; \
             on-disk file still reads:\n{written}"
        );
        assert!(
            !written.contains("/usr/bin/mcp-fs"),
            "stale command must be overwritten on disk; got:\n{written}"
        );
        // The natural-key field itself must stay on disk so the entry
        // remains addressable on the next load.
        assert!(
            written.contains("name = \"fs\""),
            "natural-key `name` must survive the incremental save; got:\n{written}"
        );

        let reparsed: zeroclaw_config::schema::Config = toml::from_str(&written).unwrap();
        let entry = reparsed
            .mcp
            .servers
            .iter()
            .find(|s| s.name == "fs")
            .expect("reparse must surface the entry by natural key");
        assert_eq!(entry.command, "/usr/local/bin/mcp-fs");
    }

    fn make_model_refresh_test_config(tmp: &tempfile::TempDir) -> zeroclaw_config::schema::Config {
        use std::collections::HashMap;
        use zeroclaw_config::schema::{AliasedAgentConfig, Config, RiskProfileConfig};

        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut config = Config {
            config_path: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
            ..Default::default()
        };
        let provider = config
            .providers
            .models
            .ensure("openai", "test-provider")
            .expect("openai provider slot exists");
        provider.api_key = Some("test-key".into());
        provider.uri = Some("http://127.0.0.1:1".into());
        provider.model = Some("old-model".into());
        provider.temperature = Some(0.2);

        config.agents = HashMap::from([(
            "test-agent".to_string(),
            AliasedAgentConfig {
                enabled: true,
                model_provider: "openai.test-provider".into(),
                risk_profile: "test-profile".into(),
                ..Default::default()
            },
        )]);
        config
            .risk_profiles
            .insert("test-profile".into(), RiskProfileConfig::default());
        config
            .runtime_profiles
            .insert("default".into(), Default::default());
        config
    }

    async fn create_model_refresh_test_session(
        dispatcher: &RpcDispatcher,
        tmp: &tempfile::TempDir,
    ) -> String {
        let session_res = dispatcher
            .handle_session_new_for_test(&json!({
                "agent_alias": "test-agent",
                "cwd": tmp.path().join("workspace"),
            }))
            .await
            .expect("session/new should create the agent");
        session_res
            .get("session_id")
            .and_then(|v| v.as_str())
            .expect("session/new result includes session_id")
            .to_string()
    }

    async fn model_name_for_session(dispatcher: &RpcDispatcher, session_id: &str) -> String {
        let agent = dispatcher
            .ctx
            .sessions
            .get_agent(session_id)
            .await
            .expect("session agent exists");
        agent.lock().await.attribution_fields().2
    }

    async fn temperature_for_session(dispatcher: &RpcDispatcher, session_id: &str) -> Option<f64> {
        let agent = dispatcher
            .ctx
            .sessions
            .get_agent(session_id)
            .await
            .expect("session agent exists");
        agent.lock().await.temperature_for_test()
    }

    async fn wait_for_model_name(dispatcher: &RpcDispatcher, session_id: &str, expected: &str) {
        for _ in 0..50 {
            if model_name_for_session(dispatcher, session_id).await == expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            model_name_for_session(dispatcher, session_id).await,
            expected
        );
    }

    async fn wait_for_temperature(
        dispatcher: &RpcDispatcher,
        session_id: &str,
        expected: Option<f64>,
    ) {
        for _ in 0..50 {
            if temperature_for_session(dispatcher, session_id).await == expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            temperature_for_session(dispatcher, session_id).await,
            expected
        );
    }

    #[tokio::test]
    async fn config_set_agent_model_provider_refreshes_bound_live_session() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut cfg = make_model_refresh_test_config(&tmp);

        let other = cfg
            .providers
            .models
            .ensure("openai", "other-provider")
            .expect("openai provider slot exists");
        other.api_key = Some("test-key".into());
        other.uri = Some("http://127.0.0.1:1".into());
        other.model = Some("other-model".into());
        other.temperature = Some(0.2);

        let dispatcher = make_config_set_test_dispatcher(cfg);
        let session_id = create_model_refresh_test_session(&dispatcher, &tmp).await;
        assert_eq!(
            model_name_for_session(&dispatcher, &session_id).await,
            "old-model",
            "session must start on the currently-bound provider's model"
        );

        let res = dispatcher
            .handle_config_set(&json!({
                "prop": "agents.test-agent.model_provider",
                "value": "openai.other-provider"
            }))
            .await;
        assert!(
            res.is_ok(),
            "config/set agents.<alias>.model_provider must succeed: {res:?}"
        );

        wait_for_model_name(&dispatcher, &session_id, "other-model").await;
    }

    #[tokio::test]
    async fn existing_session_uses_reloaded_structured_history_cap() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = make_model_refresh_test_config(&tmp);
        config
            .agents
            .get_mut("test-agent")
            .expect("test agent exists")
            .runtime_profile = "reloadable".into();
        config.runtime_profiles.insert(
            "reloadable".into(),
            zeroclaw_config::schema::RuntimeProfileConfig {
                max_history_messages: Some(10),
                ..Default::default()
            },
        );

        let dispatcher = make_config_set_test_dispatcher(config);
        let session_id = create_model_refresh_test_session(&dispatcher, &tmp).await;
        dispatcher
            .ctx
            .config
            .write()
            .runtime_profiles
            .get_mut("reloadable")
            .expect("runtime profile exists")
            .max_history_messages = Some(2);

        let agent = dispatcher
            .ctx
            .sessions
            .get_agent(&session_id)
            .await
            .expect("session agent exists");
        let mut agent = agent.lock().await;
        let event = agent.seed_history_with_event(&[
            ChatMessage::user("old user"),
            ChatMessage::assistant("old assistant"),
            ChatMessage::user("new user"),
            ChatMessage::assistant("new assistant"),
        ]);

        assert!(
            matches!(event, Some(TurnEvent::HistoryTrimmed { .. })),
            "an existing session must observe the reloaded runtime-profile cap"
        );
        assert!(!agent.history().iter().any(|message| matches!(
            message,
            zeroclaw_providers::ConversationMessage::Chat(chat) if chat.content == "old user"
        )));
        assert!(agent.history().iter().any(|message| matches!(
            message,
            zeroclaw_providers::ConversationMessage::Chat(chat)
                if chat.content == "new assistant"
        )));
    }

    #[tokio::test]
    async fn config_set_provider_model_refreshes_matching_live_session() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dispatcher = make_config_set_test_dispatcher(make_model_refresh_test_config(&tmp));
        let session_id = create_model_refresh_test_session(&dispatcher, &tmp).await;
        assert_eq!(
            model_name_for_session(&dispatcher, &session_id).await,
            "old-model"
        );

        let res = dispatcher
            .handle_config_set(&json!({
                "prop": "providers.models.openai.test-provider.model",
                "value": "new-model"
            }))
            .await;
        assert!(res.is_ok(), "config/set must succeed: {res:?}");

        wait_for_model_name(&dispatcher, &session_id, "new-model").await;
    }

    #[tokio::test]
    async fn config_set_provider_refresh_failure_does_not_fail_saved_write() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dispatcher = make_config_set_test_dispatcher(make_model_refresh_test_config(&tmp));
        let session_id = create_model_refresh_test_session(&dispatcher, &tmp).await;
        assert_eq!(
            model_name_for_session(&dispatcher, &session_id).await,
            "old-model"
        );

        let res = dispatcher
            .handle_config_set(&json!({
                "prop": "providers.models.openai.test-provider.model",
                "value": ""
            }))
            .await;
        assert!(
            res.is_ok(),
            "config/set must report the saved write even if live refresh cannot rebuild: {res:?}"
        );
        let cfg = dispatcher.ctx.config.read().clone();
        let stored = cfg
            .providers
            .models
            .openai
            .get("test-provider")
            .and_then(|e| e.base.model.clone());
        assert_eq!(
            stored, None,
            "config/set must still persist the requested provider-profile clear"
        );
        assert_eq!(
            model_name_for_session(&dispatcher, &session_id).await,
            "old-model",
            "failed live refresh must leave the existing session provider intact"
        );
    }

    #[tokio::test]
    async fn session_configure_invalid_provider_does_not_commit_override() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dispatcher = make_config_set_test_dispatcher(make_model_refresh_test_config(&tmp));
        let session_id = create_model_refresh_test_session(&dispatcher, &tmp).await;
        assert_eq!(
            model_name_for_session(&dispatcher, &session_id).await,
            "old-model"
        );

        let res = dispatcher
            .handle_session_configure(&json!({
                "session_id": session_id,
                "overrides": {
                    "model_provider": "openai.missing"
                }
            }))
            .await;
        assert!(
            res.is_err(),
            "invalid provider switch must fail before mutating session overrides"
        );

        let overrides = dispatcher
            .ctx
            .sessions
            .get_overrides(&session_id)
            .await
            .expect("session still exists");
        assert_eq!(
            overrides.model_provider, None,
            "failed provider switch must not leave a stale override behind"
        );
        assert_eq!(
            model_name_for_session(&dispatcher, &session_id).await,
            "old-model",
            "failed provider switch must leave the live agent unchanged"
        );
    }

    #[tokio::test]
    async fn session_configure_blank_model_fields_do_not_commit_override() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dispatcher = make_config_set_test_dispatcher(make_model_refresh_test_config(&tmp));
        let session_id = create_model_refresh_test_session(&dispatcher, &tmp).await;
        assert_eq!(
            model_name_for_session(&dispatcher, &session_id).await,
            "old-model"
        );

        for model in ["", "   "] {
            let res = dispatcher
                .handle_session_configure(&json!({
                    "session_id": session_id,
                    "overrides": {
                        "model": model
                    }
                }))
                .await;
            let err = res.expect_err("blank model must be rejected");
            assert_eq!(err.code, INVALID_PARAMS);

            let overrides = dispatcher
                .ctx
                .sessions
                .get_overrides(&session_id)
                .await
                .expect("session still exists");
            assert_eq!(
                overrides.model, None,
                "failed model switch must not leave a stale override behind"
            );
            assert_eq!(
                model_name_for_session(&dispatcher, &session_id).await,
                "old-model",
                "failed model switch must leave the live agent unchanged"
            );
        }

        for model_provider in ["", "   "] {
            let res = dispatcher
                .handle_session_configure(&json!({
                    "session_id": session_id,
                    "overrides": {
                        "model_provider": model_provider
                    }
                }))
                .await;
            let err = res.expect_err("blank model_provider must be rejected");
            assert_eq!(err.code, INVALID_PARAMS);

            let overrides = dispatcher
                .ctx
                .sessions
                .get_overrides(&session_id)
                .await
                .expect("session still exists");
            assert_eq!(
                overrides.model_provider, None,
                "failed provider switch must not leave a stale override behind"
            );
            assert_eq!(
                model_name_for_session(&dispatcher, &session_id).await,
                "old-model",
                "failed provider switch must leave the live agent unchanged"
            );
        }
    }

    #[tokio::test]
    async fn config_set_provider_temperature_refreshes_matching_live_session() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dispatcher = make_config_set_test_dispatcher(make_model_refresh_test_config(&tmp));
        let session_id = create_model_refresh_test_session(&dispatcher, &tmp).await;
        assert_eq!(
            temperature_for_session(&dispatcher, &session_id).await,
            Some(0.2)
        );

        let res = dispatcher
            .handle_config_set(&json!({
                "prop": "providers.models.openai.test-provider.temperature",
                "value": 0.4
            }))
            .await;
        assert!(res.is_ok(), "config/set must succeed: {res:?}");

        wait_for_temperature(&dispatcher, &session_id, Some(0.4)).await;
    }

    #[tokio::test]
    async fn config_set_provider_refresh_preserves_session_temperature_override() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dispatcher = make_config_set_test_dispatcher(make_model_refresh_test_config(&tmp));
        let session_id = create_model_refresh_test_session(&dispatcher, &tmp).await;
        let merged = dispatcher
            .ctx
            .sessions
            .set_overrides(
                &session_id,
                crate::rpc::session::SessionOverrides {
                    temperature: Some(0.6),
                    ..Default::default()
                },
            )
            .await
            .expect("session override applies");
        assert_eq!(merged.temperature, Some(0.6));

        let res = dispatcher
            .handle_config_set(&json!({
                "prop": "providers.models.openai.test-provider.model",
                "value": "new-model"
            }))
            .await;
        assert!(res.is_ok(), "config/set must succeed: {res:?}");

        wait_for_model_name(&dispatcher, &session_id, "new-model").await;
        assert_eq!(
            temperature_for_session(&dispatcher, &session_id).await,
            Some(0.6),
            "session temperature override must win over provider profile temperature"
        );
    }

    #[tokio::test]
    async fn config_delete_provider_temperature_refreshes_matching_live_session() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dispatcher = make_config_set_test_dispatcher(make_model_refresh_test_config(&tmp));
        let session_id = create_model_refresh_test_session(&dispatcher, &tmp).await;
        assert_eq!(
            temperature_for_session(&dispatcher, &session_id).await,
            Some(0.2)
        );

        let res = dispatcher
            .handle_config_delete(&json!({
                "prop": "providers.models.openai.test-provider.temperature",
            }))
            .await;
        assert!(res.is_ok(), "config/delete must succeed: {res:?}");

        wait_for_temperature(&dispatcher, &session_id, None).await;
    }

    // -----------------------------------------------------------------------
    // session/cancel ownership enforcement — the spurious-cancel bug
    // -----------------------------------------------------------------------

    /// Build two dispatchers sharing one `RpcContext`/`SessionStore`. Mirrors
    /// production where each TUI connection gets its own dispatcher with its
    /// own `tui_id`, all routing to the same shared session map.
    fn make_two_dispatchers_sharing_context(
        config: zeroclaw_config::schema::Config,
    ) -> (
        RpcDispatcher,
        RpcDispatcher,
        Arc<crate::rpc::session::SessionStore>,
    ) {
        use zeroclaw_infra::session_queue::SessionActorQueue;
        let queue = Arc::new(SessionActorQueue::new(4, 10, 60));
        let sessions = Arc::new(crate::rpc::session::SessionStore::new(16, queue));
        let ctx = RpcContext::minimal(config, Arc::clone(&sessions));
        let (tx_a, _rx_a) = tokio::sync::mpsc::channel(64);
        let (tx_b, _rx_b) = tokio::sync::mpsc::channel(64);
        let dispatcher_a = RpcDispatcher::new(Arc::clone(&ctx), tx_a, "test-peer-a:pid=1".into());
        let dispatcher_b = RpcDispatcher::new(ctx, tx_b, "test-peer-b:pid=2".into());
        (dispatcher_a, dispatcher_b, sessions)
    }

    async fn create_session_with_owner(
        dispatcher: &mut RpcDispatcher,
        sessions: &Arc<crate::rpc::session::SessionStore>,
        session_id: &str,
        owner_tui_id: &str,
    ) -> tokio_util::sync::CancellationToken {
        dispatcher.set_tui_id_for_test(Some(owner_tui_id.to_string()));
        let params = json!({
            "agent_alias": "test-agent",
            "session_id": session_id,
        });
        dispatcher
            .handle_session_new_for_test(&params)
            .await
            .expect("session/new must succeed");

        let stamped_owner = sessions
            .session_owner_tui_id(session_id)
            .await
            .expect("session must exist after session/new");
        assert_eq!(
            stamped_owner.as_deref(),
            Some(owner_tui_id),
            "harness invariant: session/new must stamp owner_tui_id from the \
             caller's tui_id; if this fails, the ownership tests below are \
             measuring nothing"
        );

        let token = tokio_util::sync::CancellationToken::new();
        sessions.register_cancel_token(session_id, token.clone());
        token
    }

    fn make_dispatcher_with_capture(
        config: zeroclaw_config::schema::Config,
    ) -> (
        RpcDispatcher,
        tokio::sync::mpsc::Receiver<String>,
        Arc<crate::rpc::session::SessionStore>,
    ) {
        use zeroclaw_infra::session_queue::SessionActorQueue;
        let queue = Arc::new(SessionActorQueue::new(4, 10, 60));
        let sessions = Arc::new(crate::rpc::session::SessionStore::new(16, queue));
        let ctx = RpcContext::minimal(config, Arc::clone(&sessions));
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let dispatcher = RpcDispatcher::new(ctx, tx, "test-peer-cap:pid=1".into());
        (dispatcher, rx, sessions)
    }

    #[tokio::test]
    async fn session_prompt_on_missing_session_emits_turn_complete_failed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = make_acp_test_config(&tmp);
        let (dispatcher, mut rx, _sessions) = make_dispatcher_with_capture(config);

        let result = dispatcher
            .handle_session_prompt(&json!({
                "session_id": "gone-id",
                "prompt": "anything",
            }))
            .await;
        assert!(
            result.is_err(),
            "missing session must still produce an RPC error for legacy \
             request-form callers; the new behaviour is the additional \
             notification, not replacing the error"
        );

        // The notification must already be queued on the writer channel by
        // the time `handle_session_prompt` returns. `try_recv` rules out
        // any test flakiness from racing with a spawned task.
        let raw = rx.try_recv().expect(
            "handle_session_prompt must emit a session/update TurnComplete \
             notification before returning on missing-session — without it \
             the TUI's `working` state never clears and the next prompt is \
             the production freeze",
        );
        let v: serde_json::Value = serde_json::from_str(&raw).expect("notification must be JSON");
        assert_eq!(v["method"], notification::SESSION_UPDATE);
        assert_eq!(v["params"]["session_id"], "gone-id");
        assert_eq!(
            v["params"]["outcome"], "failed",
            "missing-session is not Completed and not Cancelled — it is a \
             distinct Failed verdict. Folding it into Cancelled would lie \
             about whether the user pressed Esc."
        );
    }

    #[tokio::test]
    async fn session_cancel_from_distinct_non_owner_dispatcher_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = make_acp_test_config(&tmp);
        let (mut dispatcher_a, mut dispatcher_b, sessions) =
            make_two_dispatchers_sharing_context(config);

        let token =
            create_session_with_owner(&mut dispatcher_a, &sessions, "sess-owned-by-tui-A", "tui-A")
                .await;

        dispatcher_b.set_tui_id_for_test(Some("tui-B".to_string()));
        let result = dispatcher_b
            .handle_session_cancel(&json!({
                "session_id": "sess-owned-by-tui-A",
            }))
            .await;

        let err = result.expect_err(
            "a cancel from a dispatcher whose tui_id does not match the \
             session's owner_tui_id must be refused",
        );
        assert_ne!(
            err.code, SESSION_NOT_FOUND,
            "the rejection must NOT be reported as SESSION_NOT_FOUND — the \
             session DOES exist; reporting NOT_FOUND would hide the \
             ownership violation behind a benign-looking error"
        );
        assert!(
            !token.is_cancelled(),
            "the owner's cancel token must remain un-fired — the rightful \
             owner's turn must survive a mis-targeted cancel from another TUI"
        );
    }

    #[tokio::test]
    async fn session_cancel_from_anonymous_dispatcher_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = make_acp_test_config(&tmp);
        let (mut dispatcher_a, mut dispatcher_b, sessions) =
            make_two_dispatchers_sharing_context(config);

        let token =
            create_session_with_owner(&mut dispatcher_a, &sessions, "sess-owned-by-tui-A", "tui-A")
                .await;

        // dispatcher_b never set its tui_id — fresh connection, no
        // initialize handshake yet.
        dispatcher_b.set_tui_id_for_test(None);
        let result = dispatcher_b
            .handle_session_cancel(&json!({
                "session_id": "sess-owned-by-tui-A",
            }))
            .await;

        let err = result.expect_err("anonymous cancel must be refused");
        assert_ne!(err.code, SESSION_NOT_FOUND);
        assert!(
            !token.is_cancelled(),
            "anonymous cancel must not fire the token"
        );
    }

    #[tokio::test]
    async fn session_cancel_from_owner_dispatcher_still_works() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = make_acp_test_config(&tmp);
        let (mut dispatcher_a, _dispatcher_b, sessions) =
            make_two_dispatchers_sharing_context(config);

        let token =
            create_session_with_owner(&mut dispatcher_a, &sessions, "sess-owned-by-tui-A", "tui-A")
                .await;

        // Same dispatcher, same tui_id that created the session.
        let result = dispatcher_a
            .handle_session_cancel(&json!({
                "session_id": "sess-owned-by-tui-A",
            }))
            .await;

        assert!(
            result.is_ok(),
            "owner cancel must succeed; got: {:?}",
            result.err()
        );
        assert!(
            token.is_cancelled(),
            "owner cancel must fire the session's cancel token"
        );
    }

    // ── Missing-session regression: close / delete must not fabricate
    //    session_end for sessions that never existed ──────────────────

    struct EndCountingHook {
        end_count: Arc<std::sync::atomic::AtomicU32>,
    }

    impl EndCountingHook {
        fn new() -> (Self, Arc<std::sync::atomic::AtomicU32>) {
            let count = Arc::new(std::sync::atomic::AtomicU32::new(0));
            (
                Self {
                    end_count: count.clone(),
                },
                count,
            )
        }
    }

    #[async_trait]
    impl crate::hooks::HookHandler for EndCountingHook {
        fn name(&self) -> &str {
            "end-counter"
        }
        fn priority(&self) -> i32 {
            0
        }
        async fn on_session_end(&self, _session_id: &str, _channel: &str) {
            self.end_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn session_close_missing_session_does_not_fire_session_end() {
        let queue = Arc::new(zeroclaw_infra::session_queue::SessionActorQueue::new(
            4, 10, 60,
        ));
        let sessions = Arc::new(crate::rpc::session::SessionStore::new(16, queue));
        let mut runner = crate::hooks::HookRunner::new();
        let (_hook, end_count) = EndCountingHook::new();
        runner.register(Box::new(_hook));
        let ctx = Arc::new(crate::rpc::context::RpcContext {
            config: Arc::new(parking_lot::RwLock::new(
                zeroclaw_config::schema::Config::default(),
            )),
            config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
            sessions: Arc::clone(&sessions),
            session_backend: None,
            memory: None,
            cost_tracker: None,
            event_tx: None,
            reload_tx: None,
            gateway_shutdown_tx: None,
            approval_pending: Arc::new(crate::rpc::context::ApprovalPendingMap::default()),
            tui_registry: Arc::new(crate::rpc::tui_identity::TuiRegistry::new_unsigned()),
            acp_session_store: None,
            sop_engine: None,
            sop_audit: None,
            hooks: Some(Arc::new(runner)),
        });
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let dispatcher = RpcDispatcher::new(ctx, tx, "test-peer-close:pid=1".into());

        let result = dispatcher
            .handle_session_close(&serde_json::json!({"session_id": "ghost-close"}))
            .await;
        assert!(result.is_err(), "close on nonexistent session must error");

        assert_eq!(
            end_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "session_end must not fire when close targets a missing session"
        );
    }

    #[tokio::test]
    async fn session_delete_missing_session_does_not_fire_session_end() {
        let queue = Arc::new(zeroclaw_infra::session_queue::SessionActorQueue::new(
            4, 10, 60,
        ));
        let sessions = Arc::new(crate::rpc::session::SessionStore::new(16, queue));
        let mut runner = crate::hooks::HookRunner::new();
        let (_hook, end_count) = EndCountingHook::new();
        runner.register(Box::new(_hook));
        let ctx = Arc::new(crate::rpc::context::RpcContext {
            config: Arc::new(parking_lot::RwLock::new(
                zeroclaw_config::schema::Config::default(),
            )),
            config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
            sessions: Arc::clone(&sessions),
            session_backend: None,
            memory: None,
            cost_tracker: None,
            event_tx: None,
            reload_tx: None,
            gateway_shutdown_tx: None,
            approval_pending: Arc::new(crate::rpc::context::ApprovalPendingMap::default()),
            tui_registry: Arc::new(crate::rpc::tui_identity::TuiRegistry::new_unsigned()),
            acp_session_store: None,
            sop_engine: None,
            sop_audit: None,
            hooks: Some(Arc::new(runner)),
        });
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let dispatcher = RpcDispatcher::new(ctx, tx, "test-peer-delete:pid=1".into());

        let result = dispatcher
            .handle_session_delete(&serde_json::json!({"session_id": "ghost-delete"}))
            .await;
        assert!(
            result.is_ok(),
            "delete on nonexistent session should succeed"
        );

        assert_eq!(
            end_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "session_end must not fire when delete targets a missing session"
        );
    }

    // ── Positive lifecycle regression: close on a real session must fire
    //    session_end so that configured hooks observe RPC lifecycles ──

    struct DummyModelProvider;

    #[async_trait]
    impl zeroclaw_api::model_provider::ModelProvider for DummyModelProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            Ok("ok".to_string())
        }
    }

    impl zeroclaw_api::attribution::Attributable for DummyModelProvider {
        fn role(&self) -> zeroclaw_api::attribution::Role {
            zeroclaw_api::attribution::Role::Provider(
                zeroclaw_api::attribution::ProviderKind::Model(
                    zeroclaw_api::attribution::ModelProviderKind::Custom,
                ),
            )
        }
        fn alias(&self) -> &str {
            "dummy"
        }
    }

    #[tokio::test]
    async fn session_close_real_session_fires_session_end_hook() {
        let queue = Arc::new(zeroclaw_infra::session_queue::SessionActorQueue::new(
            4, 10, 60,
        ));
        let sessions = Arc::new(crate::rpc::session::SessionStore::new(16, queue));
        let sid = "real-session-close-hook".to_string();

        // Build a minimal agent and insert it into the store so the
        // dispatcher sees a live session.
        let agent = crate::agent::agent::Agent::builder()
            .model_provider(Box::new(DummyModelProvider))
            .tools(vec![])
            .memory(Arc::new(zeroclaw_memory::NoneMemory::new("none")))
            .observer(Arc::new(crate::observability::noop::NoopObserver))
            .tool_dispatcher(Box::new(crate::agent::dispatcher::NativeToolDispatcher))
            .workspace_dir(std::env::temp_dir())
            .build()
            .expect("minimal Agent should build");
        let rpc_session = crate::rpc::session::RpcSession::new(
            agent,
            "test-agent",
            std::env::temp_dir().to_str().unwrap(),
            crate::rpc::types::ChatMode::Chat,
        );
        sessions.insert(sid.clone(), rpc_session).await.unwrap();

        // Wire a counting hook.
        let mut runner = crate::hooks::HookRunner::new();
        let (_hook, end_count) = EndCountingHook::new();
        runner.register(Box::new(_hook));

        let ctx = Arc::new(crate::rpc::context::RpcContext {
            config: Arc::new(parking_lot::RwLock::new(
                zeroclaw_config::schema::Config::default(),
            )),
            config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
            sessions: Arc::clone(&sessions),
            session_backend: None,
            memory: None,
            cost_tracker: None,
            event_tx: None,
            reload_tx: None,
            gateway_shutdown_tx: None,
            approval_pending: Arc::new(crate::rpc::context::ApprovalPendingMap::default()),
            tui_registry: Arc::new(crate::rpc::tui_identity::TuiRegistry::new_unsigned()),
            acp_session_store: None,
            sop_engine: None,
            sop_audit: None,
            hooks: Some(Arc::new(runner)),
        });
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let dispatcher = RpcDispatcher::new(ctx, tx, "test-peer-real-close:pid=1".into());

        // Close the real session.
        let result = dispatcher
            .handle_session_close(&serde_json::json!({"session_id": sid}))
            .await;
        assert!(result.is_ok(), "close on real session must succeed");

        assert_eq!(
            end_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "session_end must fire when a real session is closed"
        );
    }

    // ── config_write_lock races ─────────────────────────────────

    fn make_two_provider_test_config(tmp: &tempfile::TempDir) -> zeroclaw_config::schema::Config {
        let mut cfg = zeroclaw_config::schema::Config {
            config_path: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
            ..Default::default()
        };
        cfg.create_map_key("providers.models.anthropic", "default")
            .expect("create anthropic.default");
        cfg.create_map_key("providers.models.openai", "default")
            .expect("create openai.default");
        cfg
    }

    fn provider_model(config: &zeroclaw_config::schema::Config, provider: &str) -> Option<String> {
        match provider {
            "anthropic" => config
                .providers
                .models
                .anthropic
                .get("default")
                .and_then(|e| e.base.model.clone()),
            "openai" => config
                .providers
                .models
                .openai
                .get("default")
                .and_then(|e| e.base.model.clone()),
            other => panic!("unexpected provider {other}"),
        }
    }

    /// Regression test: `flush_config` used to clone the live config,
    /// await `save_dirty()` on the clone, then swap the clone back over the
    /// live config wholesale. A write landed on `ctx.config` while that save
    /// was in flight was silently erased by the swap even though it was
    /// never given a chance to reach disk.
    ///
    /// This drives `flush_config` to its first real yield point (inside
    /// `save_dirty`'s disk I/O) with a single manual poll, proving the
    /// snapshot has already been cloned out from under the live lock, lands
    /// a second write directly on the live config, then lets the flush
    /// finish. Against the old swap-based body this fails: P2 disappears
    /// from live config when the stale snapshot lands on top of it. It was
    /// confirmed to fail this way by temporarily reverting `flush_config` to
    /// the swap-based body and re-running this test.
    #[tokio::test]
    async fn flush_config_preserves_write_landed_during_save() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        let mut cfg = make_two_provider_test_config(&tmp);
        cfg.set_prop_persistent("providers.models.anthropic.default.model", "p1-value")
            .expect("mark P1 dirty");

        let dispatcher = make_config_set_test_dispatcher(cfg);
        let guard = Arc::clone(&dispatcher.ctx.config_write_lock)
            .lock_owned()
            .await;
        let mut flush_fut = Box::pin(dispatcher.flush_config(&guard));

        // Single manual poll: past the synchronous clone-and-capture prefix
        // of `flush_config`, into `save_dirty`'s disk I/O, which must
        // suspend rather than resolve synchronously.
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        let first_poll = std::future::Future::poll(flush_fut.as_mut(), &mut cx);
        assert!(
            first_poll.is_pending(),
            "flush must suspend on save_dirty's disk I/O for this test to interleave"
        );

        // P2: lands directly on the live config while the flush above is
        // mid-save over its own (now stale) snapshot.
        {
            let mut live = dispatcher.ctx.config.write();
            live.set_prop_persistent("providers.models.openai.default.model", "p2-value")
                .expect("mark P2 dirty");
        }

        flush_fut.await.expect("flush of P1 must still succeed");
        drop(guard);

        let live = dispatcher.ctx.config.read();
        assert_eq!(
            provider_model(&live, "openai").as_deref(),
            Some("p2-value"),
            "P2, written while the flush was mid-save, must survive in live config"
        );
        assert!(
            live.dirty_paths
                .contains("providers.models.openai.default.model"),
            "P2 must still be dirty -- this flush never saved it"
        );
        assert!(
            !live
                .dirty_paths
                .contains("providers.models.anthropic.default.model"),
            "P1 was actually saved, so it must no longer be dirty"
        );
        drop(live);

        let on_disk = std::fs::read_to_string(&config_path).unwrap();
        let reparsed: zeroclaw_config::schema::Config = toml::from_str(&on_disk).unwrap();
        assert_eq!(
            provider_model(&reparsed, "anthropic").as_deref(),
            Some("p1-value"),
            "P1 must have reached disk; on-disk file:\n{on_disk}"
        );
    }

    /// Shared blocking scaffold: hold `config_write_lock`, spawn the RPC
    /// call, assert it stays parked across a bounded sleep-free yield loop,
    /// release, and return the task's result for caller-specific asserts.
    async fn assert_rpc_blocks_on_config_write_lock<F>(
        ctx: Arc<RpcContext>,
        rpc_call: F,
        blocked_msg: &'static str,
    ) -> RpcResult
    where
        F: std::future::Future<Output = RpcResult> + Send + 'static,
    {
        let guard = Arc::clone(&ctx.config_write_lock).lock_owned().await;
        let task = zeroclaw_spawn::spawn!(rpc_call);

        // Bounded, sleep-free: the handler must not race ahead of the
        // externally held guard no matter how many times it's polled.
        for _ in 0..50 {
            tokio::task::yield_now().await;
            assert!(!task.is_finished(), "{blocked_msg}");
        }

        drop(guard);
        task.await.expect("blocked RPC task must not panic")
    }

    #[tokio::test]
    async fn config_set_blocks_while_config_write_lock_held() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        let cfg = make_two_provider_test_config(&tmp);
        let dispatcher = make_config_set_test_dispatcher(cfg);
        let ctx = Arc::clone(&dispatcher.ctx);

        let params = json!({
            "prop": "providers.models.anthropic.default.model",
            "value": "blocked-value"
        });
        let result = assert_rpc_blocks_on_config_write_lock(
            ctx,
            async move { dispatcher.handle_config_set(&params).await },
            "config/set must block on config_write_lock while it is held",
        )
        .await;
        assert!(
            result.is_ok(),
            "config/set must succeed once the guard is released: {result:?}"
        );

        let on_disk = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            on_disk.contains("blocked-value"),
            "config/set must persist once unblocked; on-disk file:\n{on_disk}"
        );
    }

    #[tokio::test]
    async fn concurrent_config_sets_both_persist() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        let cfg = make_two_provider_test_config(&tmp);
        let (dispatcher_a, dispatcher_b, _sessions) = make_two_dispatchers_sharing_context(cfg);

        let params_a = json!({
            "prop": "providers.models.anthropic.default.model",
            "value": "concurrent-a"
        });
        let params_b = json!({
            "prop": "providers.models.openai.default.model",
            "value": "concurrent-b"
        });

        let (result_a, result_b) = tokio::join!(
            dispatcher_a.handle_config_set(&params_a),
            dispatcher_b.handle_config_set(&params_b),
        );
        assert!(
            result_a.is_ok(),
            "first config/set must succeed: {result_a:?}"
        );
        assert!(
            result_b.is_ok(),
            "second config/set must succeed: {result_b:?}"
        );

        let live = dispatcher_a.ctx.config.read();
        assert_eq!(
            provider_model(&live, "anthropic").as_deref(),
            Some("concurrent-a")
        );
        assert_eq!(
            provider_model(&live, "openai").as_deref(),
            Some("concurrent-b")
        );
        drop(live);

        let on_disk = std::fs::read_to_string(&config_path).unwrap();
        let reparsed: zeroclaw_config::schema::Config = toml::from_str(&on_disk).unwrap();
        assert_eq!(
            provider_model(&reparsed, "anthropic").as_deref(),
            Some("concurrent-a"),
            "provider a's set must have reached disk:\n{on_disk}"
        );
        assert_eq!(
            provider_model(&reparsed, "openai").as_deref(),
            Some("concurrent-b"),
            "provider b's set must have reached disk:\n{on_disk}"
        );
    }

    /// Same blocking pattern as `config_set_blocks_while_config_write_lock_held`,
    /// but for the `save_and_swap_config` path used by alias rename: proves
    /// alias rename and config/set share the same `config_write_lock` and so
    /// can never interleave their read-mutate-flush critical sections.
    #[tokio::test]
    async fn alias_rename_serializes_with_config_set() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = make_agent_rename_test_config(&tmp);
        let config_path = config.config_path.clone();
        let data_dir = config.data_dir.clone();
        let (dispatcher, _sessions, _chat_backend, _acp_store) =
            make_persistence_test_dispatcher(config, &data_dir);
        let ctx = Arc::clone(&dispatcher.ctx);

        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let mut rename_dispatcher =
            RpcDispatcher::new(Arc::clone(&ctx), tx, "test-peer-rename".into());
        rename_dispatcher.authenticated = true;
        let params = json!({
            "path": "agents",
            "from": "alpha",
            "to": "beta"
        });
        let result = assert_rpc_blocks_on_config_write_lock(
            ctx,
            async move {
                rename_dispatcher
                    .handle_config_map_key_rename(&params)
                    .await
            },
            "alias rename must block on config_write_lock while it is held",
        )
        .await;
        assert!(
            result.is_ok(),
            "alias rename must succeed once the guard is released: {result:?}"
        );

        let live = dispatcher.ctx.config.read();
        assert!(!live.agents.contains_key("alpha"));
        assert!(live.agents.contains_key("beta"));
        drop(live);

        let on_disk = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            on_disk.contains("[agents.beta]"),
            "renamed alias must persist:\n{on_disk}"
        );
        assert!(
            !on_disk.contains("[agents.alpha]"),
            "old alias must not survive on disk:\n{on_disk}"
        );
    }

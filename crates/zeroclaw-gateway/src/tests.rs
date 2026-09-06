#[cfg(test)]
use super::*;
use async_trait::async_trait;
use axum::http::{HeaderValue, Uri};
use axum::response::IntoResponse;
use http_body_util::BodyExt;
use parking_lot::{Mutex, RwLock};
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(feature = "channel-whatsapp-cloud")]
use zeroclaw_api::channel::ChannelMessage;
use zeroclaw_memory::{Memory, MemoryCategory, MemoryEntry};
use zeroclaw_providers::ModelProvider;
use zeroclaw_runtime::agent::loop_::{mcp_tool_access_policy, register_eager_mcp_tool_if_allowed};

#[test]
fn default_agent_alias_picks_smallest_enabled_and_is_deterministic() {
    use zeroclaw_config::schema::AliasedAgentConfig;

    let enabled = || AliasedAgentConfig {
        enabled: true,
        ..AliasedAgentConfig::default()
    };

    // No agents -> no default.
    let mut config = Config::default();
    assert_eq!(default_agent_alias(&config), None);

    // Insertion order is deliberately not alphabetical; `config.agents` is
    // a HashMap whose iteration order is randomized per process. The pick
    // must still be the lexicographically smallest ENABLED alias so the
    // Tools page seeds the same agent on every restart.
    config.agents.insert("zeta".to_string(), enabled());
    config.agents.insert("alpha".to_string(), enabled());
    config.agents.insert("mid".to_string(), enabled());
    assert_eq!(default_agent_alias(&config).as_deref(), Some("alpha"));

    // A smaller-but-disabled alias is skipped (omission is not a grant).
    config.agents.insert(
        "aaa_disabled".to_string(),
        AliasedAgentConfig {
            enabled: false,
            ..AliasedAgentConfig::default()
        },
    );
    assert_eq!(default_agent_alias(&config).as_deref(), Some("alpha"));
}

/// Generate a random hex secret at runtime to avoid hard-coded cryptographic values.
fn generate_test_secret() -> String {
    let bytes: [u8; 32] = rand::random();
    hex::encode(bytes)
}

struct NamedMcpMockTool(&'static str);
zeroclaw_api::mock_tool_attribution!(NamedMcpMockTool);
#[async_trait]
impl tools::Tool for NamedMcpMockTool {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "mcp mock"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<tools::ToolResult> {
        Ok(tools::ToolResult {
            success: true,
            output: tools::ToolOutput::default(),
            error: None,
        })
    }
}

#[test]
fn gateway_excluded_tools_drops_denied_mcp_tool() {
    let policy = SecurityPolicy {
        excluded_tools: Some(vec!["aa_mcp__find_items".to_string()]),
        workspace_dir: std::env::temp_dir(),
        ..SecurityPolicy::default()
    };
    let mcp_policy = mcp_tool_access_policy(&policy, None);
    let mut gw_tools: Vec<Box<dyn tools::Tool>> = Vec::new();
    let denied: std::sync::Arc<dyn tools::Tool> =
        std::sync::Arc::new(NamedMcpMockTool("aa_mcp__find_items"));
    let allowed: std::sync::Arc<dyn tools::Tool> =
        std::sync::Arc::new(NamedMcpMockTool("aa_mcp__find_npcs"));
    let registered_denied =
        register_eager_mcp_tool_if_allowed(denied, &mut gw_tools, mcp_policy.as_ref());
    let registered_allowed =
        register_eager_mcp_tool_if_allowed(allowed, &mut gw_tools, mcp_policy.as_ref());
    assert!(
        !registered_denied,
        "gateway must not register an `excluded_tools`-denied MCP tool"
    );
    assert!(
        registered_allowed,
        "gateway must register a non-denied MCP tool (allowlist auto-admit)"
    );
    let names: Vec<&str> = gw_tools.iter().map(|t| t.name()).collect();
    assert!(
        !names.contains(&"aa_mcp__find_items"),
        "denied MCP tool leaked into the gateway registry; got {names:?}"
    );
    assert!(
        names.contains(&"aa_mcp__find_npcs"),
        "allowed MCP tool missing from the gateway registry; got {names:?}"
    );
}

#[test]
fn security_body_limit_is_64kb() {
    assert_eq!(MAX_BODY_SIZE, 65_536);
}

#[test]
fn security_timeout_default_is_30_seconds() {
    assert_eq!(REQUEST_TIMEOUT_SECS, 30);
}

#[test]
fn gateway_timeout_uses_typed_config_default() {
    let cfg = zeroclaw_config::schema::GatewayConfig::default();
    assert_eq!(gateway_request_timeout_secs(&cfg), 30);
}

#[test]
fn paircode_recovery_command_includes_alternate_port() {
    assert_eq!(
        format_paircode_recovery_command("127.0.0.1", 42617),
        "zeroclaw gateway get-paircode --new --port 42617"
    );
}

#[test]
fn paircode_recovery_command_includes_specific_host_when_needed() {
    // Admin paircode routes are localhost-only, so the recovery hint must
    // not advertise a non-loopback `--host` (the admin guard would 403 it).
    // The CLI is left to fall back to its loopback default.
    assert_eq!(
        format_paircode_recovery_command("192.168.1.20", 42617),
        "zeroclaw gateway get-paircode --new --port 42617"
    );
}

#[test]
fn paircode_recovery_command_uses_loopback_for_nonloopback_host() {
    // a gateway bound to a non-loopback interface must
    // not surface a recovery hint that the localhost-only admin guard rejects.
    let cmd = format_paircode_recovery_command("192.168.1.20", 42617);
    assert!(
        !cmd.contains("192.168.1.20"),
        "recovery command must not advertise the non-loopback bound host: {cmd}"
    );
    assert!(
        !cmd.contains("--host"),
        "recovery command should omit --host so the CLI uses its loopback default: {cmd}"
    );

    let curl = format_paircode_recovery_curl("192.168.1.20", 42617, "");
    assert_eq!(
        curl, "curl -s -X POST http://127.0.0.1:42617/admin/paircode/new",
        "curl fallback must target loopback, not the non-loopback bound host"
    );
    assert!(
        !curl.contains("192.168.1.20"),
        "curl fallback must not advertise the non-loopback bound host: {curl}"
    );

    // Path prefix is still preserved while the host is normalized.
    assert_eq!(
        format_paircode_recovery_curl("192.168.1.20", 42617, "/gw"),
        "curl -s -X POST http://127.0.0.1:42617/gw/admin/paircode/new"
    );
}

#[test]
fn paircode_recovery_curl_targets_running_instance() {
    assert_eq!(
        format_paircode_recovery_curl("127.0.0.1", 42617, ""),
        "curl -s -X POST http://127.0.0.1:42617/admin/paircode/new"
    );
}

#[test]
fn already_paired_notice_states_no_code_was_generated() {
    // the banner must say plainly that NO code exists
    // (already paired), not just "Pairing: ACTIVE" — otherwise the operator
    // hits the dashboard's 6-digit prompt with no code printed anywhere.
    let lines = already_paired_pairing_notice("127.0.0.1", 3001, "");
    let joined = lines.join("\n");
    assert!(
        joined.contains("already paired"),
        "notice must say the gateway is already paired: {joined}"
    );
    assert!(
        joined.contains("no new") && joined.contains("code"),
        "notice must state that no new code was generated: {joined}"
    );
}

#[test]
fn already_paired_notice_includes_recovery_command_and_curl() {
    // The notice is the single source of truth for the on-demand recovery
    // commands; it must reuse the loopback-safe builders so the banner and
    // any future surface never drift from's no-`--host` rule.
    let lines = already_paired_pairing_notice("192.168.1.20", 3001, "/gw");
    let joined = lines.join("\n");
    assert!(
        joined.contains(&format_paircode_recovery_command("192.168.1.20", 3001)),
        "notice must surface the get-paircode recovery command: {joined}"
    );
    assert!(
        joined.contains(&format_paircode_recovery_curl("192.168.1.20", 3001, "/gw")),
        "notice must surface the curl fallback (honoring the path prefix): {joined}"
    );
    // never advertise the non-loopback bound host in the hint.
    assert!(
        !joined.contains("192.168.1.20"),
        "notice must not advertise the non-loopback bound host: {joined}"
    );
}

#[test]
fn paircode_recovery_curl_normalizes_unspecified_bind_hosts() {
    assert_eq!(
        format_paircode_recovery_curl("0.0.0.0", 42617, ""),
        "curl -s -X POST http://127.0.0.1:42617/admin/paircode/new"
    );
    assert_eq!(
        format_paircode_recovery_curl("::", 42617, ""),
        "curl -s -X POST http://127.0.0.1:42617/admin/paircode/new"
    );
}

#[test]
fn paircode_recovery_curl_preserves_actual_loopback_hosts() {
    assert_eq!(
        format_paircode_recovery_curl("localhost", 42617, ""),
        "curl -s -X POST http://localhost:42617/admin/paircode/new"
    );
    assert_eq!(
        format_paircode_recovery_curl("::1", 42617, ""),
        "curl -s -X POST http://[::1]:42617/admin/paircode/new"
    );
}

#[test]
fn paircode_recovery_curl_preserves_path_prefix() {
    assert_eq!(
        format_paircode_recovery_curl("127.0.0.1", 42617, "/gw"),
        "curl -s -X POST http://127.0.0.1:42617/gw/admin/paircode/new"
    );
}

/// Build an AppState wired with a real pairing guard, on-disk config path,
/// and an optional device registry so the admin paircode handler's
/// revoke + persist paths can be exercised end to end.
fn admin_paircode_state(
    tmp: &tempfile::TempDir,
    require_pairing: bool,
    with_registry: bool,
) -> AppState {
    let data_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&data_dir).unwrap();
    let config = Config {
        data_dir: data_dir.clone(),
        config_path: tmp.path().join("config.toml"),
        ..Config::default()
    };
    let registry = with_registry.then(|| Arc::new(api_pairing::DeviceRegistry::new(&data_dir)));
    AppState {
        config: Arc::new(RwLock::new(config)),
        config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
        model_provider: Arc::new(MockModelProvider::default()),
        model: "test-model".into(),
        temperature: None,
        mem: Arc::new(MockMemory),
        memory_strategy: Arc::new(DefaultMemoryStrategy::with_config(
            Arc::new(MockMemory),
            zeroclaw_config::schema::MemoryConfig::default(),
            std::path::PathBuf::new(),
        )),
        companion_store: None,
        auto_save: false,
        webhook_secret_hash: None,
        pairing: Arc::new(PairingGuard::new(require_pairing, &[])),
        trust_forwarded_headers: false,
        rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100)),
        auth_limiter: Arc::new(auth_rate_limit::AuthRateLimiter::new()),
        idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp: HashMap::new(),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp_app_secret: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq_signing_secrets: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk_webhook_secret: HashMap::new(),
        #[cfg(feature = "channel-wati")]
        wati: HashMap::new(),
        #[cfg(feature = "channel-email")]
        gmail_push: None,
        observer: Arc::new(zeroclaw_runtime::observability::NoopObserver),
        tools_registry: Arc::new(Vec::new()),
        tools_registry_by_agent: Arc::new(std::collections::HashMap::new()),
        cost_tracker: None,
        event_tx: tokio::sync::broadcast::channel(16).0,
        event_buffer: Arc::new(sse::EventBuffer::new(16)),
        shutdown_tx: tokio::sync::watch::channel(false).0,
        reload_tx: None,
        #[cfg(feature = "nodes")]
        node_registry: Arc::new(nodes::NodeRegistry::new(16)),
        #[cfg(feature = "nodes")]
        mdns_peer_registry: nodes::mdns::MdnsPeerRegistry::default(),
        path_prefix: String::new(),
        web_dist_dir: None,
        session_backend: None,
        session_queue: std::sync::Arc::new(crate::session_queue::SessionActorQueue::new(
            8, 30, 600,
        )),
        device_registry: registry,
        pending_pairings: None,
        canvas_store: CanvasStore::new(),
        cancel_tokens: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        pending_reload: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        tui_registry: None,
        #[cfg(feature = "webauthn")]
        webauthn: None,
    }
}

fn spa_fallback_state(tmp: &tempfile::TempDir) -> AppState {
    let dist_dir = tmp.path().join("web").join("dist");
    std::fs::create_dir_all(&dist_dir).unwrap();
    std::fs::write(
        dist_dir.join("index.html"),
        r#"<!DOCTYPE html><html><head></head><body>dashboard shell</body></html>"#,
    )
    .unwrap();

    let mut state = admin_paircode_state(tmp, false, false);
    state.web_dist_dir = Some(dist_dir);
    state
}

async fn spa_fallback_response(path: &'static str, state: AppState) -> axum::response::Response {
    static_files::handle_spa_fallback(State(state), Uri::from_static(path)).await
}

/// Pair a device into both the pairing guard and the device registry,
/// returning the plaintext token so the test can assert it is revoked.
async fn pair_device(state: &AppState, device_id: &str) -> String {
    let code = state
        .pairing
        .generate_new_pairing_code()
        .expect("pairing enabled");
    let token = state
        .pairing
        .try_pair(&code, device_id)
        .await
        .unwrap()
        .unwrap();
    state
        .device_registry
        .as_ref()
        .unwrap()
        .register(
            PairingGuard::token_hash(&token),
            api_pairing::DeviceInfo {
                id: device_id.to_string(),
                name: None,
                device_type: None,
                paired_at: chrono::Utc::now(),
                last_seen: chrono::Utc::now(),
                ip_address: None,
                capabilities: None,
            },
        )
        .expect("test device registry insert");
    token
}

async fn admin_paircode_response_json(
    result: Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)>,
) -> (StatusCode, serde_json::Value) {
    let response = result.into_response();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    (status, json)
}

#[tokio::test]
async fn admin_paircode_new_without_rotate_keeps_existing_tokens() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = admin_paircode_state(&tmp, true, true);
    let token = pair_device(&state, "dev-a").await;

    let (status, json) = admin_paircode_response_json(
        handle_admin_paircode_new(
            State(state.clone()),
            test_connect_info(),
            Query(AdminPaircodeQuery::default()),
        )
        .await,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(json["pairing_code"].is_string());
    assert!(
        state.pairing.is_authenticated(&token),
        "add-another-client path must not revoke existing tokens"
    );
}

#[tokio::test]
async fn admin_paircode_new_rotate_all_revokes_everything() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = admin_paircode_state(&tmp, true, true);
    let token_a = pair_device(&state, "dev-a").await;
    let token_b = pair_device(&state, "dev-b").await;

    let (status, json) = admin_paircode_response_json(
        handle_admin_paircode_new(
            State(state.clone()),
            test_connect_info(),
            Query(AdminPaircodeQuery {
                rotate: Some("all".into()),
            }),
        )
        .await,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(json["pairing_code"].is_string());
    assert!(!state.pairing.is_authenticated(&token_a));
    assert!(!state.pairing.is_authenticated(&token_b));
    assert!(
        state.config.read().gateway.paired_tokens.is_empty(),
        "rotate=all must persist an empty token set"
    );
    assert!(
        state
            .device_registry
            .as_ref()
            .unwrap()
            .list()
            .expect("test device registry list")
            .is_empty(),
        "rotate=all must clear the device registry"
    );
}

#[tokio::test]
async fn admin_paircode_new_rotate_device_revokes_one() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = admin_paircode_state(&tmp, true, true);
    let token_a = pair_device(&state, "dev-a").await;
    let token_b = pair_device(&state, "dev-b").await;

    let (status, json) = admin_paircode_response_json(
        handle_admin_paircode_new(
            State(state.clone()),
            test_connect_info(),
            Query(AdminPaircodeQuery {
                rotate: Some("dev-a".into()),
            }),
        )
        .await,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(json["pairing_code"].is_string());
    assert!(!state.pairing.is_authenticated(&token_a));
    assert!(
        state.pairing.is_authenticated(&token_b),
        "targeted rotate must not touch other devices"
    );
    let old_hash = PairingGuard::token_hash(&token_a);
    assert!(
        !state
            .config
            .read()
            .gateway
            .paired_tokens
            .contains(&old_hash)
    );
}

#[tokio::test]
async fn admin_paircode_new_rotate_unknown_device_is_not_found() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = admin_paircode_state(&tmp, true, true);
    let token = pair_device(&state, "dev-a").await;

    let (status, _json) = admin_paircode_response_json(
        handle_admin_paircode_new(
            State(state.clone()),
            test_connect_info(),
            Query(AdminPaircodeQuery {
                rotate: Some("ghost".into()),
            }),
        )
        .await,
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        state.pairing.is_authenticated(&token),
        "a not-found rotate must not revoke any token"
    );
}

#[tokio::test]
async fn admin_paircode_new_pairing_disabled_is_bad_request() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = admin_paircode_state(&tmp, false, false);

    let (status, json) = admin_paircode_response_json(
        handle_admin_paircode_new(
            State(state),
            test_connect_info(),
            Query(AdminPaircodeQuery {
                rotate: Some("all".into()),
            }),
        )
        .await,
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["success"], false);
}

#[tokio::test]
async fn admin_paircode_new_rejects_remote_peer() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = admin_paircode_state(&tmp, true, true);

    let remote = ConnectInfo(SocketAddr::from(([203, 0, 113, 7], 40_000)));
    let (status, _json) = admin_paircode_response_json(
        handle_admin_paircode_new(State(state), remote, Query(AdminPaircodeQuery::default())).await,
    )
    .await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "minting a pairing code must be rejected for non-loopback peers"
    );
}

#[test]
fn long_running_request_timeout_default_is_ten_minutes() {
    assert_eq!(LONG_RUNNING_REQUEST_TIMEOUT_SECS, 600);
}

#[test]
fn long_running_request_timeout_uses_typed_config_default() {
    let cfg = zeroclaw_config::schema::GatewayConfig::default();
    assert_eq!(gateway_long_running_request_timeout_secs(&cfg), 600);
}

#[test]
fn webhook_body_requires_message_field() {
    let valid = r#"{"message": "hello"}"#;
    let parsed: Result<WebhookBody, _> = serde_json::from_str(valid);
    assert!(parsed.is_ok());
    assert_eq!(parsed.unwrap().message, "hello");

    let missing = r#"{"other": "field"}"#;
    let parsed: Result<WebhookBody, _> = serde_json::from_str(missing);
    assert!(parsed.is_err());
}

#[test]
fn whatsapp_query_fields_are_optional() {
    let q = WhatsAppVerifyQuery {
        mode: None,
        verify_token: None,
        challenge: None,
    };
    assert!(q.mode.is_none());
}

#[test]
fn app_state_is_clone() {
    fn assert_clone<T: Clone>() {}
    assert_clone::<AppState>();
}

#[tokio::test]
async fn spa_fallback_returns_json_not_html_for_unknown_api_path() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = spa_fallback_state(&tmp);

    let response = spa_fallback_response("/api/agents", state).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json")),
        "unknown API paths must not be served as HTML"
    );

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"], "not_found");
    assert_eq!(json["path"], "/api/agents");
}

#[tokio::test]
async fn spa_fallback_returns_json_for_api_root_path() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = spa_fallback_state(&tmp);

    let response = spa_fallback_response("/api", state).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["path"], "/api");
}

#[tokio::test]
async fn spa_fallback_returns_json_for_path_prefixed_api_miss() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut state = spa_fallback_state(&tmp);
    state.path_prefix = "/gw".to_string();

    let response = spa_fallback_response("/gw/api/agents", state).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["path"], "/api/agents");
}

#[tokio::test]
async fn spa_fallback_still_serves_dashboard_routes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = spa_fallback_state(&tmp);

    let response = spa_fallback_response("/config", state).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/html")),
        "dashboard routes should still receive the SPA shell"
    );

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("dashboard shell"));
}

#[tokio::test]
async fn spa_fallback_does_not_treat_api_like_spa_paths_as_api() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = spa_fallback_state(&tmp);

    let response = spa_fallback_response("/apiary", state).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/html")),
        "similarly named SPA routes should not be reserved as API paths"
    );
}

#[tokio::test]
async fn run_gateway_starts_with_zero_agents() {
    // Isolate data_dir so parallel nextest runs don't race on the
    // real ~/.zeroclaw/data
    let tmp = tempfile::TempDir::new().unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("workspace"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();

    // Default Config has no [agents.*] entries — the exact shape
    // a fresh install presents on first daemon boot.
    assert!(
        config.agents.is_empty(),
        "regression assumes default Config has no agents",
    );

    let handle = zeroclaw_spawn::spawn!(async move {
        run_gateway("127.0.0.1", 0, config, None, None, None, None, None).await
    });

    match tokio::time::timeout(
        std::time::Duration::from_millis(750),
        &mut Box::pin(async {
            // We cannot await `handle` directly because the gateway
            // never returns under normal operation; instead, peek at
            // whether it has finished by polling join with a tiny
            // budget.
            let _ = tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }),
    )
    .await
    {
        Ok(()) => {}
        Err(_) => panic!("test setup timed out before checking gateway state"),
    }

    // If the boot path errored, the task is finished and join
    // returns the error. If it's still running, abort and accept
    // boot reached the serving stage.
    if handle.is_finished() {
        let result = handle.await.expect("task did not panic");
        panic!(
            "gateway exited during boot with zero agents — must stay up for reload/quickstart: {:?}",
            result
        );
    }
    handle.abort();
}

#[tokio::test]
async fn run_gateway_starts_with_unresolved_agent_risk_profile() {
    use zeroclaw_config::schema::AliasedAgentConfig;

    // Isolate data_dir so parallel nextest runs don't race on the
    // real ~/.zeroclaw/data
    let tmp = tempfile::TempDir::new().unwrap();
    let mut config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("workspace"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();

    // Enabled agent whose `risk_profile` does not resolve. No
    // matching [risk_profiles.<key>] entry exists.
    let agent = AliasedAgentConfig {
        enabled: true,
        risk_profile: "definitely_not_configured".into(),
        ..AliasedAgentConfig::default()
    };
    config.agents.insert("fake123".to_string(), agent);

    let handle = zeroclaw_spawn::spawn!(async move {
        run_gateway("127.0.0.1", 0, config, None, None, None, None, None).await
    });

    match tokio::time::timeout(
        std::time::Duration::from_millis(750),
        &mut Box::pin(async {
            let _ = tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }),
    )
    .await
    {
        Ok(()) => {}
        Err(_) => panic!("test setup timed out before checking gateway state"),
    }

    if handle.is_finished() {
        let result = handle.await.expect("task did not panic");
        panic!(
            "gateway exited during boot when agent.risk_profile was unresolved \
             — must stay up so operator can fix via /admin/reload or /quickstart: {:?}",
            result
        );
    }
    handle.abort();
}

#[tokio::test]
async fn run_gateway_starts_with_mismatched_provider_api_key() {
    let mut config = Config::default();
    config.providers.models.anthropic.insert(
        "default".to_string(),
        zeroclaw_config::schema::AnthropicModelProviderConfig {
            base: zeroclaw_config::schema::ModelProviderConfig {
                model: Some("anthropic/claude-sonnet-4-6".to_string()),
                api_key: Some("sk-test-openai-shaped-key".to_string()),
                ..Default::default()
            },
        },
    );

    let handle = zeroclaw_spawn::spawn!(async move {
        run_gateway("127.0.0.1", 0, config, None, None, None, None, None).await
    });

    match tokio::time::timeout(
        std::time::Duration::from_millis(750),
        &mut Box::pin(async {
            let _ = tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }),
    )
    .await
    {
        Ok(()) => {}
        Err(_) => panic!("test setup timed out before checking gateway state"),
    }

    if handle.is_finished() {
        let result = handle.await.expect("task did not panic");
        panic!(
            "gateway exited during boot when seed provider API key was \
             mismatched — must stay up so operator can fix via /admin/reload \
             or /quickstart: {:?}",
            result
        );
    }
    handle.abort();
}

#[tokio::test]
async fn run_gateway_uses_external_shutdown_sender() {
    let port_probe = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = port_probe.local_addr().unwrap().port();
    drop(port_probe);

    let tmp = tempfile::TempDir::new().unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("workspace"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();

    let (shutdown_tx, _) = tokio::sync::watch::channel(false);
    let (reload_tx, _) = tokio::sync::watch::channel(false);
    let reload_controls = zeroclaw_runtime::daemon::GatewayReloadControls {
        shutdown_tx: shutdown_tx.clone(),
        reload_tx,
    };

    let handle = zeroclaw_spawn::spawn!(async move {
        run_gateway(
            "127.0.0.1",
            port,
            config,
            None,
            Some(reload_controls),
            None,
            None,
            None,
        )
        .await
    });

    let addr = format!("127.0.0.1:{port}");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if tokio::net::TcpStream::connect(&addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("gateway should accept connections before shutdown");

    shutdown_tx
        .send(true)
        .expect("external daemon-owned shutdown sender should stay connected");

    tokio::time::timeout(std::time::Duration::from_secs(2), handle)
        .await
        .expect("gateway should return after external shutdown")
        .expect("gateway task should not panic")
        .expect("gateway shutdown should be graceful");

    std::net::TcpListener::bind(("127.0.0.1", port))
        .expect("gateway should release the listener after external shutdown");
}

#[tokio::test]
async fn metrics_endpoint_returns_hint_when_prometheus_is_disabled() {
    let state = AppState {
        config: Arc::new(RwLock::new(Config::default())),
        config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
        model_provider: Arc::new(MockModelProvider::default()),
        model: "test-model".into(),
        temperature: None,
        mem: Arc::new(MockMemory),
        memory_strategy: Arc::new(DefaultMemoryStrategy::with_config(
            Arc::new(MockMemory),
            zeroclaw_config::schema::MemoryConfig::default(),
            std::path::PathBuf::new(),
        )),
        companion_store: None,
        auto_save: false,
        webhook_secret_hash: None,
        pairing: Arc::new(PairingGuard::new(false, &[])),
        trust_forwarded_headers: false,
        rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100)),
        auth_limiter: Arc::new(auth_rate_limit::AuthRateLimiter::new()),
        idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp: HashMap::new(),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp_app_secret: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq_signing_secrets: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk_webhook_secret: HashMap::new(),
        #[cfg(feature = "channel-wati")]
        wati: HashMap::new(),
        #[cfg(feature = "channel-email")]
        gmail_push: None,
        observer: Arc::new(zeroclaw_runtime::observability::NoopObserver),
        tools_registry: Arc::new(Vec::new()),
        tools_registry_by_agent: Arc::new(std::collections::HashMap::new()),
        cost_tracker: None,
        event_tx: tokio::sync::broadcast::channel(16).0,
        event_buffer: Arc::new(sse::EventBuffer::new(16)),
        shutdown_tx: tokio::sync::watch::channel(false).0,
        reload_tx: None,
        #[cfg(feature = "nodes")]
        node_registry: Arc::new(nodes::NodeRegistry::new(16)),
        #[cfg(feature = "nodes")]
        mdns_peer_registry: nodes::mdns::MdnsPeerRegistry::default(),
        path_prefix: String::new(),
        web_dist_dir: None,
        session_backend: None,
        session_queue: std::sync::Arc::new(crate::session_queue::SessionActorQueue::new(
            8, 30, 600,
        )),
        device_registry: None,
        pending_pairings: None,
        canvas_store: CanvasStore::new(),
        cancel_tokens: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        pending_reload: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        tui_registry: None,
        #[cfg(feature = "webauthn")]
        webauthn: None,
    };

    let response = handle_metrics(State(state)).await.into_response();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some(PROMETHEUS_CONTENT_TYPE)
    );

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("Prometheus backend not enabled"));
}

#[cfg(feature = "observability-prometheus")]
#[tokio::test]
async fn metrics_endpoint_renders_prometheus_output() {
    let event_tx = tokio::sync::broadcast::channel(16).0;
    let prom = zeroclaw_runtime::observability::PrometheusObserver::new();
    zeroclaw_runtime::observability::Observer::record_event(
        &prom,
        &zeroclaw_runtime::observability::ObserverEvent::HeartbeatTick,
    );

    let observer: Arc<dyn zeroclaw_runtime::observability::Observer> = Arc::new(prom);
    let state = AppState {
        config: Arc::new(RwLock::new(Config::default())),
        config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
        model_provider: Arc::new(MockModelProvider::default()),
        model: "test-model".into(),
        temperature: None,
        mem: Arc::new(MockMemory),
        memory_strategy: Arc::new(DefaultMemoryStrategy::with_config(
            Arc::new(MockMemory),
            zeroclaw_config::schema::MemoryConfig::default(),
            std::path::PathBuf::new(),
        )),
        companion_store: None,
        auto_save: false,
        webhook_secret_hash: None,
        pairing: Arc::new(PairingGuard::new(false, &[])),
        trust_forwarded_headers: false,
        rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100)),
        auth_limiter: Arc::new(auth_rate_limit::AuthRateLimiter::new()),
        idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp: HashMap::new(),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp_app_secret: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq_signing_secrets: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk_webhook_secret: HashMap::new(),
        #[cfg(feature = "channel-wati")]
        wati: HashMap::new(),
        #[cfg(feature = "channel-email")]
        gmail_push: None,
        observer,
        tools_registry: Arc::new(Vec::new()),
        tools_registry_by_agent: Arc::new(std::collections::HashMap::new()),
        cost_tracker: None,
        event_tx,
        event_buffer: Arc::new(sse::EventBuffer::new(16)),
        shutdown_tx: tokio::sync::watch::channel(false).0,
        reload_tx: None,
        #[cfg(feature = "nodes")]
        node_registry: Arc::new(nodes::NodeRegistry::new(16)),
        #[cfg(feature = "nodes")]
        mdns_peer_registry: nodes::mdns::MdnsPeerRegistry::default(),
        path_prefix: String::new(),
        web_dist_dir: None,
        session_backend: None,
        session_queue: std::sync::Arc::new(crate::session_queue::SessionActorQueue::new(
            8, 30, 600,
        )),
        device_registry: None,
        pending_pairings: None,
        canvas_store: CanvasStore::new(),
        cancel_tokens: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        pending_reload: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        tui_registry: None,
        #[cfg(feature = "webauthn")]
        webauthn: None,
    };

    let response = handle_metrics(State(state)).await.into_response();
    assert_eq!(response.status(), StatusCode::OK);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("zeroclaw_heartbeat_ticks_total 1"));
}

#[test]
fn gateway_rate_limiter_blocks_after_limit() {
    let limiter = GatewayRateLimiter::new(2, 2, 100);
    assert!(limiter.allow_pair("127.0.0.1"));
    assert!(limiter.allow_pair("127.0.0.1"));
    assert!(!limiter.allow_pair("127.0.0.1"));
}

#[test]
fn rate_limiter_sweep_removes_stale_entries() {
    let limiter = SlidingWindowRateLimiter::new(10, Duration::from_secs(60), 100);
    // Add entries for multiple IPs
    assert!(limiter.allow("ip-1"));
    assert!(limiter.allow("ip-2"));
    assert!(limiter.allow("ip-3"));

    {
        let guard = limiter.requests.lock();
        assert_eq!(guard.0.len(), 3);
    }

    // Force a sweep by backdating last_sweep
    {
        let mut guard = limiter.requests.lock();
        guard.1 = Instant::now()
            .checked_sub(Duration::from_secs(RATE_LIMITER_SWEEP_INTERVAL_SECS + 1))
            .unwrap();
        // Clear timestamps for ip-2 and ip-3 to simulate stale entries
        guard.0.get_mut("ip-2").unwrap().clear();
        guard.0.get_mut("ip-3").unwrap().clear();
    }

    // Next allow() call should trigger sweep and remove stale entries
    assert!(limiter.allow("ip-1"));

    {
        let guard = limiter.requests.lock();
        assert_eq!(guard.0.len(), 1, "Stale entries should have been swept");
        assert!(guard.0.contains_key("ip-1"));
    }
}

#[test]
fn rate_limiter_zero_limit_always_allows() {
    let limiter = SlidingWindowRateLimiter::new(0, Duration::from_secs(60), 10);
    for _ in 0..100 {
        assert!(limiter.allow("any-key"));
    }
}

#[test]
fn idempotency_store_rejects_duplicate_key() {
    let store = IdempotencyStore::new(Duration::from_secs(30), 10);
    assert!(store.record_if_new("req-1"));
    assert!(!store.record_if_new("req-1"));
    assert!(store.record_if_new("req-2"));
}

#[test]
fn rate_limiter_bounded_cardinality_evicts_oldest_key() {
    let limiter = SlidingWindowRateLimiter::new(5, Duration::from_secs(60), 2);
    assert!(limiter.allow("ip-1"));
    assert!(limiter.allow("ip-2"));
    assert!(limiter.allow("ip-3"));

    let guard = limiter.requests.lock();
    assert_eq!(guard.0.len(), 2);
    assert!(guard.0.contains_key("ip-2"));
    assert!(guard.0.contains_key("ip-3"));
}

#[test]
fn idempotency_store_bounded_cardinality_evicts_oldest_key() {
    let store = IdempotencyStore::new(Duration::from_secs(300), 2);
    assert!(store.record_if_new("k1"));
    std::thread::sleep(Duration::from_millis(2));
    assert!(store.record_if_new("k2"));
    std::thread::sleep(Duration::from_millis(2));
    assert!(store.record_if_new("k3"));

    let keys = store.keys.lock();
    assert_eq!(keys.len(), 2);
    assert!(!keys.contains_key("k1"));
    assert!(keys.contains_key("k2"));
    assert!(keys.contains_key("k3"));
}

#[test]
fn client_key_defaults_to_peer_addr_when_untrusted_proxy_mode() {
    let peer = SocketAddr::from(([10, 0, 0, 5], 42617));
    let mut headers = HeaderMap::new();
    headers.insert(
        "X-Forwarded-For",
        HeaderValue::from_static("198.51.100.10, 203.0.113.11"),
    );

    let key = client_key_from_request(Some(peer), &headers, false);
    assert_eq!(key, "10.0.0.5");
}

#[test]
fn client_key_uses_forwarded_ip_only_in_trusted_proxy_mode() {
    let peer = SocketAddr::from(([10, 0, 0, 5], 42617));
    let mut headers = HeaderMap::new();
    headers.insert(
        "X-Forwarded-For",
        HeaderValue::from_static("198.51.100.10, 203.0.113.11"),
    );

    let key = client_key_from_request(Some(peer), &headers, true);
    assert_eq!(key, "198.51.100.10");
}

#[test]
fn client_key_falls_back_to_peer_when_forwarded_header_invalid() {
    let peer = SocketAddr::from(([10, 0, 0, 5], 42617));
    let mut headers = HeaderMap::new();
    headers.insert("X-Forwarded-For", HeaderValue::from_static("garbage-value"));

    let key = client_key_from_request(Some(peer), &headers, true);
    assert_eq!(key, "10.0.0.5");
}

#[test]
fn normalize_max_keys_uses_fallback_for_zero() {
    assert_eq!(normalize_max_keys(0, 10_000), 10_000);
    assert_eq!(normalize_max_keys(0, 0), 1);
}

#[test]
fn normalize_max_keys_preserves_nonzero_values() {
    assert_eq!(normalize_max_keys(2_048, 10_000), 2_048);
    assert_eq!(normalize_max_keys(1, 10_000), 1);
}

#[tokio::test]
async fn persist_pairing_tokens_writes_config_tokens() {
    let temp = tempfile::tempdir().unwrap();
    let config_path = temp.path().join("config.toml");
    let workspace_path = temp.path().join("workspace");

    let config = Config {
        config_path: config_path.clone(),
        data_dir: workspace_path,
        ..Default::default()
    };
    config.save().await.unwrap();

    let guard = PairingGuard::new(true, &[]);
    let code = guard.pairing_code().unwrap();
    let token = guard.try_pair(&code, "test_client").await.unwrap().unwrap();
    assert!(guard.is_authenticated(&token));

    let shared_config = Arc::new(RwLock::new(config));
    let config_write_lock = Arc::new(tokio::sync::Mutex::new(()));
    Box::pin(persist_pairing_tokens(
        shared_config.clone(),
        &guard,
        config_write_lock,
    ))
    .await
    .unwrap();

    // In-memory tokens should remain as plaintext 64-char hex hashes.
    let plaintext = {
        let in_memory = shared_config.read();
        assert_eq!(in_memory.gateway.paired_tokens.len(), 1);
        in_memory.gateway.paired_tokens[0].clone()
    };
    assert_eq!(plaintext.len(), 64);
    assert!(plaintext.chars().all(|c: char| c.is_ascii_hexdigit()));

    // On disk, the token should be encrypted (secrets.encrypt defaults to true).
    let saved = tokio::fs::read_to_string(config_path).await.unwrap();
    let raw_parsed: Config = toml::from_str(&saved).unwrap();
    assert_eq!(raw_parsed.gateway.paired_tokens.len(), 1);
    let on_disk = &raw_parsed.gateway.paired_tokens[0];
    assert!(
        zeroclaw_runtime::security::SecretStore::is_encrypted(on_disk),
        "paired_token should be encrypted on disk"
    );
}

/// Unlike the `persist_and_swap` callers (which pre-acquire the witness
/// before their own read-for-modify), `persist_pairing_tokens` acquires
/// `config_write_lock` internally since it is self-contained. This
/// proves that internal acquisition still serializes it against a
/// second, concurrent config mutation the same way. A single Pending
/// poll wouldn't distinguish "blocked on `config_write_lock`" from
/// "transiently Pending on unrelated I/O", so this polls repeatedly
/// with a no-op waker while the witness stays held and asserts the
/// future never completes -- proving it stays parked on the lock for as
/// long as it's held. Once the lock is released both changes land —
/// neither clobbers the other.
#[tokio::test]
async fn persist_pairing_tokens_serializes_against_concurrent_config_write() {
    let temp = tempfile::tempdir().unwrap();
    let config = Config {
        config_path: temp.path().join("config.toml"),
        data_dir: temp.path().join("workspace"),
        ..Default::default()
    };
    config.save().await.unwrap();

    let guard = PairingGuard::new(true, &[]);
    let code = guard.pairing_code().unwrap();
    let token = guard.try_pair(&code, "test_client").await.unwrap().unwrap();
    assert!(guard.is_authenticated(&token));

    let shared_config = Arc::new(RwLock::new(config));
    let config_write_lock = Arc::new(tokio::sync::Mutex::new(()));

    // Simulate another in-flight config mutation already holding the
    // witness for its own read-mutate-save-swap section.
    let held_guard = Arc::clone(&config_write_lock).lock_owned().await;

    let mut persist_fut = Box::pin(persist_pairing_tokens(
        shared_config.clone(),
        &guard,
        config_write_lock.clone(),
    ));

    // Bounded, sleep-free: `persist_pairing_tokens` acquires the witness
    // as its very first action, so poll with a no-op waker 50 times
    // while `held_guard` stays live and assert Pending every time,
    // rather than resolving synchronously or racing ahead after a
    // single yield.
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    for _ in 0..50 {
        assert!(
            std::future::Future::poll(persist_fut.as_mut(), &mut cx).is_pending(),
            "persist_pairing_tokens must stay parked on config_write_lock \
             acquisition for as long as another writer holds it"
        );
    }

    // Land a distinct, concurrent write directly on live config while
    // persist_pairing_tokens is parked waiting for the lock.
    shared_config.write().gateway.port = 55555;

    drop(held_guard);
    persist_fut
        .await
        .expect("persist_pairing_tokens must still succeed once unblocked");

    let live = shared_config.read();
    assert_eq!(
        live.gateway.port, 55555,
        "the concurrent writer's change must survive — no lost update"
    );
    assert_eq!(
        live.gateway.paired_tokens.len(),
        1,
        "persist_pairing_tokens' own token write must also land"
    );
}

#[test]
fn webhook_memory_key_is_unique() {
    let key1 = webhook_memory_key();
    let key2 = webhook_memory_key();

    assert!(key1.starts_with("webhook_msg_"));
    assert!(key2.starts_with("webhook_msg_"));
    assert_ne!(key1, key2);
}

#[test]
fn webhook_session_id_accepts_valid() {
    let mut headers = HeaderMap::new();
    headers.insert("X-Session-Id", HeaderValue::from_static("abc-DEF_123.foo"));
    assert_eq!(webhook_session_id(&headers), Some("abc-DEF_123.foo".into()));
}

#[test]
fn webhook_session_id_trims_whitespace() {
    let mut headers = HeaderMap::new();
    headers.insert("X-Session-Id", HeaderValue::from_static("  my-session  "));
    assert_eq!(webhook_session_id(&headers), Some("my-session".into()));
}

#[test]
fn webhook_session_id_rejects_empty() {
    let mut headers = HeaderMap::new();
    headers.insert("X-Session-Id", HeaderValue::from_static(""));
    assert_eq!(webhook_session_id(&headers), None);

    headers.insert("X-Session-Id", HeaderValue::from_static("   "));
    assert_eq!(webhook_session_id(&headers), None);
}

#[test]
fn webhook_session_id_rejects_missing() {
    let headers = HeaderMap::new();
    assert_eq!(webhook_session_id(&headers), None);
}

#[test]
fn webhook_session_id_rejects_oversized() {
    let mut headers = HeaderMap::new();
    let long = "a".repeat(129);
    headers.insert("X-Session-Id", HeaderValue::from_str(&long).unwrap());
    assert_eq!(webhook_session_id(&headers), None);

    let at_limit = "b".repeat(128);
    headers.insert("X-Session-Id", HeaderValue::from_str(&at_limit).unwrap());
    assert!(webhook_session_id(&headers).is_some());
}

#[test]
fn webhook_session_id_rejects_invalid_chars() {
    let mut headers = HeaderMap::new();
    for bad in &[
        "has/slash",
        "has:colon",
        "has space",
        "has@at",
        "emoji\u{1f600}",
    ] {
        if let Ok(val) = HeaderValue::from_str(bad) {
            headers.insert("X-Session-Id", val);
            assert_eq!(webhook_session_id(&headers), None, "should reject: {bad}");
        }
    }
}

#[cfg(feature = "channel-whatsapp-cloud")]
#[test]
fn whatsapp_memory_key_includes_sender_and_message_id() {
    let msg = ChannelMessage {
        id: "wamid-123".into(),
        sender: "+1234567890".into(),
        reply_target: "+1234567890".into(),
        content: "hello".into(),
        channel: "whatsapp".into(),
        channel_alias: None,
        timestamp: 1,
        thread_ts: None,
        interruption_scope_id: None,
        attachments: vec![],
        subject: None,

        ..Default::default()
    };

    let key = whatsapp_memory_key(&msg);
    assert_eq!(key, "whatsapp_+1234567890_wamid-123");
}

#[derive(Default)]
struct MockMemory;

#[async_trait]
impl Memory for MockMemory {
    fn name(&self) -> &str {
        "mock"
    }

    async fn store(
        &self,
        _key: &str,
        _content: &str,
        _category: MemoryCategory,
        _session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn recall(
        &self,
        _query: &str,
        _limit: usize,
        _session_id: Option<&str>,
        _since: Option<&str>,
        _until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        Ok(Vec::new())
    }

    async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        Ok(None)
    }

    async fn list(
        &self,
        _category: Option<&MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        Ok(Vec::new())
    }

    async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn forget_for_agent(&self, _key: &str, _agent_id: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn count(&self) -> anyhow::Result<usize> {
        Ok(0)
    }

    async fn health_check(&self) -> bool {
        true
    }

    async fn store_with_agent(
        &self,
        _key: &str,
        _content: &str,
        _category: MemoryCategory,
        _session_id: Option<&str>,
        _namespace: Option<&str>,
        _importance: Option<f64>,
        _agent_id: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn recall_for_agents(
        &self,
        _allowed_agent_ids: &[&str],
        _query: &str,
        _limit: usize,
        _session_id: Option<&str>,
        _since: Option<&str>,
        _until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        Ok(Vec::new())
    }
}
impl ::zeroclaw_api::attribution::Attributable for MockMemory {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        ::zeroclaw_api::attribution::Role::Memory(::zeroclaw_api::attribution::MemoryKind::InMemory)
    }
    fn alias(&self) -> &str {
        "MockMemory"
    }
}

#[derive(Default)]
struct MockModelProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl ModelProvider for MockModelProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: Option<f64>,
    ) -> anyhow::Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok("ok".into())
    }
}
impl ::zeroclaw_api::attribution::Attributable for MockModelProvider {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        ::zeroclaw_api::attribution::Role::Provider(
            ::zeroclaw_api::attribution::ProviderKind::Model(
                ::zeroclaw_api::attribution::ModelProviderKind::Custom,
            ),
        )
    }
    fn alias(&self) -> &str {
        "MockModelProvider"
    }
}

#[derive(Default)]
struct CapturingObserver {
    events: Mutex<Vec<zeroclaw_runtime::observability::ObserverEvent>>,
}

impl zeroclaw_runtime::observability::Observer for CapturingObserver {
    fn record_event(&self, event: &zeroclaw_runtime::observability::ObserverEvent) {
        self.events.lock().push(event.clone());
    }

    fn record_metric(&self, _metric: &zeroclaw_runtime::observability::traits::ObserverMetric) {}

    fn name(&self) -> &str {
        "capturing"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[derive(Default)]
struct TrackingMemory {
    keys: Mutex<Vec<String>>,
}

#[async_trait]
impl Memory for TrackingMemory {
    fn name(&self) -> &str {
        "tracking"
    }

    async fn store(
        &self,
        key: &str,
        _content: &str,
        _category: MemoryCategory,
        _session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.keys.lock().push(key.to_string());
        Ok(())
    }

    async fn recall(
        &self,
        _query: &str,
        _limit: usize,
        _session_id: Option<&str>,
        _since: Option<&str>,
        _until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        Ok(Vec::new())
    }

    async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        Ok(None)
    }

    async fn list(
        &self,
        _category: Option<&MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        Ok(Vec::new())
    }

    async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn forget_for_agent(&self, _key: &str, _agent_id: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn count(&self) -> anyhow::Result<usize> {
        let size = self.keys.lock().len();
        Ok(size)
    }

    async fn health_check(&self) -> bool {
        true
    }

    async fn store_with_agent(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        _namespace: Option<&str>,
        _importance: Option<f64>,
        _agent_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.store(key, content, category, session_id).await
    }

    async fn recall_for_agents(
        &self,
        _allowed_agent_ids: &[&str],
        _query: &str,
        _limit: usize,
        _session_id: Option<&str>,
        _since: Option<&str>,
        _until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        Ok(Vec::new())
    }
}
impl ::zeroclaw_api::attribution::Attributable for TrackingMemory {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        ::zeroclaw_api::attribution::Role::Memory(::zeroclaw_api::attribution::MemoryKind::InMemory)
    }
    fn alias(&self) -> &str {
        "TrackingMemory"
    }
}

fn test_connect_info() -> ConnectInfo<SocketAddr> {
    ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 30_300)))
}

#[tokio::test]
async fn webhook_idempotency_skips_duplicate_provider_calls() {
    let provider_impl = Arc::new(MockModelProvider::default());
    let model_provider: Arc<dyn ModelProvider> = provider_impl.clone();
    let memory: Arc<dyn Memory> = Arc::new(MockMemory);

    let state = AppState {
        config: Arc::new(RwLock::new(Config::default())),
        config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
        model_provider,
        model: "test-model".into(),
        temperature: None,
        mem: memory.clone(),
        memory_strategy: Arc::new(DefaultMemoryStrategy::with_config(
            Arc::clone(&memory),
            zeroclaw_config::schema::MemoryConfig::default(),
            std::path::PathBuf::new(),
        )),
        companion_store: None,
        auto_save: false,
        webhook_secret_hash: None,
        pairing: Arc::new(PairingGuard::new(false, &[])),
        trust_forwarded_headers: false,
        rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100)),
        auth_limiter: Arc::new(auth_rate_limit::AuthRateLimiter::new()),
        idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp: HashMap::new(),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp_app_secret: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq_signing_secrets: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk_webhook_secret: HashMap::new(),
        #[cfg(feature = "channel-wati")]
        wati: HashMap::new(),
        #[cfg(feature = "channel-email")]
        gmail_push: None,
        observer: Arc::new(zeroclaw_runtime::observability::NoopObserver),
        tools_registry: Arc::new(Vec::new()),
        tools_registry_by_agent: Arc::new(std::collections::HashMap::new()),
        cost_tracker: None,
        event_tx: tokio::sync::broadcast::channel(16).0,
        event_buffer: Arc::new(sse::EventBuffer::new(16)),
        shutdown_tx: tokio::sync::watch::channel(false).0,
        reload_tx: None,
        #[cfg(feature = "nodes")]
        node_registry: Arc::new(nodes::NodeRegistry::new(16)),
        #[cfg(feature = "nodes")]
        mdns_peer_registry: nodes::mdns::MdnsPeerRegistry::default(),
        path_prefix: String::new(),
        web_dist_dir: None,
        session_backend: None,
        session_queue: std::sync::Arc::new(crate::session_queue::SessionActorQueue::new(
            8, 30, 600,
        )),
        device_registry: None,
        pending_pairings: None,
        canvas_store: CanvasStore::new(),
        cancel_tokens: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        pending_reload: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        tui_registry: None,
        #[cfg(feature = "webauthn")]
        webauthn: None,
    };

    let mut headers = HeaderMap::new();
    headers.insert("X-Idempotency-Key", HeaderValue::from_static("abc-123"));

    let body = Ok(Json(WebhookBody {
        message: "hello".into(),
    }));
    let first = handle_webhook(
        State(state.clone()),
        test_connect_info(),
        Query(WebhookQuery::default()),
        headers.clone(),
        body,
    )
    .await
    .into_response();
    assert_eq!(first.status(), StatusCode::OK);

    let body = Ok(Json(WebhookBody {
        message: "hello".into(),
    }));
    let second = handle_webhook(
        State(state),
        test_connect_info(),
        Query(WebhookQuery::default()),
        headers,
        body,
    )
    .await
    .into_response();
    assert_eq!(second.status(), StatusCode::OK);

    let payload = second.into_body().collect().await.unwrap().to_bytes();
    let parsed: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(parsed["status"], "duplicate");
    assert_eq!(parsed["idempotent"], true);
    assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn webhook_unknown_agent_rejected_before_dispatch() {
    let provider_impl = Arc::new(MockModelProvider::default());
    let model_provider: Arc<dyn ModelProvider> = provider_impl.clone();
    let memory: Arc<dyn Memory> = Arc::new(MockMemory);

    let state = AppState {
        config: Arc::new(RwLock::new(Config::default())),
        config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
        model_provider,
        model: "test-model".into(),
        temperature: None,
        mem: memory.clone(),
        memory_strategy: Arc::new(DefaultMemoryStrategy::with_config(
            Arc::clone(&memory),
            zeroclaw_config::schema::MemoryConfig::default(),
            std::path::PathBuf::new(),
        )),
        companion_store: None,
        auto_save: false,
        webhook_secret_hash: None,
        pairing: Arc::new(PairingGuard::new(false, &[])),
        trust_forwarded_headers: false,
        rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100)),
        auth_limiter: Arc::new(auth_rate_limit::AuthRateLimiter::new()),
        idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp: HashMap::new(),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp_app_secret: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq_signing_secrets: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk_webhook_secret: HashMap::new(),
        #[cfg(feature = "channel-wati")]
        wati: HashMap::new(),
        #[cfg(feature = "channel-email")]
        gmail_push: None,
        observer: Arc::new(zeroclaw_runtime::observability::NoopObserver),
        tools_registry: Arc::new(Vec::new()),
        tools_registry_by_agent: Arc::new(std::collections::HashMap::new()),
        cost_tracker: None,
        event_tx: tokio::sync::broadcast::channel(16).0,
        event_buffer: Arc::new(sse::EventBuffer::new(16)),
        shutdown_tx: tokio::sync::watch::channel(false).0,
        reload_tx: None,
        #[cfg(feature = "nodes")]
        node_registry: Arc::new(nodes::NodeRegistry::new(16)),
        #[cfg(feature = "nodes")]
        mdns_peer_registry: nodes::mdns::MdnsPeerRegistry::default(),
        path_prefix: String::new(),
        web_dist_dir: None,
        session_backend: None,
        session_queue: std::sync::Arc::new(crate::session_queue::SessionActorQueue::new(
            8, 30, 600,
        )),
        device_registry: None,
        pending_pairings: None,
        canvas_store: CanvasStore::new(),
        cancel_tokens: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        pending_reload: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        tui_registry: None,
        #[cfg(feature = "webauthn")]
        webauthn: None,
    };

    // An idempotency key on a rejected request must NOT be consumed.
    let mut headers = HeaderMap::new();
    headers.insert("X-Idempotency-Key", HeaderValue::from_static("ghost-key"));

    let response = handle_webhook(
        State(state.clone()),
        test_connect_info(),
        Query(WebhookQuery {
            agent: Some("ghost".into()),
        }),
        headers,
        Ok(Json(WebhookBody {
            message: "hello".into(),
        })),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let payload = response.into_body().collect().await.unwrap().to_bytes();
    let parsed: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert!(
        parsed["error"]
            .as_str()
            .unwrap_or_default()
            .contains("Unknown agent `ghost`")
    );
    assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 0);
    // Key still fresh — a corrected retry with the same key proceeds.
    assert!(state.idempotency_store.record_if_new("ghost-key"));
}

#[tokio::test]
async fn webhook_explicit_agent_reports_model_without_owning_lifecycle() {
    let provider_impl = Arc::new(MockModelProvider::default());
    let model_provider: Arc<dyn ModelProvider> = provider_impl.clone();
    let memory: Arc<dyn Memory> = Arc::new(MockMemory);
    let observer_impl = Arc::new(CapturingObserver::default());
    let observer: Arc<dyn zeroclaw_runtime::observability::Observer> = observer_impl.clone();

    let mut config = Config::default();
    config.providers.models.anthropic.insert(
        "default".into(),
        zeroclaw_config::schema::AnthropicModelProviderConfig {
            base: zeroclaw_config::schema::ModelProviderConfig {
                model: Some("agent-model".into()),
                ..Default::default()
            },
        },
    );
    let expected_provider = "anthropic.default".to_string();
    config.agents.insert(
        "nova".to_string(),
        zeroclaw_config::schema::AliasedAgentConfig {
            enabled: true,
            model_provider: expected_provider.clone().into(),
            ..Default::default()
        },
    );

    let state = AppState {
        config: Arc::new(RwLock::new(config)),
        config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
        model_provider,
        model: "startup-model".into(),
        temperature: None,
        mem: memory,
        memory_strategy: Arc::new(DefaultMemoryStrategy::with_config(
            Arc::new(MockMemory),
            zeroclaw_config::schema::MemoryConfig::default(),
            std::path::PathBuf::new(),
        )),
        companion_store: None,
        auto_save: false,
        webhook_secret_hash: None,
        pairing: Arc::new(PairingGuard::new(false, &[])),
        trust_forwarded_headers: false,
        rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100)),
        auth_limiter: Arc::new(auth_rate_limit::AuthRateLimiter::new()),
        idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp: HashMap::new(),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp_app_secret: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq_signing_secrets: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk_webhook_secret: HashMap::new(),
        #[cfg(feature = "channel-wati")]
        wati: HashMap::new(),
        #[cfg(feature = "channel-email")]
        gmail_push: None,
        observer,
        tools_registry: Arc::new(Vec::new()),
        tools_registry_by_agent: Arc::new(std::collections::HashMap::new()),
        cost_tracker: None,
        event_tx: tokio::sync::broadcast::channel(16).0,
        event_buffer: Arc::new(sse::EventBuffer::new(16)),
        shutdown_tx: tokio::sync::watch::channel(false).0,
        reload_tx: None,
        #[cfg(feature = "nodes")]
        node_registry: Arc::new(nodes::NodeRegistry::new(16)),
        #[cfg(feature = "nodes")]
        mdns_peer_registry: nodes::mdns::MdnsPeerRegistry::default(),
        path_prefix: String::new(),
        web_dist_dir: None,
        session_backend: None,
        session_queue: std::sync::Arc::new(crate::session_queue::SessionActorQueue::new(
            8, 30, 600,
        )),
        device_registry: None,
        pending_pairings: None,
        canvas_store: CanvasStore::new(),
        cancel_tokens: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        pending_reload: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        tui_registry: None,
        #[cfg(feature = "webauthn")]
        webauthn: None,
    };

    let response = handle_webhook(
        State(state),
        test_connect_info(),
        Query(WebhookQuery {
            agent: Some("nova".into()),
        }),
        HeaderMap::new(),
        Ok(Json(WebhookBody {
            message: "hello".into(),
        })),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 1);
    let payload = response.into_body().collect().await.unwrap().to_bytes();
    let parsed: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(parsed["model"], "agent-model");
    let events = observer_impl.events.lock();
    assert!(
        !events.iter().any(|event| matches!(
            event,
            zeroclaw_runtime::observability::ObserverEvent::AgentStart { .. }
                | zeroclaw_runtime::observability::ObserverEvent::AgentEnd { .. }
                | zeroclaw_runtime::observability::ObserverEvent::LlmRequest { .. }
                | zeroclaw_runtime::observability::ObserverEvent::LlmResponse { .. }
        )),
        "the HTTP handler must not create a second agent lifecycle; events were: {events:?}"
    );
}

#[tokio::test]
async fn webhook_autosave_stores_distinct_keys_per_request() {
    let provider_impl = Arc::new(MockModelProvider::default());
    let model_provider: Arc<dyn ModelProvider> = provider_impl.clone();

    let tracking_impl = Arc::new(TrackingMemory::default());
    let memory: Arc<dyn Memory> = tracking_impl.clone();

    let state = AppState {
        config: Arc::new(RwLock::new(Config::default())),
        config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
        model_provider,
        model: "test-model".into(),
        temperature: None,
        mem: memory,
        memory_strategy: Arc::new(DefaultMemoryStrategy::with_config(
            Arc::new(MockMemory),
            zeroclaw_config::schema::MemoryConfig::default(),
            std::path::PathBuf::new(),
        )),
        companion_store: None,
        auto_save: true,
        webhook_secret_hash: None,
        pairing: Arc::new(PairingGuard::new(false, &[])),
        trust_forwarded_headers: false,
        rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100)),
        auth_limiter: Arc::new(auth_rate_limit::AuthRateLimiter::new()),
        idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp: HashMap::new(),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp_app_secret: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq_signing_secrets: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk_webhook_secret: HashMap::new(),
        #[cfg(feature = "channel-wati")]
        wati: HashMap::new(),
        #[cfg(feature = "channel-email")]
        gmail_push: None,
        observer: Arc::new(zeroclaw_runtime::observability::NoopObserver),
        tools_registry: Arc::new(Vec::new()),
        tools_registry_by_agent: Arc::new(std::collections::HashMap::new()),
        cost_tracker: None,
        event_tx: tokio::sync::broadcast::channel(16).0,
        event_buffer: Arc::new(sse::EventBuffer::new(16)),
        shutdown_tx: tokio::sync::watch::channel(false).0,
        reload_tx: None,
        #[cfg(feature = "nodes")]
        node_registry: Arc::new(nodes::NodeRegistry::new(16)),
        #[cfg(feature = "nodes")]
        mdns_peer_registry: nodes::mdns::MdnsPeerRegistry::default(),
        path_prefix: String::new(),
        web_dist_dir: None,
        session_backend: None,
        session_queue: std::sync::Arc::new(crate::session_queue::SessionActorQueue::new(
            8, 30, 600,
        )),
        device_registry: None,
        pending_pairings: None,
        canvas_store: CanvasStore::new(),
        cancel_tokens: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        pending_reload: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        tui_registry: None,
        #[cfg(feature = "webauthn")]
        webauthn: None,
    };

    let headers = HeaderMap::new();

    let body1 = Ok(Json(WebhookBody {
        message: "hello one".into(),
    }));
    let first = handle_webhook(
        State(state.clone()),
        test_connect_info(),
        Query(WebhookQuery::default()),
        headers.clone(),
        body1,
    )
    .await
    .into_response();
    assert_eq!(first.status(), StatusCode::OK);

    let body2 = Ok(Json(WebhookBody {
        message: "hello two".into(),
    }));
    let second = handle_webhook(
        State(state),
        test_connect_info(),
        Query(WebhookQuery::default()),
        headers,
        body2,
    )
    .await
    .into_response();
    assert_eq!(second.status(), StatusCode::OK);

    let keys = tracking_impl.keys.lock().clone();
    assert_eq!(keys.len(), 2);
    assert_ne!(keys[0], keys[1]);
    assert!(keys[0].starts_with("webhook_msg_"));
    assert!(keys[1].starts_with("webhook_msg_"));
    assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn webhook_secret_hash_is_deterministic_and_nonempty() {
    let secret_a = generate_test_secret();
    let secret_b = generate_test_secret();
    let one = hash_webhook_secret(&secret_a);
    let two = hash_webhook_secret(&secret_a);
    let other = hash_webhook_secret(&secret_b);

    assert_eq!(one, two);
    assert_ne!(one, other);
    assert_eq!(one.len(), 64);
}

#[tokio::test]
async fn webhook_secret_hash_rejects_missing_header() {
    let provider_impl = Arc::new(MockModelProvider::default());
    let model_provider: Arc<dyn ModelProvider> = provider_impl.clone();
    let memory: Arc<dyn Memory> = Arc::new(MockMemory);
    let secret = generate_test_secret();

    let state = AppState {
        config: Arc::new(RwLock::new(Config::default())),
        config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
        model_provider,
        model: "test-model".into(),
        temperature: None,
        mem: memory.clone(),
        memory_strategy: Arc::new(DefaultMemoryStrategy::with_config(
            Arc::clone(&memory),
            zeroclaw_config::schema::MemoryConfig::default(),
            std::path::PathBuf::new(),
        )),
        companion_store: None,
        auto_save: false,
        webhook_secret_hash: Some(Arc::from(hash_webhook_secret(&secret))),
        pairing: Arc::new(PairingGuard::new(false, &[])),
        trust_forwarded_headers: false,
        rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100)),
        auth_limiter: Arc::new(auth_rate_limit::AuthRateLimiter::new()),
        idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp: HashMap::new(),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp_app_secret: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq_signing_secrets: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk_webhook_secret: HashMap::new(),
        #[cfg(feature = "channel-wati")]
        wati: HashMap::new(),
        #[cfg(feature = "channel-email")]
        gmail_push: None,
        observer: Arc::new(zeroclaw_runtime::observability::NoopObserver),
        tools_registry: Arc::new(Vec::new()),
        tools_registry_by_agent: Arc::new(std::collections::HashMap::new()),
        cost_tracker: None,
        event_tx: tokio::sync::broadcast::channel(16).0,
        event_buffer: Arc::new(sse::EventBuffer::new(16)),
        shutdown_tx: tokio::sync::watch::channel(false).0,
        reload_tx: None,
        #[cfg(feature = "nodes")]
        node_registry: Arc::new(nodes::NodeRegistry::new(16)),
        #[cfg(feature = "nodes")]
        mdns_peer_registry: nodes::mdns::MdnsPeerRegistry::default(),
        path_prefix: String::new(),
        web_dist_dir: None,
        session_backend: None,
        session_queue: std::sync::Arc::new(crate::session_queue::SessionActorQueue::new(
            8, 30, 600,
        )),
        device_registry: None,
        pending_pairings: None,
        canvas_store: CanvasStore::new(),
        cancel_tokens: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        pending_reload: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        tui_registry: None,
        #[cfg(feature = "webauthn")]
        webauthn: None,
    };

    let response = handle_webhook(
        State(state),
        test_connect_info(),
        Query(WebhookQuery::default()),
        HeaderMap::new(),
        Ok(Json(WebhookBody {
            message: "hello".into(),
        })),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn webhook_secret_hash_rejects_invalid_header() {
    let provider_impl = Arc::new(MockModelProvider::default());
    let model_provider: Arc<dyn ModelProvider> = provider_impl.clone();
    let memory: Arc<dyn Memory> = Arc::new(MockMemory);
    let valid_secret = generate_test_secret();
    let wrong_secret = generate_test_secret();

    let state = AppState {
        config: Arc::new(RwLock::new(Config::default())),
        config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
        model_provider,
        model: "test-model".into(),
        temperature: None,
        mem: memory.clone(),
        memory_strategy: Arc::new(DefaultMemoryStrategy::with_config(
            Arc::clone(&memory),
            zeroclaw_config::schema::MemoryConfig::default(),
            std::path::PathBuf::new(),
        )),
        companion_store: None,
        auto_save: false,
        webhook_secret_hash: Some(Arc::from(hash_webhook_secret(&valid_secret))),
        pairing: Arc::new(PairingGuard::new(false, &[])),
        trust_forwarded_headers: false,
        rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100)),
        auth_limiter: Arc::new(auth_rate_limit::AuthRateLimiter::new()),
        idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp: HashMap::new(),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp_app_secret: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq_signing_secrets: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk_webhook_secret: HashMap::new(),
        #[cfg(feature = "channel-wati")]
        wati: HashMap::new(),
        #[cfg(feature = "channel-email")]
        gmail_push: None,
        observer: Arc::new(zeroclaw_runtime::observability::NoopObserver),
        tools_registry: Arc::new(Vec::new()),
        tools_registry_by_agent: Arc::new(std::collections::HashMap::new()),
        cost_tracker: None,
        event_tx: tokio::sync::broadcast::channel(16).0,
        event_buffer: Arc::new(sse::EventBuffer::new(16)),
        shutdown_tx: tokio::sync::watch::channel(false).0,
        reload_tx: None,
        #[cfg(feature = "nodes")]
        node_registry: Arc::new(nodes::NodeRegistry::new(16)),
        #[cfg(feature = "nodes")]
        mdns_peer_registry: nodes::mdns::MdnsPeerRegistry::default(),
        path_prefix: String::new(),
        web_dist_dir: None,
        session_backend: None,
        session_queue: std::sync::Arc::new(crate::session_queue::SessionActorQueue::new(
            8, 30, 600,
        )),
        device_registry: None,
        pending_pairings: None,
        canvas_store: CanvasStore::new(),
        cancel_tokens: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        pending_reload: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        tui_registry: None,
        #[cfg(feature = "webauthn")]
        webauthn: None,
    };

    let mut headers = HeaderMap::new();
    headers.insert(
        "X-Webhook-Secret",
        HeaderValue::from_str(&wrong_secret).unwrap(),
    );

    let response = handle_webhook(
        State(state),
        test_connect_info(),
        Query(WebhookQuery::default()),
        headers,
        Ok(Json(WebhookBody {
            message: "hello".into(),
        })),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn webhook_secret_hash_accepts_valid_header() {
    let provider_impl = Arc::new(MockModelProvider::default());
    let model_provider: Arc<dyn ModelProvider> = provider_impl.clone();
    let memory: Arc<dyn Memory> = Arc::new(MockMemory);
    let secret = generate_test_secret();

    let state = AppState {
        config: Arc::new(RwLock::new(Config::default())),
        config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
        model_provider,
        model: "test-model".into(),
        temperature: None,
        mem: memory.clone(),
        memory_strategy: Arc::new(DefaultMemoryStrategy::with_config(
            Arc::clone(&memory),
            zeroclaw_config::schema::MemoryConfig::default(),
            std::path::PathBuf::new(),
        )),
        companion_store: None,
        auto_save: false,
        webhook_secret_hash: Some(Arc::from(hash_webhook_secret(&secret))),
        pairing: Arc::new(PairingGuard::new(false, &[])),
        trust_forwarded_headers: false,
        rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100)),
        auth_limiter: Arc::new(auth_rate_limit::AuthRateLimiter::new()),
        idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp: HashMap::new(),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp_app_secret: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq_signing_secrets: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk_webhook_secret: HashMap::new(),
        #[cfg(feature = "channel-wati")]
        wati: HashMap::new(),
        #[cfg(feature = "channel-email")]
        gmail_push: None,
        observer: Arc::new(zeroclaw_runtime::observability::NoopObserver),
        tools_registry: Arc::new(Vec::new()),
        tools_registry_by_agent: Arc::new(std::collections::HashMap::new()),
        cost_tracker: None,
        event_tx: tokio::sync::broadcast::channel(16).0,
        event_buffer: Arc::new(sse::EventBuffer::new(16)),
        shutdown_tx: tokio::sync::watch::channel(false).0,
        reload_tx: None,
        #[cfg(feature = "nodes")]
        node_registry: Arc::new(nodes::NodeRegistry::new(16)),
        #[cfg(feature = "nodes")]
        mdns_peer_registry: nodes::mdns::MdnsPeerRegistry::default(),
        path_prefix: String::new(),
        web_dist_dir: None,
        session_backend: None,
        session_queue: std::sync::Arc::new(crate::session_queue::SessionActorQueue::new(
            8, 30, 600,
        )),
        device_registry: None,
        pending_pairings: None,
        canvas_store: CanvasStore::new(),
        cancel_tokens: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        pending_reload: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        tui_registry: None,
        #[cfg(feature = "webauthn")]
        webauthn: None,
    };

    let mut headers = HeaderMap::new();
    headers.insert("X-Webhook-Secret", HeaderValue::from_str(&secret).unwrap());

    let response = handle_webhook(
        State(state),
        test_connect_info(),
        Query(WebhookQuery::default()),
        headers,
        Ok(Json(WebhookBody {
            message: "hello".into(),
        })),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 1);
}

#[cfg(feature = "channel-nextcloud")]
fn compute_nextcloud_signature_hex(secret: &str, random: &str, body: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let payload = format!("{random}{body}");
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

#[cfg(feature = "channel-nextcloud")]
#[tokio::test]
async fn nextcloud_talk_webhook_returns_not_found_when_not_configured() {
    let model_provider: Arc<dyn ModelProvider> = Arc::new(MockModelProvider::default());
    let memory: Arc<dyn Memory> = Arc::new(MockMemory);

    let state = AppState {
        config: Arc::new(RwLock::new(Config::default())),
        config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
        model_provider,
        model: "test-model".into(),
        temperature: None,
        mem: memory.clone(),
        memory_strategy: Arc::new(DefaultMemoryStrategy::with_config(
            Arc::clone(&memory),
            zeroclaw_config::schema::MemoryConfig::default(),
            std::path::PathBuf::new(),
        )),
        companion_store: None,
        auto_save: false,
        webhook_secret_hash: None,
        pairing: Arc::new(PairingGuard::new(false, &[])),
        trust_forwarded_headers: false,
        rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100)),
        auth_limiter: Arc::new(auth_rate_limit::AuthRateLimiter::new()),
        idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp: HashMap::new(),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp_app_secret: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq_signing_secrets: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk_webhook_secret: HashMap::new(),
        #[cfg(feature = "channel-wati")]
        wati: HashMap::new(),
        #[cfg(feature = "channel-email")]
        gmail_push: None,
        observer: Arc::new(zeroclaw_runtime::observability::NoopObserver),
        tools_registry: Arc::new(Vec::new()),
        tools_registry_by_agent: Arc::new(std::collections::HashMap::new()),
        cost_tracker: None,
        event_tx: tokio::sync::broadcast::channel(16).0,
        event_buffer: Arc::new(sse::EventBuffer::new(16)),
        shutdown_tx: tokio::sync::watch::channel(false).0,
        reload_tx: None,
        #[cfg(feature = "nodes")]
        node_registry: Arc::new(nodes::NodeRegistry::new(16)),
        #[cfg(feature = "nodes")]
        mdns_peer_registry: nodes::mdns::MdnsPeerRegistry::default(),
        path_prefix: String::new(),
        web_dist_dir: None,
        session_backend: None,
        session_queue: std::sync::Arc::new(crate::session_queue::SessionActorQueue::new(
            8, 30, 600,
        )),
        device_registry: None,
        pending_pairings: None,
        canvas_store: CanvasStore::new(),
        cancel_tokens: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        pending_reload: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        tui_registry: None,
        #[cfg(feature = "webauthn")]
        webauthn: None,
    };

    let response = Box::pin(handle_nextcloud_talk_webhook(
        State(state),
        HeaderMap::new(),
        Bytes::from_static(br#"{"type":"message"}"#),
    ))
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[cfg(feature = "channel-nextcloud")]
#[tokio::test]
async fn nextcloud_talk_webhook_rejects_invalid_signature() {
    let provider_impl = Arc::new(MockModelProvider::default());
    let model_provider: Arc<dyn ModelProvider> = provider_impl.clone();
    let memory: Arc<dyn Memory> = Arc::new(MockMemory);

    let alias = "nextcloud_talk_test_alias";
    let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = Arc::new(Vec::new);
    let channel = Arc::new(NextcloudTalkChannel::new(
        "https://cloud.example.com".into(),
        "app-token".into(),
        String::new(),
        alias,
        peer_resolver,
    ));

    let secret = "nextcloud-test-secret";
    let random = "seed-value";
    let body = r#"{"type":"message","object":{"token":"room-token"},"message":{"actorType":"users","actorId":"user_a","message":"hello"}}"#;
    let _valid_signature = compute_nextcloud_signature_hex(secret, random, body);
    let invalid_signature = "deadbeef";

    let state = AppState {
        config: Arc::new(RwLock::new(Config::default())),
        config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
        model_provider,
        model: "test-model".into(),
        temperature: None,
        mem: memory.clone(),
        memory_strategy: Arc::new(DefaultMemoryStrategy::with_config(
            Arc::clone(&memory),
            zeroclaw_config::schema::MemoryConfig::default(),
            std::path::PathBuf::new(),
        )),
        companion_store: None,
        auto_save: false,
        webhook_secret_hash: None,
        pairing: Arc::new(PairingGuard::new(false, &[])),
        trust_forwarded_headers: false,
        rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100)),
        auth_limiter: Arc::new(auth_rate_limit::AuthRateLimiter::new()),
        idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp: HashMap::new(),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp_app_secret: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq_signing_secrets: HashMap::new(),
        nextcloud_talk: HashMap::from([(alias.to_string(), channel)]),
        nextcloud_talk_webhook_secret: HashMap::from([(alias.to_string(), Arc::from(secret))]),
        #[cfg(feature = "channel-wati")]
        wati: HashMap::new(),
        #[cfg(feature = "channel-email")]
        gmail_push: None,
        observer: Arc::new(zeroclaw_runtime::observability::NoopObserver),
        tools_registry: Arc::new(Vec::new()),
        tools_registry_by_agent: Arc::new(std::collections::HashMap::new()),
        cost_tracker: None,
        event_tx: tokio::sync::broadcast::channel(16).0,
        event_buffer: Arc::new(sse::EventBuffer::new(16)),
        shutdown_tx: tokio::sync::watch::channel(false).0,
        reload_tx: None,
        #[cfg(feature = "nodes")]
        node_registry: Arc::new(nodes::NodeRegistry::new(16)),
        #[cfg(feature = "nodes")]
        mdns_peer_registry: nodes::mdns::MdnsPeerRegistry::default(),
        path_prefix: String::new(),
        web_dist_dir: None,
        session_backend: None,
        session_queue: std::sync::Arc::new(crate::session_queue::SessionActorQueue::new(
            8, 30, 600,
        )),
        device_registry: None,
        pending_pairings: None,
        canvas_store: CanvasStore::new(),
        cancel_tokens: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        pending_reload: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        tui_registry: None,
        #[cfg(feature = "webauthn")]
        webauthn: None,
    };

    let mut headers = HeaderMap::new();
    headers.insert(
        "X-Nextcloud-Talk-Random",
        HeaderValue::from_str(random).unwrap(),
    );
    headers.insert(
        "X-Nextcloud-Talk-Signature",
        HeaderValue::from_str(invalid_signature).unwrap(),
    );

    let response = Box::pin(handle_nextcloud_talk_webhook(
        State(state),
        headers,
        Bytes::from(body),
    ))
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 0);
}

// handler must return 200 OK before the (potentially
// slow) LLM call completes, so Nextcloud Talk doesn't cancel the webhook
// request at its ~5s timeout.
#[cfg(feature = "channel-nextcloud")]
#[derive(Default)]
struct SlowProvider {
    calls: AtomicUsize,
    started_tx: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

#[cfg(feature = "channel-nextcloud")]
#[async_trait]
impl ModelProvider for SlowProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: Option<f64>,
    ) -> anyhow::Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(tx) = self.started_tx.lock().take() {
            let _ = tx.send(());
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
        Ok("slow ok".into())
    }
}
#[cfg(feature = "channel-nextcloud")]
impl ::zeroclaw_api::attribution::Attributable for SlowProvider {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        ::zeroclaw_api::attribution::Role::Provider(
            ::zeroclaw_api::attribution::ProviderKind::Model(
                ::zeroclaw_api::attribution::ModelProviderKind::Custom,
            ),
        )
    }
    fn alias(&self) -> &str {
        "SlowProvider"
    }
}

#[cfg(feature = "channel-nextcloud")]
#[tokio::test]
async fn nextcloud_talk_webhook_returns_before_llm_call_completes() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let provider_impl = Arc::new(SlowProvider {
        calls: AtomicUsize::new(0),
        started_tx: Mutex::new(Some(started_tx)),
    });
    let provider: Arc<dyn ModelProvider> = provider_impl.clone();
    let memory: Arc<dyn Memory> = Arc::new(MockMemory);

    let channel = Arc::new(NextcloudTalkChannel::new(
        "https://cloud.example.com".into(),
        "app-token".into(),
        String::new(),
        "default",
        Arc::new(|| vec!["*".to_string()]),
    ));

    let body = r#"{"type":"message","object":{"token":"room-token"},"actor":{"id":"user_a","name":"User A"},"message":{"actorType":"users","actorId":"user_a","message":"hello"}}"#;

    let state = AppState {
        config: Arc::new(RwLock::new(Config::default())),
        config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
        model_provider: provider,
        model: "test-model".into(),
        temperature: None,
        mem: memory.clone(),
        memory_strategy: Arc::new(DefaultMemoryStrategy::with_config(
            Arc::clone(&memory),
            zeroclaw_config::schema::MemoryConfig::default(),
            std::path::PathBuf::new(),
        )),
        companion_store: None,
        auto_save: false,
        webhook_secret_hash: None,
        pairing: Arc::new(PairingGuard::new(false, &[])),
        trust_forwarded_headers: false,
        rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100)),
        auth_limiter: Arc::new(auth_rate_limit::AuthRateLimiter::new()),
        idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp: HashMap::new(),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp_app_secret: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq_signing_secrets: HashMap::new(),
        nextcloud_talk: HashMap::from([("default".to_string(), channel)]),
        nextcloud_talk_webhook_secret: HashMap::new(),
        pending_reload: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        tui_registry: None,
        #[cfg(feature = "channel-wati")]
        wati: HashMap::new(),
        #[cfg(feature = "channel-email")]
        gmail_push: None,
        observer: Arc::new(zeroclaw_runtime::observability::NoopObserver),
        tools_registry: Arc::new(Vec::new()),
        tools_registry_by_agent: Arc::new(std::collections::HashMap::new()),
        cost_tracker: None,
        event_tx: tokio::sync::broadcast::channel(16).0,
        event_buffer: Arc::new(sse::EventBuffer::new(16)),
        shutdown_tx: tokio::sync::watch::channel(false).0,
        reload_tx: None,
        #[cfg(feature = "nodes")]
        node_registry: Arc::new(nodes::NodeRegistry::new(16)),
        #[cfg(feature = "nodes")]
        mdns_peer_registry: nodes::mdns::MdnsPeerRegistry::default(),
        path_prefix: String::new(),
        web_dist_dir: None,
        session_backend: None,
        session_queue: std::sync::Arc::new(crate::session_queue::SessionActorQueue::new(
            8, 30, 600,
        )),
        device_registry: None,
        pending_pairings: None,
        canvas_store: CanvasStore::new(),
        cancel_tokens: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        #[cfg(feature = "webauthn")]
        webauthn: None,
    };

    let start = std::time::Instant::now();
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        Box::pin(handle_nextcloud_talk_webhook(
            State(state),
            HeaderMap::new(),
            Bytes::from(body),
        )),
    )
    .await
    .expect("webhook must return before 2s deadline (regression #6156)")
    .into_response();

    let elapsed = start.elapsed();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        elapsed < Duration::from_secs(2),
        "handler returned after {elapsed:?}; expected fast return for #6156"
    );

    // Confirm the spawned task actually started the LLM call (i.e., the
    // ack didn't just skip processing). The 30s sleep is still in flight.
    tokio::time::timeout(Duration::from_secs(2), started_rx)
        .await
        .expect("spawned LLM call did not start within 2s")
        .expect("started_tx sender was dropped");
    assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 1);
}

// ══════════════════════════════════════════════════════════
// WhatsApp Signature Verification Tests (CWE-345 Prevention)
// ══════════════════════════════════════════════════════════

#[cfg(feature = "channel-whatsapp-cloud")]
fn compute_whatsapp_signature_hex(secret: &str, body: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

#[cfg(feature = "channel-whatsapp-cloud")]
fn compute_whatsapp_signature_header(secret: &str, body: &[u8]) -> String {
    format!("sha256={}", compute_whatsapp_signature_hex(secret, body))
}

#[cfg(feature = "channel-whatsapp-cloud")]
#[test]
fn whatsapp_signature_valid() {
    let app_secret = generate_test_secret();
    let body = b"test body content";

    let signature_header = compute_whatsapp_signature_header(&app_secret, body);

    assert!(verify_whatsapp_signature(
        &app_secret,
        body,
        &signature_header
    ));
}

#[cfg(feature = "channel-whatsapp-cloud")]
#[test]
fn whatsapp_signature_invalid_wrong_secret() {
    let app_secret = generate_test_secret();
    let wrong_secret = generate_test_secret();
    let body = b"test body content";

    let signature_header = compute_whatsapp_signature_header(&wrong_secret, body);

    assert!(!verify_whatsapp_signature(
        &app_secret,
        body,
        &signature_header
    ));
}

#[cfg(feature = "channel-whatsapp-cloud")]
#[test]
fn whatsapp_signature_invalid_wrong_body() {
    let app_secret = generate_test_secret();
    let original_body = b"original body";
    let tampered_body = b"tampered body";

    let signature_header = compute_whatsapp_signature_header(&app_secret, original_body);

    // Verify with tampered body should fail
    assert!(!verify_whatsapp_signature(
        &app_secret,
        tampered_body,
        &signature_header
    ));
}

#[cfg(feature = "channel-whatsapp-cloud")]
#[test]
fn whatsapp_signature_missing_prefix() {
    let app_secret = generate_test_secret();
    let body = b"test body";

    // Signature without "sha256=" prefix
    let signature_header = "abc123def456";

    assert!(!verify_whatsapp_signature(
        &app_secret,
        body,
        signature_header
    ));
}

#[cfg(feature = "channel-whatsapp-cloud")]
#[test]
fn whatsapp_signature_empty_header() {
    let app_secret = generate_test_secret();
    let body = b"test body";

    assert!(!verify_whatsapp_signature(&app_secret, body, ""));
}

#[cfg(feature = "channel-whatsapp-cloud")]
#[test]
fn whatsapp_signature_invalid_hex() {
    let app_secret = generate_test_secret();
    let body = b"test body";

    // Invalid hex characters
    let signature_header = "sha256=not_valid_hex_zzz";

    assert!(!verify_whatsapp_signature(
        &app_secret,
        body,
        signature_header
    ));
}

#[cfg(feature = "channel-whatsapp-cloud")]
#[test]
fn whatsapp_signature_empty_body() {
    let app_secret = generate_test_secret();
    let body = b"";

    let signature_header = compute_whatsapp_signature_header(&app_secret, body);

    assert!(verify_whatsapp_signature(
        &app_secret,
        body,
        &signature_header
    ));
}

#[cfg(feature = "channel-whatsapp-cloud")]
#[test]
fn whatsapp_signature_unicode_body() {
    let app_secret = generate_test_secret();
    let body = "Hello 🦀 World".as_bytes();

    let signature_header = compute_whatsapp_signature_header(&app_secret, body);

    assert!(verify_whatsapp_signature(
        &app_secret,
        body,
        &signature_header
    ));
}

#[cfg(feature = "channel-whatsapp-cloud")]
#[test]
fn whatsapp_signature_json_payload() {
    let app_secret = generate_test_secret();
    let body = br#"{"entry":[{"changes":[{"value":{"messages":[{"from":"1234567890","text":{"body":"Hello"}}]}}]}]}"#;

    let signature_header = compute_whatsapp_signature_header(&app_secret, body);

    assert!(verify_whatsapp_signature(
        &app_secret,
        body,
        &signature_header
    ));
}

#[cfg(feature = "channel-whatsapp-cloud")]
#[test]
fn whatsapp_signature_case_sensitive_prefix() {
    let app_secret = generate_test_secret();
    let body = b"test body";

    let hex_sig = compute_whatsapp_signature_hex(&app_secret, body);

    // Wrong case prefix should fail
    let wrong_prefix = format!("SHA256={hex_sig}");
    assert!(!verify_whatsapp_signature(&app_secret, body, &wrong_prefix));

    // Correct prefix should pass
    let correct_prefix = format!("sha256={hex_sig}");
    assert!(verify_whatsapp_signature(
        &app_secret,
        body,
        &correct_prefix
    ));
}

#[cfg(feature = "channel-whatsapp-cloud")]
#[test]
fn whatsapp_signature_truncated_hex() {
    let app_secret = generate_test_secret();
    let body = b"test body";

    let hex_sig = compute_whatsapp_signature_hex(&app_secret, body);
    let truncated = &hex_sig[..32]; // Only half the signature
    let signature_header = format!("sha256={truncated}");

    assert!(!verify_whatsapp_signature(
        &app_secret,
        body,
        &signature_header
    ));
}

#[cfg(feature = "channel-whatsapp-cloud")]
#[test]
fn whatsapp_signature_extra_bytes() {
    let app_secret = generate_test_secret();
    let body = b"test body";

    let hex_sig = compute_whatsapp_signature_hex(&app_secret, body);
    let extended = format!("{hex_sig}deadbeef");
    let signature_header = format!("sha256={extended}");

    assert!(!verify_whatsapp_signature(
        &app_secret,
        body,
        &signature_header
    ));
}

// ══════════════════════════════════════════════════════════
// IdempotencyStore Edge-Case Tests
// ══════════════════════════════════════════════════════════

#[test]
fn idempotency_store_allows_different_keys() {
    let store = IdempotencyStore::new(Duration::from_secs(60), 100);
    assert!(store.record_if_new("key-a"));
    assert!(store.record_if_new("key-b"));
    assert!(store.record_if_new("key-c"));
    assert!(store.record_if_new("key-d"));
}

#[test]
fn idempotency_store_max_keys_clamped_to_one() {
    let store = IdempotencyStore::new(Duration::from_secs(60), 0);
    assert!(store.record_if_new("only-key"));
    assert!(!store.record_if_new("only-key"));
}

#[test]
fn idempotency_store_rapid_duplicate_rejected() {
    let store = IdempotencyStore::new(Duration::from_secs(300), 100);
    assert!(store.record_if_new("rapid"));
    assert!(!store.record_if_new("rapid"));
}

#[test]
fn idempotency_store_accepts_after_ttl_expires() {
    let store = IdempotencyStore::new(Duration::from_millis(1), 100);
    assert!(store.record_if_new("ttl-key"));
    std::thread::sleep(Duration::from_millis(10));
    assert!(store.record_if_new("ttl-key"));
}

#[test]
fn idempotency_store_eviction_preserves_newest() {
    let store = IdempotencyStore::new(Duration::from_secs(300), 1);
    assert!(store.record_if_new("old-key"));
    std::thread::sleep(Duration::from_millis(2));
    assert!(store.record_if_new("new-key"));

    let keys = store.keys.lock();
    assert_eq!(keys.len(), 1);
    assert!(!keys.contains_key("old-key"));
    assert!(keys.contains_key("new-key"));
}

#[test]
fn rate_limiter_allows_after_window_expires() {
    let window = Duration::from_millis(50);
    let limiter = SlidingWindowRateLimiter::new(2, window, 100);
    assert!(limiter.allow("ip-1"));
    assert!(limiter.allow("ip-1"));
    assert!(!limiter.allow("ip-1")); // blocked

    // Wait for window to expire
    std::thread::sleep(Duration::from_millis(60));

    // Should be allowed again
    assert!(limiter.allow("ip-1"));
}

#[test]
fn rate_limiter_independent_keys_tracked_separately() {
    let limiter = SlidingWindowRateLimiter::new(2, Duration::from_secs(60), 100);
    assert!(limiter.allow("ip-1"));
    assert!(limiter.allow("ip-1"));
    assert!(!limiter.allow("ip-1")); // ip-1 blocked

    // ip-2 should still work
    assert!(limiter.allow("ip-2"));
    assert!(limiter.allow("ip-2"));
    assert!(!limiter.allow("ip-2")); // ip-2 now blocked
}

#[test]
fn rate_limiter_exact_boundary_at_max_keys() {
    let limiter = SlidingWindowRateLimiter::new(10, Duration::from_secs(60), 3);
    assert!(limiter.allow("ip-1"));
    assert!(limiter.allow("ip-2"));
    assert!(limiter.allow("ip-3"));
    // At capacity now
    assert!(limiter.allow("ip-4")); // should evict ip-1

    let guard = limiter.requests.lock();
    assert_eq!(guard.0.len(), 3);
    assert!(
        !guard.0.contains_key("ip-1"),
        "ip-1 should have been evicted"
    );
    assert!(guard.0.contains_key("ip-2"));
    assert!(guard.0.contains_key("ip-3"));
    assert!(guard.0.contains_key("ip-4"));
}

#[test]
fn gateway_rate_limiter_pair_and_webhook_are_independent() {
    let limiter = GatewayRateLimiter::new(2, 3, 100);

    // Exhaust pair limit
    assert!(limiter.allow_pair("ip-1"));
    assert!(limiter.allow_pair("ip-1"));
    assert!(!limiter.allow_pair("ip-1")); // pair blocked

    // Webhook should still work
    assert!(limiter.allow_webhook("ip-1"));
    assert!(limiter.allow_webhook("ip-1"));
    assert!(limiter.allow_webhook("ip-1"));
    assert!(!limiter.allow_webhook("ip-1")); // webhook now blocked
}

#[test]
fn rate_limiter_single_key_max_allows_one_request() {
    let limiter = SlidingWindowRateLimiter::new(5, Duration::from_secs(60), 1);
    assert!(limiter.allow("ip-1"));
    assert!(limiter.allow("ip-2")); // evicts ip-1

    let guard = limiter.requests.lock();
    assert_eq!(guard.0.len(), 1);
    assert!(guard.0.contains_key("ip-2"));
    assert!(!guard.0.contains_key("ip-1"));
}

#[test]
fn rate_limiter_concurrent_access_safe() {
    use std::sync::Arc;

    let limiter = Arc::new(SlidingWindowRateLimiter::new(
        1000,
        Duration::from_secs(60),
        1000,
    ));
    let mut handles = Vec::new();

    for i in 0..10 {
        let limiter = limiter.clone();
        handles.push(std::thread::spawn(move || {
            for j in 0..100 {
                limiter.allow(&format!("thread-{i}-req-{j}"));
            }
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    // Should not panic or deadlock
    let guard = limiter.requests.lock();
    assert!(guard.0.len() <= 1000, "should respect max_keys");
}

#[test]
fn idempotency_store_concurrent_access_safe() {
    use std::sync::Arc;

    let store = Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000));
    let mut handles = Vec::new();

    for i in 0..10 {
        let store = store.clone();
        handles.push(std::thread::spawn(move || {
            for j in 0..100 {
                store.record_if_new(&format!("thread-{i}-key-{j}"));
            }
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    let keys = store.keys.lock();
    assert!(keys.len() <= 1000, "should respect max_keys");
}

#[test]
fn rate_limiter_rapid_burst_then_cooldown() {
    let limiter = SlidingWindowRateLimiter::new(5, Duration::from_millis(50), 100);

    // Burst: use all 5 requests
    for _ in 0..5 {
        assert!(limiter.allow("burst-ip"));
    }
    assert!(!limiter.allow("burst-ip")); // 6th should fail

    // Cooldown
    std::thread::sleep(Duration::from_millis(60));

    // Should be allowed again
    assert!(limiter.allow("burst-ip"));
}

#[test]
fn require_localhost_accepts_ipv4_loopback() {
    let peer = SocketAddr::from(([127, 0, 0, 1], 12345));
    assert!(require_localhost(&peer).is_ok());
}

#[test]
fn require_localhost_accepts_ipv6_loopback() {
    let peer = SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, 12345));
    assert!(require_localhost(&peer).is_ok());
}

#[test]
fn require_localhost_rejects_non_loopback_ipv4() {
    let peer = SocketAddr::from(([192, 168, 1, 100], 12345));
    let err = require_localhost(&peer).unwrap_err();
    assert_eq!(err.0, StatusCode::FORBIDDEN);
}

#[test]
fn require_localhost_rejects_non_loopback_ipv6() {
    let peer = SocketAddr::from((
        std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1),
        12345,
    ));
    let err = require_localhost(&peer).unwrap_err();
    assert_eq!(err.0, StatusCode::FORBIDDEN);
}

#[test]
fn admin_reload_gate_loopback_always_allowed() {
    // Loopback is allowed regardless of the opt-in or pairing flags.
    assert_eq!(
        admin_reload_gate(true, false, false),
        AdminReloadGate::Allow
    );
    assert_eq!(admin_reload_gate(true, true, true), AdminReloadGate::Allow);
    assert_eq!(admin_reload_gate(true, false, true), AdminReloadGate::Allow);
    assert_eq!(admin_reload_gate(true, true, false), AdminReloadGate::Allow);
}

#[test]
fn admin_reload_gate_remote_blocked_by_default() {
    // Non-loopback caller with the flag off is rejected outright,
    // regardless of pairing.
    assert_eq!(
        admin_reload_gate(false, false, true),
        AdminReloadGate::Forbidden
    );
    assert_eq!(
        admin_reload_gate(false, false, false),
        AdminReloadGate::Forbidden
    );
}

#[test]
fn admin_reload_gate_remote_opt_in_requires_auth() {
    // Non-loopback caller with the flag on and pairing on must authenticate.
    assert_eq!(
        admin_reload_gate(false, true, true),
        AdminReloadGate::RequireAuth
    );
}

#[test]
fn admin_reload_gate_remote_opt_in_without_pairing_is_rejected() {
    // Opting in with pairing off cannot authenticate the caller, so the
    // request is rejected rather than allowed anonymously.
    assert_eq!(
        admin_reload_gate(false, true, false),
        AdminReloadGate::ForbiddenNoPairing
    );
}

#[test]
fn allow_remote_admin_defaults_off() {
    // Security default: remote admin reload is disabled until opted in.
    assert!(!zeroclaw_config::schema::GatewayConfig::default().allow_remote_admin);
}

/// Build an `AppState` for `handle_admin_reload`: controls
/// `gateway.allow_remote_admin`, pairing (and its tokens), and wires a
/// live reload channel so the allowed path reaches `200` rather than the
/// `503` standalone-gateway branch.
fn admin_reload_state(
    tmp: &tempfile::TempDir,
    allow_remote_admin: bool,
    require_pairing: bool,
    tokens: &[String],
) -> AppState {
    let mut state = admin_paircode_state(tmp, require_pairing, false);
    state.config.write().gateway.allow_remote_admin = allow_remote_admin;
    state.pairing = Arc::new(PairingGuard::new(require_pairing, tokens));
    state.reload_tx = Some(tokio::sync::watch::channel(false).0);
    state
}

fn loopback_peer() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 40000))
}

fn remote_peer() -> SocketAddr {
    // RFC 5737 TEST-NET-3 documentation address — a stable non-loopback
    // peer that is never a real host on anyone's network.
    SocketAddr::from(([203, 0, 113, 50], 40000))
}

fn bearer_headers(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    headers
}

#[tokio::test]
async fn admin_reload_loopback_no_token_reloads() {
    let tmp = tempfile::tempdir().unwrap();
    let state = admin_reload_state(&tmp, false, true, &[]);
    let resp = handle_admin_reload(State(state), ConnectInfo(loopback_peer()), HeaderMap::new())
        .await
        .unwrap()
        .into_response();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn admin_reload_remote_default_off_is_forbidden() {
    let tmp = tempfile::tempdir().unwrap();
    let state = admin_reload_state(&tmp, false, true, &[]);
    let err = handle_admin_reload(State(state), ConnectInfo(remote_peer()), HeaderMap::new())
        .await
        .err()
        .unwrap();
    assert_eq!(err.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn admin_reload_remote_opt_in_without_pairing_does_not_reload() {
    // The fixed hole: allow_remote_admin = true + require_pairing = false
    // must NOT permit an anonymous remote reload.
    let tmp = tempfile::tempdir().unwrap();
    let state = admin_reload_state(&tmp, true, false, &[]);
    let err = handle_admin_reload(State(state), ConnectInfo(remote_peer()), HeaderMap::new())
        .await
        .err()
        .unwrap();
    assert_eq!(err.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn admin_reload_remote_opt_in_missing_token_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let state = admin_reload_state(&tmp, true, true, &["zc_test_token".to_string()]);
    let err = handle_admin_reload(State(state), ConnectInfo(remote_peer()), HeaderMap::new())
        .await
        .err()
        .unwrap();
    assert_eq!(err.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn admin_reload_remote_opt_in_invalid_token_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let state = admin_reload_state(&tmp, true, true, &["zc_test_token".to_string()]);
    let err = handle_admin_reload(
        State(state),
        ConnectInfo(remote_peer()),
        bearer_headers("not-the-token"),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(err.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn admin_reload_remote_opt_in_valid_token_reloads() {
    let tmp = tempfile::tempdir().unwrap();
    let state = admin_reload_state(&tmp, true, true, &["zc_test_token".to_string()]);
    let resp = handle_admin_reload(
        State(state),
        ConnectInfo(remote_peer()),
        bearer_headers("zc_test_token"),
    )
    .await
    .unwrap()
    .into_response();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[test]
fn needs_quickstart_for_flags_empty_model() {
    let err = needs_quickstart_for("").expect("empty model must produce a needs_quickstart error");
    let msg = err.to_string();
    assert!(
        msg.contains("needs_quickstart"),
        "error must carry the needs_quickstart marker for callers to map to 503; got: {msg}"
    );
    assert!(
        msg.contains("/quickstart"),
        "error must point the user at /quickstart; got: {msg}"
    );
}

#[test]
fn needs_quickstart_for_flags_whitespace_only_model() {
    assert!(
        needs_quickstart_for("   ").is_some(),
        "whitespace-only model must be treated as empty"
    );
    assert!(
        needs_quickstart_for("\n\t ").is_some(),
        "tabs and newlines count as empty too"
    );
}

#[test]
fn needs_quickstart_for_passes_real_model() {
    assert!(
        needs_quickstart_for("anthropic/claude-sonnet-4").is_none(),
        "a real model id must not be flagged"
    );
    assert!(
        needs_quickstart_for("  gpt-4  ").is_none(),
        "leading/trailing whitespace around a real model id must not be flagged"
    );
}

#[test]
fn is_needs_quickstart_err_detects_marker_from_helper() {
    let err = needs_quickstart_for("").expect("empty model produces marker");
    assert!(
        is_needs_quickstart_err(&err),
        "the marker emitted by needs_quickstart_for must be detected"
    );
}

#[test]
fn is_needs_quickstart_err_ignores_unrelated_errors() {
    let err = anyhow::Error::msg("upstream timeout: provider returned 504");
    assert!(
        !is_needs_quickstart_err(&err),
        "unrelated errors must not be misclassified as needs_quickstart"
    );
    let err = anyhow::Error::msg("invalid api key");
    assert!(!is_needs_quickstart_err(&err));
}

#[test]
fn is_needs_quickstart_err_detects_via_substring() {
    // Defends the contract that the substring marker is the
    // detection key — not the exact string. Wrappers (e.g.
    // anyhow::Error::context) must not break the check.
    let err = anyhow::Error::msg("provider call failed").context("needs_quickstart: empty model");
    assert!(is_needs_quickstart_err(&err));
}

#[test]
fn needs_quickstart_channel_reply_resolves_via_fluent() {
    let reply = needs_quickstart_channel_reply();
    assert!(
        !reply.starts_with('{') && !reply.ends_with('}'),
        "fluent missing-key fallback leaked into channel reply: {reply:?}"
    );
    assert!(
        reply.to_lowercase().contains("quickstart"),
        "channel reply must mention Quickstart so users know what's missing: {reply:?}"
    );
}

// ══════════════════════════════════════════════════════════
// Linq Multi-Tenant Webhook Routing Tests
// ══════════════════════════════════════════════════════════

/// Helper: compute a valid Linq HMAC-SHA256 signature for the given
/// secret, timestamp, and body.  Mirrors the verification logic in
/// `zeroclaw_channels::linq::verify_linq_signature`.
#[cfg(feature = "channel-linq")]
fn compute_linq_signature_hex(secret: &str, timestamp: &str, body: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let message = format!("{timestamp}.{body}");
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(message.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Helper: build a minimal Linq webhook payload that `parse_webhook_payload`
/// recognises as a `message.received` event with one text part.
#[cfg(feature = "channel-linq")]
fn linq_webhook_body(sender: &str, text: &str) -> String {
    serde_json::json!({
        "event_type": "message.received",
        "data": {
            "sender": { "phone": sender },
            "message": {
                "parts": [{ "type": "text", "value": text }]
            }
        }
    })
    .to_string()
}

/// Helper: build an `AppState` with one Linq channel registered under the
/// given alias, with an allow-any peer resolver and an optional signing
/// secret.
#[cfg(feature = "channel-linq")]
fn linq_test_state(alias: &str, signing_secret: Option<&str>) -> AppState {
    let model_provider: Arc<dyn ModelProvider> = Arc::new(MockModelProvider::default());
    let memory: Arc<dyn Memory> = Arc::new(MockMemory);

    let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> =
        Arc::new(|| vec!["*".to_string()]);
    let channel = Arc::new(LinqChannel::new(
        "test-token".into(),
        "+15550000000".into(),
        alias,
        peer_resolver,
    ));
    let mut linq = HashMap::new();
    linq.insert(alias.to_string(), channel);

    let mut linq_signing_secrets: HashMap<String, Arc<str>> = HashMap::new();
    if let Some(secret) = signing_secret {
        linq_signing_secrets.insert(alias.to_string(), Arc::from(secret));
    }

    AppState {
        config: Arc::new(RwLock::new(Config::default())),
        config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
        model_provider,
        model: "test-model".into(),
        temperature: None,
        mem: memory,
        memory_strategy: Arc::new(DefaultMemoryStrategy::with_config(
            Arc::new(MockMemory),
            zeroclaw_config::schema::MemoryConfig::default(),
            std::path::PathBuf::new(),
        )),
        companion_store: None,
        auto_save: false,
        webhook_secret_hash: None,
        pairing: Arc::new(PairingGuard::new(false, &[])),
        trust_forwarded_headers: false,
        rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100)),
        auth_limiter: Arc::new(auth_rate_limit::AuthRateLimiter::new()),
        idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp: HashMap::new(),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp_app_secret: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq,
        #[cfg(feature = "channel-linq")]
        linq_signing_secrets,
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk_webhook_secret: HashMap::new(),
        #[cfg(feature = "channel-wati")]
        wati: HashMap::new(),
        #[cfg(feature = "channel-email")]
        gmail_push: None,
        observer: Arc::new(zeroclaw_runtime::observability::NoopObserver),
        tools_registry: Arc::new(Vec::new()),
        tools_registry_by_agent: Arc::new(std::collections::HashMap::new()),
        cost_tracker: None,
        event_tx: tokio::sync::broadcast::channel(16).0,
        event_buffer: Arc::new(sse::EventBuffer::new(16)),
        shutdown_tx: tokio::sync::watch::channel(false).0,
        reload_tx: None,
        #[cfg(feature = "nodes")]
        node_registry: Arc::new(nodes::NodeRegistry::new(16)),
        #[cfg(feature = "nodes")]
        mdns_peer_registry: nodes::mdns::MdnsPeerRegistry::default(),
        path_prefix: String::new(),
        web_dist_dir: None,
        session_backend: None,
        session_queue: std::sync::Arc::new(crate::session_queue::SessionActorQueue::new(
            8, 30, 600,
        )),
        device_registry: None,
        pending_pairings: None,
        canvas_store: CanvasStore::new(),
        cancel_tokens: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        pending_reload: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        tui_registry: None,
        #[cfg(feature = "webauthn")]
        webauthn: None,
    }
}

#[cfg(feature = "channel-linq")]
#[tokio::test]
async fn linq_webhook_returns_not_found_for_unknown_alias() {
    // No Linq channels configured at all.
    let state = linq_test_state("production", None);

    let response = Box::pin(handle_linq_webhook_alias(
        State(state),
        Path("staging".to_string()),
        HeaderMap::new(),
        Bytes::from_static(br#"{"event_type":"message.received"}"#),
    ))
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[cfg(feature = "channel-linq")]
#[tokio::test]
async fn linq_webhook_returns_not_found_when_no_channels_configured() {
    let model_provider: Arc<dyn ModelProvider> = Arc::new(MockModelProvider::default());
    let memory: Arc<dyn Memory> = Arc::new(MockMemory);

    let state = AppState {
        config: Arc::new(RwLock::new(Config::default())),
        config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
        model_provider,
        model: "test-model".into(),
        temperature: None,
        mem: memory,
        memory_strategy: Arc::new(DefaultMemoryStrategy::with_config(
            Arc::new(MockMemory),
            zeroclaw_config::schema::MemoryConfig::default(),
            std::path::PathBuf::new(),
        )),
        companion_store: None,
        auto_save: false,
        webhook_secret_hash: None,
        pairing: Arc::new(PairingGuard::new(false, &[])),
        trust_forwarded_headers: false,
        rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100)),
        auth_limiter: Arc::new(auth_rate_limit::AuthRateLimiter::new()),
        idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp: HashMap::new(),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp_app_secret: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq_signing_secrets: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk_webhook_secret: HashMap::new(),
        #[cfg(feature = "channel-wati")]
        wati: HashMap::new(),
        #[cfg(feature = "channel-email")]
        gmail_push: None,
        observer: Arc::new(zeroclaw_runtime::observability::NoopObserver),
        tools_registry: Arc::new(Vec::new()),
        tools_registry_by_agent: Arc::new(std::collections::HashMap::new()),
        cost_tracker: None,
        event_tx: tokio::sync::broadcast::channel(16).0,
        event_buffer: Arc::new(sse::EventBuffer::new(16)),
        shutdown_tx: tokio::sync::watch::channel(false).0,
        reload_tx: None,
        #[cfg(feature = "nodes")]
        node_registry: Arc::new(nodes::NodeRegistry::new(16)),
        #[cfg(feature = "nodes")]
        mdns_peer_registry: nodes::mdns::MdnsPeerRegistry::default(),
        path_prefix: String::new(),
        web_dist_dir: None,
        session_backend: None,
        session_queue: std::sync::Arc::new(crate::session_queue::SessionActorQueue::new(
            8, 30, 600,
        )),
        device_registry: None,
        pending_pairings: None,
        canvas_store: CanvasStore::new(),
        cancel_tokens: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        pending_reload: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        tui_registry: None,
        #[cfg(feature = "webauthn")]
        webauthn: None,
    };

    let response = Box::pin(handle_linq_webhook_alias(
        State(state),
        Path("default".to_string()),
        HeaderMap::new(),
        Bytes::from_static(br#"{"event_type":"message.received"}"#),
    ))
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[cfg(feature = "channel-linq")]
#[tokio::test]
async fn linq_webhook_accepts_valid_message_for_known_alias() {
    let state = linq_test_state("default", None);
    let body = linq_webhook_body("+15551234567", "hello from test");

    let response = Box::pin(handle_linq_webhook_alias(
        State(state),
        Path("default".to_string()),
        HeaderMap::new(),
        Bytes::from(body),
    ))
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
}

#[cfg(feature = "channel-linq")]
#[tokio::test]
async fn linq_webhook_rejects_invalid_signature_for_alias() {
    let secret = generate_test_secret();
    let state = linq_test_state("secure-alias", Some(&secret));

    let body = linq_webhook_body("+15551234567", "hello from test");
    let mut headers = HeaderMap::new();
    headers.insert(
        "X-Webhook-Signature",
        HeaderValue::from_static("sha256=deadbeef"),
    );
    headers.insert(
        "X-Webhook-Timestamp",
        HeaderValue::from_static("9999999999"),
    );

    let response = Box::pin(handle_linq_webhook_alias(
        State(state),
        Path("secure-alias".to_string()),
        headers,
        Bytes::from(body),
    ))
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[cfg(feature = "channel-linq")]
#[tokio::test]
async fn linq_webhook_accepts_valid_signature_for_alias() {
    let secret = generate_test_secret();
    let state = linq_test_state("secure-alias", Some(&secret));

    let body = linq_webhook_body("+15551234567", "hello from test");
    let timestamp = chrono::Utc::now().timestamp().to_string();
    let sig = compute_linq_signature_hex(&secret, &timestamp, &body);

    let mut headers = HeaderMap::new();
    headers.insert(
        "X-Webhook-Signature",
        HeaderValue::from_str(&format!("sha256={sig}")).unwrap(),
    );
    headers.insert(
        "X-Webhook-Timestamp",
        HeaderValue::from_str(&timestamp).unwrap(),
    );

    let response = Box::pin(handle_linq_webhook_alias(
        State(state),
        Path("secure-alias".to_string()),
        headers,
        Bytes::from(body),
    ))
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
}

// ── Per-alias webhook routing───────────────────────────────────

/// Baseline `AppState` with no channels configured, for the per-alias
/// routing tests. Tests insert the WhatsApp instances they exercise.
#[cfg(feature = "channel-whatsapp-cloud")]
fn webhook_baseline_state() -> AppState {
    let model_provider: Arc<dyn ModelProvider> = Arc::new(MockModelProvider::default());
    let mem: Arc<dyn Memory> = Arc::new(MockMemory);
    AppState {
        config: Arc::new(RwLock::new(Config::default())),
        config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
        model_provider,
        model: "test-model".into(),
        temperature: None,
        mem,
        memory_strategy: Arc::new(DefaultMemoryStrategy::with_config(
            Arc::new(MockMemory),
            zeroclaw_config::schema::MemoryConfig::default(),
            std::path::PathBuf::new(),
        )),
        companion_store: None,
        auto_save: false,
        webhook_secret_hash: None,
        pairing: Arc::new(PairingGuard::new(false, &[])),
        trust_forwarded_headers: false,
        rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100)),
        auth_limiter: Arc::new(auth_rate_limit::AuthRateLimiter::new()),
        idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp: HashMap::new(),
        #[cfg(feature = "channel-whatsapp-cloud")]
        whatsapp_app_secret: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq: HashMap::new(),
        #[cfg(feature = "channel-linq")]
        linq_signing_secrets: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk: HashMap::new(),
        #[cfg(feature = "channel-nextcloud")]
        nextcloud_talk_webhook_secret: HashMap::new(),
        #[cfg(feature = "channel-wati")]
        wati: HashMap::new(),
        #[cfg(feature = "channel-email")]
        gmail_push: None,
        observer: Arc::new(zeroclaw_runtime::observability::NoopObserver),
        tools_registry: Arc::new(Vec::new()),
        tools_registry_by_agent: Arc::new(std::collections::HashMap::new()),
        cost_tracker: None,
        event_tx: tokio::sync::broadcast::channel(16).0,
        event_buffer: Arc::new(sse::EventBuffer::new(16)),
        shutdown_tx: tokio::sync::watch::channel(false).0,
        reload_tx: None,
        #[cfg(feature = "nodes")]
        node_registry: Arc::new(nodes::NodeRegistry::new(16)),
        #[cfg(feature = "nodes")]
        mdns_peer_registry: nodes::mdns::MdnsPeerRegistry::default(),
        path_prefix: String::new(),
        web_dist_dir: None,
        session_backend: None,
        session_queue: std::sync::Arc::new(crate::session_queue::SessionActorQueue::new(
            8, 30, 600,
        )),
        device_registry: None,
        pending_pairings: None,
        canvas_store: CanvasStore::new(),
        cancel_tokens: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        pending_reload: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        tui_registry: None,
        #[cfg(feature = "webauthn")]
        webauthn: None,
    }
}

#[cfg(feature = "channel-whatsapp-cloud")]
fn whatsapp_instance(alias: &str, verify_token: &str) -> Arc<WhatsAppChannel> {
    let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = Arc::new(Vec::new);
    Arc::new(WhatsAppChannel::new(
        "access-token".into(),
        "phone-number-id".into(),
        verify_token.into(),
        alias.to_string(),
        peer_resolver,
    ))
}

#[cfg(feature = "channel-whatsapp-cloud")]
fn whatsapp_signature(secret: &str, body: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

#[cfg(feature = "channel-whatsapp-cloud")]
fn verify_query(token: &str, challenge: &str) -> WhatsAppVerifyQuery {
    WhatsAppVerifyQuery {
        mode: Some("subscribe".to_string()),
        verify_token: Some(token.to_string()),
        challenge: Some(challenge.to_string()),
    }
}

#[cfg(feature = "channel-whatsapp-cloud")]
#[tokio::test]
async fn webhook_alias_routes_to_the_matching_instance() {
    let mut state = webhook_baseline_state();
    state.whatsapp = HashMap::from([
        ("work".to_string(), whatsapp_instance("work", "tok-work")),
        (
            "personal".to_string(),
            whatsapp_instance("personal", "tok-personal"),
        ),
    ]);

    let resp = handle_whatsapp_verify_alias(
        State(state.clone()),
        Path("work".to_string()),
        Query(verify_query("tok-work", "challenge-work")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    // Explicit alias path carries no deprecation header.
    assert!(
        resp.headers()
            .get(api_webhook::DEPRECATION_HEADER)
            .is_none()
    );

    // The other instance's token must NOT verify against `work`.
    let resp = handle_whatsapp_verify_alias(
        State(state.clone()),
        Path("work".to_string()),
        Query(verify_query("tok-personal", "challenge")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let resp = handle_whatsapp_verify_alias(
        State(state),
        Path("personal".to_string()),
        Query(verify_query("tok-personal", "challenge-personal")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[cfg(feature = "channel-whatsapp-cloud")]
#[tokio::test]
async fn webhook_unknown_alias_is_404_not_500() {
    let mut state = webhook_baseline_state();
    state.whatsapp = HashMap::from([("work".to_string(), whatsapp_instance("work", "tok"))]);

    let resp = handle_whatsapp_verify_alias(
        State(state),
        Path("nope".to_string()),
        Query(verify_query("tok", "challenge")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[cfg(feature = "channel-whatsapp-cloud")]
#[tokio::test]
async fn webhook_bare_path_is_back_compat_and_flags_deprecation() {
    let mut state = webhook_baseline_state();
    state.whatsapp = HashMap::from([("default".to_string(), whatsapp_instance("default", "tok"))]);

    let resp = handle_whatsapp_verify(
        State(state),
        Query(verify_query("tok", "challenge-default")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()
            .get(api_webhook::DEPRECATION_HEADER)
            .is_some()
    );
}

#[cfg(feature = "channel-whatsapp-cloud")]
#[tokio::test]
async fn webhook_alias_path_preserves_signature_auth() {
    let mut state = webhook_baseline_state();
    state.whatsapp = HashMap::from([("work".to_string(), whatsapp_instance("work", "tok"))]);
    state.whatsapp_app_secret =
        HashMap::from([("work".to_string(), Arc::<str>::from("app-secret"))]);

    // Unknown alias → 404 before any processing.
    let resp = Box::pin(handle_whatsapp_message_alias(
        State(state.clone()),
        Path("nope".to_string()),
        HeaderMap::new(),
        Bytes::from_static(b"{}"),
    ))
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Configured alias, missing/invalid signature → 401.
    let resp = Box::pin(handle_whatsapp_message_alias(
        State(state.clone()),
        Path("work".to_string()),
        HeaderMap::new(),
        Bytes::from_static(b"{}"),
    ))
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Configured alias, valid signature over an empty payload → 200 ack.
    let body = br#"{"object":"whatsapp_business_account","entry":[]}"#;
    let mut headers = HeaderMap::new();
    headers.insert(
        "X-Hub-Signature-256",
        HeaderValue::from_str(&whatsapp_signature("app-secret", body)).unwrap(),
    );
    let resp = Box::pin(handle_whatsapp_message_alias(
        State(state),
        Path("work".to_string()),
        headers,
        Bytes::from_static(body),
    ))
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// Build an `AppState` whose device registry points at a non-existent
/// path so every SQLite write fails. Mirrors `unwriteable_registry_state`
/// in `api_pairing::tests` so the regression set stays side-by-side.
fn unwriteable_registry_pair_state(tmp: &tempfile::TempDir) -> AppState {
    let mut state = admin_paircode_state(tmp, true, false);
    // No registry from `admin_paircode_state`; inject the broken one.
    state.device_registry = Some(Arc::new(api_pairing::DeviceRegistry::with_db_path(
        std::path::PathBuf::from("/this/path/does/not/exist/devices.db"),
    )));
    state
}

async fn legacy_pair_response_json(result: impl IntoResponse) -> (StatusCode, serde_json::Value) {
    let response = result.into_response();
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("legacy /pair response body")
        .to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    (status, body)
}

#[tokio::test]
async fn legacy_pair_rolls_back_in_process_token_when_registry_register_fails() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = unwriteable_registry_pair_state(&tmp);

    let code = state
        .pairing
        .generate_new_pairing_code()
        .expect("pairing code must be issuable when require_pairing=true");

    let mut headers = HeaderMap::new();
    headers.insert("X-Pairing-Code", HeaderValue::from_str(&code).unwrap());

    let (status, body) = legacy_pair_response_json(
        handle_pair(State(state.clone()), test_connect_info(), headers).await,
    )
    .await;

    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "legacy /pair registry.register failure must surface as 500"
    );
    assert_eq!(body["paired"], serde_json::Value::Bool(false));
    assert!(
        body.get("token").is_none(),
        "legacy /pair 5xx body MUST NOT contain the plaintext bearer token; got: {body}"
    );
    assert!(
        state.pairing.tokens().is_empty(),
        "PairingGuard::paired_tokens must be empty after a failed /pair \
         registry.register (compensating `revoke_token_hash`); instead have {:?}",
        state.pairing.tokens()
    );
}

#[tokio::test]
async fn legacy_pair_rolls_back_in_process_token_when_persist_fails() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = admin_paircode_state(&tmp, true, false);
    let blocker = tmp.path().join("legacy-pair-blocker");
    std::fs::write(&blocker, b"").expect("seed blocker file");
    state.config.write().config_path = blocker.join("config.toml");

    let code = state
        .pairing
        .generate_new_pairing_code()
        .expect("pairing code must be issuable when require_pairing=true");

    let mut headers = HeaderMap::new();
    headers.insert("X-Pairing-Code", HeaderValue::from_str(&code).unwrap());

    let (status, body) = legacy_pair_response_json(
        handle_pair(State(state.clone()), test_connect_info(), headers).await,
    )
    .await;

    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "legacy /pair persistence failure MUST surface as 500 (legacy leaked 200 + token)"
    );
    assert_eq!(body["paired"], serde_json::Value::Bool(false));
    assert!(
        body.get("token").is_none(),
        "legacy /pair 5xx body MUST NOT contain the plaintext bearer token; got: {body}"
    );
    assert!(
        state.pairing.tokens().is_empty(),
        "PairingGuard::paired_tokens must be empty after a failed /pair \
         persist; have {:?}",
        state.pairing.tokens()
    );
}

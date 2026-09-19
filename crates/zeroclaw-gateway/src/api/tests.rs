use super::*;
#[cfg(feature = "nodes")]
use crate::nodes;
use crate::{AppState, GatewayRateLimiter, IdempotencyStore};
use async_trait::async_trait;
use axum::response::IntoResponse;
use http_body_util::BodyExt;
use parking_lot::RwLock;
#[cfg(feature = "channel-linq")]
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
#[cfg(feature = "nodes")]
use std::time::Instant;
use zeroclaw_infra::session_backend::SessionBackend;
use zeroclaw_infra::session_store::SessionStore;
use zeroclaw_memory::{Memory, MemoryCategory, MemoryEntry};
use zeroclaw_providers::ModelProvider;
use zeroclaw_runtime::security::pairing::PairingGuard;

#[derive(Default)]
struct MockMemory {
    entries: Vec<MemoryEntry>,
}

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
        Ok(self.entries.clone())
    }

    async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        Ok(None)
    }

    async fn list(
        &self,
        _category: Option<&MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        Ok(self.entries.clone())
    }

    async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn forget_for_agent(&self, _key: &str, _agent_id: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn count(&self) -> anyhow::Result<usize> {
        Ok(self.entries.len())
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

    async fn purge_agent(&self, _agent_alias: &str) -> anyhow::Result<usize> {
        Ok(0)
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

struct MockModelProvider;

#[async_trait]
impl ModelProvider for MockModelProvider {
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

/// Wire a minimal agent + model_provider + risk_profile into a test config
/// so cron-add API tests have an `agent` reference to bind to.
fn with_test_agent(mut config: zeroclaw_config::schema::Config) -> zeroclaw_config::schema::Config {
    config.providers.models.openrouter.insert(
        "default".to_string(),
        zeroclaw_config::schema::OpenRouterModelProviderConfig::default(),
    );
    config.risk_profiles.insert(
        "test-profile".to_string(),
        zeroclaw_config::schema::RiskProfileConfig::default(),
    );
    config.agents.insert(
        "test-agent".to_string(),
        zeroclaw_config::schema::AliasedAgentConfig {
            model_provider: "openrouter.default".into(),
            risk_profile: "test-profile".into(),
            ..Default::default()
        },
    );
    config
}

pub(crate) fn test_state(config: zeroclaw_config::schema::Config) -> AppState {
    AppState {
        config: Arc::new(RwLock::new(config)),
        config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
        model_provider: Arc::new(MockModelProvider),
        model: "test-model".into(),
        temperature: None,
        mem: Arc::new(MockMemory::default()),
        memory_strategy: Arc::new(
            zeroclaw_runtime::agent::memory_strategy::DefaultMemoryStrategy::with_config(
                Arc::new(MockMemory::default()),
                zeroclaw_config::schema::MemoryConfig::default(),
                std::path::PathBuf::new(),
            ),
        ),
        companion_store: None,
        user_model: None,
        auto_save: false,
        webhook_secret_hash: None,
        pairing: Arc::new(PairingGuard::new(false, &[])),
        trust_forwarded_headers: false,
        rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100)),
        auth_limiter: Arc::new(crate::auth_rate_limit::AuthRateLimiter::new()),
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
        event_buffer: Arc::new(crate::sse::EventBuffer::new(16)),
        shutdown_tx: tokio::sync::watch::channel(false).0,
        #[cfg(feature = "nodes")]
        node_registry: Arc::new(nodes::NodeRegistry::new(16)),
        #[cfg(feature = "nodes")]
        mdns_peer_registry: nodes::mdns::MdnsPeerRegistry::default(),
        session_backend: None,
        session_queue: Arc::new(crate::session_queue::SessionActorQueue::new(8, 30, 600)),
        device_registry: None,
        pending_pairings: None,
        path_prefix: String::new(),
        web_dist_dir: None,
        canvas_store: zeroclaw_runtime::tools::CanvasStore::new(),
        cancel_tokens: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        pending_reload: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        tui_registry: None,
        reload_tx: None,
        #[cfg(feature = "webauthn")]
        webauthn: None,
    }
}

fn test_state_with_memory(
    config: zeroclaw_config::schema::Config,
    entries: Vec<MemoryEntry>,
) -> AppState {
    AppState {
        mem: Arc::new(MockMemory { entries }),
        ..test_state(config)
    }
}

async fn response_json(response: axum::response::Response) -> serde_json::Value {
    let body = response
        .into_body()
        .collect()
        .await
        .expect("response body")
        .to_bytes();
    serde_json::from_slice(&body).expect("valid json response")
}

#[test]
fn companion_outbox_from_state_is_not_configured_without_a_store() {
    let state = test_state(zeroclaw_config::schema::Config::default());
    let health = companion_outbox_from_state(&state);
    assert_eq!(
        health.status,
        zeroclaw_api::companion::CompanionOutboxStatus::NotConfigured
    );
    assert_eq!(health.pending_count, 0);
    assert_eq!(health.oldest_pending_age_secs, None);
}

#[tokio::test]
async fn api_health_reports_not_configured_companion_outbox_without_a_store() {
    let state = test_state(zeroclaw_config::schema::Config::default());
    let response = handle_api_health(State(state), HeaderMap::new())
        .await
        .into_response();
    let body = response_json(response).await;
    assert_eq!(body["companion_outbox"]["status"], "not_configured");
    assert_eq!(body["companion_outbox"]["pending_count"], 0);
    assert!(body["companion_outbox"]["oldest_pending_age_secs"].is_null());
    let encoded = body["companion_outbox"].to_string();
    assert!(!encoded.contains("synchronized"), "{encoded}");
    assert!(!encoded.contains("accumulating"), "{encoded}");
}

#[tokio::test]
async fn repeated_api_health_calls_do_not_emit_stale_outbox_warn() {
    let _writer_guard = zeroclaw_log::__private_test_writer_lock();
    let _hook_guard = zeroclaw_log::__private_test_hook_lock();
    zeroclaw_log::try_install_capture_subscriber();
    let mut rx = zeroclaw_log::subscribe_or_install();
    while rx.try_recv().is_ok() {}

    let state = test_state(zeroclaw_config::schema::Config::default());
    for _ in 0..3 {
        let response = handle_api_health(State(state.clone()), HeaderMap::new())
            .await
            .into_response();
        let body = response_json(response).await;
        assert_eq!(body["companion_outbox"]["status"], "not_configured");
        let encoded = body["companion_outbox"].to_string();
        assert!(!encoded.contains("accumulating"), "{encoded}");
        assert!(!encoded.contains("synchronized"), "{encoded}");
    }

    let mut found_stale_warn = false;
    loop {
        match rx.try_recv() {
            Ok(value) => {
                if value.get("severity_text").and_then(|v| v.as_str()) != Some("WARN") {
                    continue;
                }
                let message = value.get("message").and_then(|v| v.as_str()).unwrap_or("");
                if message.contains("aging with no drain configured") {
                    found_stale_warn = true;
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(
                tokio::sync::broadcast::error::TryRecvError::Empty
                | tokio::sync::broadcast::error::TryRecvError::Closed,
            ) => break,
        }
    }
    zeroclaw_log::clear_broadcast_hook();
    assert!(
        !found_stale_warn,
        "consecutive /api/health reads must not emit stale outbox WARN"
    );
}

#[tokio::test]
#[cfg(feature = "nodes")]
async fn api_status_includes_connected_nodes_and_mdns_peers() {
    let state = test_state(zeroclaw_config::schema::Config::default());
    let (invoke_tx, _invoke_rx) = tokio::sync::mpsc::channel(1);
    assert!(state.node_registry.register(nodes::NodeInfo {
        node_id: "connected-node".into(),
        capabilities: Vec::new(),
        invoke_tx,
    }));
    state.mdns_peer_registry.insert(
        "peer-1".into(),
        nodes::mdns::MdnsPeer {
            name: "peer-one".into(),
            addr: "10.0.0.2".into(),
            port: 42617,
            version: "0.8.2".into(),
            path_prefix: Some("/peer".into()),
            last_seen: Instant::now(),
        },
    );

    let response = handle_api_status(
        State(state),
        HeaderMap::new(),
        Query(StatusQuery { agent: None }),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);

    let json = response_json(response).await;
    assert_eq!(
        json["nodes"]["connected"],
        serde_json::json!(["connected-node"])
    );
    assert_eq!(
        json["nodes"]["mdns_peers"],
        serde_json::json!([{
            "id": "peer-1",
            "name": "peer-one",
            "addr": "10.0.0.2",
            "port": 42617,
            "version": "0.8.2",
            "path_prefix": "/peer",
            "base_url": "http://10.0.0.2:42617/peer",
        }])
    );
}

#[test]
fn integration_entry_json_derives_category_label_from_category() {
    let entry = zeroclaw_runtime::integrations::IntegrationEntry {
        name: "Browser".into(),
        description: "Run browser automation".into(),
        category: zeroclaw_runtime::integrations::IntegrationCategory::ToolsAutomation,
        status: zeroclaw_runtime::integrations::IntegrationStatus::Active,
    };

    let json = integration_entry_json(&entry);

    assert_eq!(json["category"], "ToolsAutomation");
    assert_eq!(json["category_label"], "Tools & Automation");
    assert_eq!(json["status"], "Active");
}

fn memory_entry_with_content(content: String) -> MemoryEntry {
    MemoryEntry {
        id: "entry-1".into(),
        key: "huge-memory".into(),
        content,
        category: MemoryCategory::Conversation,
        timestamp: "2026-04-06T00:00:00Z".into(),
        session_id: None,
        score: None,
        namespace: "default".into(),
        importance: Some(0.5),
        superseded_by: None,
        kind: None,
        pinned: false,
        tenant_id: None,
        agent_alias: None,
        agent_id: None,
    }
}

fn memory_content_from_response(json: &serde_json::Value) -> &str {
    json["entries"][0]["content"]
        .as_str()
        .expect("string content")
}

#[test]
fn truncate_memory_api_content_caps_total_chars_with_ellipsis() {
    let exact = "x".repeat(MEMORY_API_CONTENT_MAX_CHARS);
    assert_eq!(truncate_with_ellipsis_total_chars(exact.clone()), exact);

    let short = "short memory".to_string();
    assert_eq!(truncate_with_ellipsis_total_chars(short.clone()), short);

    let over = "火".repeat(MEMORY_API_CONTENT_MAX_CHARS + 1);
    let truncated = truncate_with_ellipsis_total_chars(over.clone());
    assert_eq!(truncated.chars().count(), MEMORY_API_CONTENT_MAX_CHARS);
    assert!(truncated.ends_with("..."));
    assert_ne!(truncated, over);
}

#[tokio::test]
async fn handle_api_memory_list_truncates_oversized_content() {
    let mut config = zeroclaw_config::schema::Config::default();
    config.gateway.require_pairing = false;
    let huge = "x".repeat(MEMORY_API_CONTENT_MAX_CHARS + 128);
    let state = test_state_with_memory(config, vec![memory_entry_with_content(huge.clone())]);

    let response = handle_api_memory_list(
        State(state),
        HeaderMap::new(),
        Query(MemoryQuery {
            query: None,
            category: None,
            since: None,
            until: None,
            agent: None,
        }),
    )
    .await
    .into_response();

    let json = response_json(response).await;
    let content = memory_content_from_response(&json);

    assert_eq!(content.chars().count(), MEMORY_API_CONTENT_MAX_CHARS);
    assert!(content.ends_with("..."));
    assert_eq!(json["entries"][0]["key"], "huge-memory");
    assert_eq!(json["entries"][0]["category"], "conversation");
    assert_ne!(content, huge);
}

#[tokio::test]
async fn handle_api_memory_search_truncates_oversized_content_after_filtering() {
    let mut config = zeroclaw_config::schema::Config::default();
    config.gateway.require_pairing = false;
    let huge = "火".repeat(MEMORY_API_CONTENT_MAX_CHARS + 128);
    let state = test_state_with_memory(config, vec![memory_entry_with_content(huge.clone())]);

    let response = handle_api_memory_list(
        State(state),
        HeaderMap::new(),
        Query(MemoryQuery {
            query: Some("huge".into()),
            category: Some("conversation".into()),
            since: None,
            until: None,
            agent: None,
        }),
    )
    .await
    .into_response();

    let json = response_json(response).await;
    let content = memory_content_from_response(&json);

    assert_eq!(content.chars().count(), MEMORY_API_CONTENT_MAX_CHARS);
    assert!(content.ends_with("..."));
    assert_ne!(content, huge);
}

#[tokio::test]
async fn handle_api_tools_scopes_listing_by_agent_query() {
    use zeroclaw_api::tool::ToolSpec;

    let mut config = zeroclaw_config::schema::Config::default();
    config.gateway.require_pairing = false;
    let mut state = test_state(config);

    let spec = |name: &str| {
        ToolSpec::new(
            name.to_string(),
            format!("{name} desc"),
            serde_json::json!({}),
        )
    };
    state.tools_registry = Arc::new(vec![spec("default_tool")]);
    let mut by_agent: std::collections::HashMap<String, Arc<Vec<ToolSpec>>> =
        std::collections::HashMap::new();
    by_agent.insert("alpha".to_string(), Arc::new(vec![spec("alpha_tool")]));
    by_agent.insert("beta".to_string(), Arc::new(vec![spec("beta_tool")]));
    state.tools_registry_by_agent = Arc::new(by_agent);

    async fn tool_names(state: AppState, agent: Option<&str>) -> Vec<String> {
        let response = handle_api_tools(
            State(state),
            HeaderMap::new(),
            Query(ToolsQuery {
                agent: agent.map(str::to_string),
            }),
        )
        .await
        .into_response();
        response_json(response).await["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    // A known agent gets its own scoped listing.
    assert_eq!(
        tool_names(state.clone(), Some("beta")).await,
        vec!["beta_tool".to_string()]
    );
    // Omitted agent falls back to the default seed listing.
    assert_eq!(
        tool_names(state.clone(), None).await,
        vec!["default_tool".to_string()]
    );
    // Unknown and blank aliases fall back to the default rather than error,
    // so a stale UI selection still renders something.
    assert_eq!(
        tool_names(state.clone(), Some("ghost")).await,
        vec!["default_tool".to_string()]
    );
    assert_eq!(
        tool_names(state.clone(), Some("   ")).await,
        vec!["default_tool".to_string()]
    );
}

#[test]
fn api_channels_readiness_key_tracks_whatsapp_backend_type() {
    let mut config = zeroclaw_config::schema::Config::default();
    config.channels.whatsapp.insert(
        "web".to_string(),
        zeroclaw_config::schema::WhatsAppConfig {
            enabled: true,
            session_path: Some("~/.zeroclaw/state/whatsapp-web/session.db".into()),
            ..Default::default()
        },
    );
    config.channels.whatsapp.insert(
        "cloud".to_string(),
        zeroclaw_config::schema::WhatsAppConfig {
            enabled: true,
            access_token: Some("token".into()),
            phone_number_id: Some("phone-id".into()),
            verify_token: Some("verify".into()),
            ..Default::default()
        },
    );
    config.channels.whatsapp.insert(
        "ambiguous".to_string(),
        zeroclaw_config::schema::WhatsAppConfig {
            enabled: true,
            access_token: Some("token".into()),
            phone_number_id: Some("phone-id".into()),
            verify_token: Some("verify".into()),
            session_path: Some("~/.zeroclaw/state/whatsapp-web/session.db".into()),
            ..Default::default()
        },
    );

    let web = zeroclaw_config::schema::ChannelAliasInfo {
        channel_type: "whatsapp".to_string(),
        alias: "web".to_string(),
        owning_agent: None,
        enabled: true,
    };
    let cloud = zeroclaw_config::schema::ChannelAliasInfo {
        channel_type: "whatsapp".to_string(),
        alias: "cloud".to_string(),
        owning_agent: None,
        enabled: true,
    };
    let ambiguous = zeroclaw_config::schema::ChannelAliasInfo {
        channel_type: "whatsapp".to_string(),
        alias: "ambiguous".to_string(),
        owning_agent: None,
        enabled: true,
    };
    let discord = zeroclaw_config::schema::ChannelAliasInfo {
        channel_type: "discord".to_string(),
        alias: "default".to_string(),
        owning_agent: None,
        enabled: true,
    };

    assert_eq!(
        compiled_readiness_key_for_alias(&config, &web),
        "whatsapp-web"
    );
    assert_eq!(
        compiled_readiness_key_for_alias(&config, &cloud),
        "whatsapp"
    );
    assert_eq!(
        compiled_readiness_key_for_alias(&config, &ambiguous),
        "whatsapp",
        "ambiguous WhatsApp configs follow runtime Cloud precedence"
    );
    assert_eq!(
        compiled_readiness_key_for_alias(&config, &discord),
        "discord"
    );
}

#[cfg(not(feature = "channel-nextcloud"))]
#[tokio::test]
async fn api_channels_marks_configured_uncompiled_channel_unavailable() {
    let mut config = zeroclaw_config::schema::Config::default();
    config.channels.nextcloud_talk.insert(
        "default".to_string(),
        zeroclaw_config::schema::NextcloudTalkConfig {
            enabled: true,
            base_url: "https://cloud.example.com".to_string(),
            app_token: "test-token".to_string(),
            ..Default::default()
        },
    );

    let response = handle_api_channels(State(test_state(config)), HeaderMap::new())
        .await
        .into_response();
    let json = response_json(response).await;
    let channels = json["channels"].as_array().expect("channels array");
    let nextcloud = channels
        .iter()
        .find(|channel| channel["alias"] == "default")
        .expect("configured channel is listed");

    assert!(
        matches!(
            nextcloud["type"].as_str(),
            Some("nextcloud-talk" | "nextcloud_talk")
        ),
        "unexpected channel type: {}",
        nextcloud["type"]
    );
    assert_eq!(nextcloud["enabled"], true);
    assert_eq!(nextcloud["compiled"], false);
    assert_eq!(nextcloud["status"], "not_compiled");
    assert_eq!(nextcloud["health"], "unavailable");
}

/// Bind `channel_ref` (e.g. `"wechat.admin"`) to an enabled agent so
/// readiness reaches the authenticated/listening probes.
fn bind_channel_to_agent(config: &mut zeroclaw_config::schema::Config, channel_ref: &str) {
    config.agents.insert(
        "rowan".to_string(),
        zeroclaw_config::schema::AliasedAgentConfig {
            channels: vec![zeroclaw_config::providers::ChannelRef::new(
                channel_ref.to_string(),
            )],
            ..Default::default()
        },
    );
}

#[cfg(feature = "channel-wechat")]
#[tokio::test]
async fn api_channels_wechat_authenticated_tracks_persisted_login() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = zeroclaw_config::schema::Config::default();
    config.gateway.require_pairing = false;
    config.channels.wechat.insert(
        "admin".to_string(),
        zeroclaw_config::schema::WeChatConfig {
            enabled: true,
            state_dir: Some(temp.path().to_string_lossy().into_owned()),
            ..Default::default()
        },
    );
    bind_channel_to_agent(&mut config, "wechat.admin");

    // Unpaired: nothing persisted in the channel's state dir.
    let response = handle_api_channels(State(test_state(config.clone())), HeaderMap::new())
        .await
        .into_response();
    let json = response_json(response).await;
    let channel = json["channels"]
        .as_array()
        .expect("channels array")
        .iter()
        .find(|channel| channel["name"] == "wechat.admin")
        .cloned()
        .expect("wechat channel is listed");
    assert_eq!(channel["readiness"]["authenticated"], "missing");
    assert_eq!(channel["status"], "error");
    assert_eq!(channel["health"], "down");
    assert!(
        channel["readiness"]["requirements"]
            .as_array()
            .expect("requirements array")
            .iter()
            .any(|item| item
                .as_str()
                .is_some_and(|s| s.contains("Pair this channel")))
    );

    // Paired: the channel's own persisted login (account.json token).
    std::fs::write(
        temp.path().join("account.json"),
        r#"{"token": "tok_persisted", "account_id": "acct_1"}"#,
    )
    .unwrap();
    let response = handle_api_channels(State(test_state(config)), HeaderMap::new())
        .await
        .into_response();
    let json = response_json(response).await;
    let channel = json["channels"]
        .as_array()
        .expect("channels array")
        .iter()
        .find(|channel| channel["name"] == "wechat.admin")
        .cloned()
        .expect("wechat channel is listed");
    assert_eq!(channel["readiness"]["authenticated"], "ready");
    // Listener liveness is still unprobed, so the summary stays
    // conservative rather than claiming the channel is up.
    assert_eq!(channel["readiness"]["listening"], "unknown");
    assert_eq!(channel["status"], "unknown");
}

#[cfg(feature = "whatsapp-web")]
#[tokio::test]
async fn api_channels_whatsapp_web_unpaired_reports_missing_auth_without_touching_disk() {
    let temp = tempfile::tempdir().unwrap();
    let session_path = temp.path().join("session.db");
    let mut config = zeroclaw_config::schema::Config::default();
    config.gateway.require_pairing = false;
    config.channels.whatsapp.insert(
        "admin".to_string(),
        zeroclaw_config::schema::WhatsAppConfig {
            enabled: true,
            session_path: Some(session_path.to_string_lossy().into_owned()),
            ..Default::default()
        },
    );
    bind_channel_to_agent(&mut config, "whatsapp.admin");

    let response = handle_api_channels(State(test_state(config)), HeaderMap::new())
        .await
        .into_response();
    let json = response_json(response).await;
    let channel = json["channels"]
        .as_array()
        .expect("channels array")
        .iter()
        .find(|channel| channel["name"] == "whatsapp.admin")
        .cloned()
        .expect("whatsapp channel is listed");
    assert_eq!(channel["readiness"]["authenticated"], "missing");
    assert_eq!(channel["status"], "error");
    assert!(
        !session_path.exists(),
        "the readiness probe must never create the session database"
    );
}

#[tokio::test]
async fn api_channels_without_login_probe_keeps_authenticated_unknown() {
    let mut config = config_with_telegram("default");
    bind_channel_to_agent(&mut config, "telegram.default");

    let response = handle_api_channels(State(test_state(config)), HeaderMap::new())
        .await
        .into_response();
    let json = response_json(response).await;
    let channel = json["channels"]
        .as_array()
        .expect("channels array")
        .iter()
        .find(|channel| channel["name"] == "telegram.default")
        .cloned()
        .expect("telegram channel is listed");
    assert_eq!(channel["readiness"]["authenticated"], "unknown");
    assert!(
        channel["readiness"]["notes"]
            .as_array()
            .expect("notes array")
            .iter()
            .any(|note| {
                note.as_str()
                    .is_some_and(|s| s.contains("not checked for `telegram`"))
            })
    );
}

#[cfg(feature = "channel-wechat")]
#[tokio::test]
async fn api_channel_relink_wechat_clears_persisted_login_then_noops() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = zeroclaw_config::schema::Config::default();
    config.gateway.require_pairing = false;
    config.channels.wechat.insert(
        "admin".to_string(),
        zeroclaw_config::schema::WeChatConfig {
            enabled: true,
            state_dir: Some(temp.path().to_string_lossy().into_owned()),
            ..Default::default()
        },
    );
    std::fs::write(
        temp.path().join("account.json"),
        r#"{"token": "tok_persisted", "account_id": "acct_1"}"#,
    )
    .unwrap();
    std::fs::write(temp.path().join("sync.json"), r#"{"get_updates_buf": "c"}"#).unwrap();

    let response = handle_api_channel_relink(
        State(test_state(config.clone())),
        Path("wechat.admin".to_string()),
        HeaderMap::new(),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    let json = response_json(response).await;
    assert_eq!(json["outcome"], "cleared");
    assert_eq!(json["restart_required"], true);
    assert_eq!(json["removed"].as_array().expect("removed array").len(), 2);
    assert!(!temp.path().join("account.json").exists());
    assert!(!temp.path().join("sync.json").exists());

    // Relinking again is the documented no-op.
    let response = handle_api_channel_relink(
        State(test_state(config)),
        Path("wechat.admin".to_string()),
        HeaderMap::new(),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    let json = response_json(response).await;
    assert_eq!(json["outcome"], "nothing_to_clear");
    assert_eq!(json["restart_required"], false);
}

#[cfg(feature = "whatsapp-web")]
#[tokio::test]
async fn api_channel_relink_whatsapp_web_unpaired_noops_without_touching_disk() {
    let temp = tempfile::tempdir().unwrap();
    let session_path = temp.path().join("session.db");
    let mut config = zeroclaw_config::schema::Config::default();
    config.gateway.require_pairing = false;
    config.channels.whatsapp.insert(
        "admin".to_string(),
        zeroclaw_config::schema::WhatsAppConfig {
            enabled: true,
            session_path: Some(session_path.to_string_lossy().into_owned()),
            ..Default::default()
        },
    );

    let response = handle_api_channel_relink(
        State(test_state(config)),
        Path("whatsapp.admin".to_string()),
        HeaderMap::new(),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    let json = response_json(response).await;
    assert_eq!(json["outcome"], "nothing_to_clear");
    assert!(
        !session_path.exists(),
        "relinking an unpaired channel must not create the session database"
    );
}

#[tokio::test]
async fn api_channel_relink_unsupported_channel_is_explicit_conflict_noop() {
    let config = config_with_telegram("default");

    let response = handle_api_channel_relink(
        State(test_state(config)),
        Path("telegram.default".to_string()),
        HeaderMap::new(),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let json = response_json(response).await;
    assert_eq!(json["outcome"], "unsupported");
    assert!(
        json["error"]
            .as_str()
            .expect("error string")
            .contains("nothing was changed")
    );
}

#[tokio::test]
async fn api_channel_relink_unknown_channel_is_not_found() {
    let response = handle_api_channel_relink(
        State(test_state(zeroclaw_config::schema::Config::default())),
        Path("wechat.ghost".to_string()),
        HeaderMap::new(),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn api_channel_relink_requires_bearer_auth_when_pairing_enabled() {
    let state = AppState {
        pairing: Arc::new(PairingGuard::new(true, &[])),
        ..test_state(config_with_telegram("default"))
    };

    let response = handle_api_channel_relink(
        State(state),
        Path("telegram.default".to_string()),
        HeaderMap::new(),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

fn link_job_to_test_agent(state: &AppState, job_id: &str) {
    state
        .config
        .write()
        .agents
        .get_mut("test-agent")
        .expect("test-agent configured by with_test_agent")
        .cron_jobs
        .push(job_id.to_string());
}

fn config_with_webhook(
    alias: &str,
    enabled: bool,
    bound: bool,
    port: u16,
    listen_path: Option<&str>,
) -> zeroclaw_config::schema::Config {
    let mut config = zeroclaw_config::schema::Config::default();
    config.gateway.port = 42617;
    config.gateway.require_pairing = false;
    config.channels.webhook.insert(
        alias.to_string(),
        zeroclaw_config::schema::WebhookConfig {
            enabled,
            port,
            listen_path: listen_path.map(ToString::to_string),
            ..Default::default()
        },
    );
    if bound {
        config.agents.insert(
            "rowan".to_string(),
            zeroclaw_config::schema::AliasedAgentConfig {
                channels: vec![zeroclaw_config::providers::ChannelRef::new(format!(
                    "webhook.{alias}"
                ))],
                ..Default::default()
            },
        );
    }
    config
}

fn config_with_telegram(alias: &str) -> zeroclaw_config::schema::Config {
    let mut config = zeroclaw_config::schema::Config::default();
    config.channels.telegram.insert(
        alias.to_string(),
        zeroclaw_config::schema::TelegramConfig {
            enabled: true,
            bot_token: "test-token".to_string(),
            ..Default::default()
        },
    );
    config.agents.insert(
        "rowan".to_string(),
        zeroclaw_config::schema::AliasedAgentConfig {
            channels: vec![zeroclaw_config::providers::ChannelRef::new(format!(
                "telegram.{alias}"
            ))],
            ..Default::default()
        },
    );
    config
}

fn first_channel_info(
    config: &zeroclaw_config::schema::Config,
) -> zeroclaw_config::schema::ChannelAliasInfo {
    config
        .channels_by_alias()
        .into_iter()
        .next()
        .expect("channel alias should be present")
}

#[test]
fn channel_readiness_webhook_does_not_call_gateway_route_healthy_without_listener() {
    let config = config_with_webhook("default", true, true, 42617, Some("/webhook"));
    let state = test_state(config.clone());
    let health = zeroclaw_runtime::health::snapshot();
    let info = first_channel_info(&config);
    let readiness = channel_readiness(&config, &info, &health, &state);

    assert_eq!(readiness.authenticated, ChannelReadinessState::Ready);
    assert_eq!(readiness.listening, ChannelReadinessState::Missing);
    assert_eq!(channel_readiness_summary(&readiness), ("error", "down"));
    assert!(
        readiness
            .requirements
            .iter()
            .any(|item| item.contains("Start a channel listener"))
    );
}

#[test]
fn channel_readiness_webhook_does_not_call_custom_path_healthy_without_listener() {
    let config = config_with_webhook("custom_path", true, true, 42632, Some("/eyrie"));
    let state = test_state(config.clone());
    let health = zeroclaw_runtime::health::snapshot();
    let info = first_channel_info(&config);
    let readiness = channel_readiness(&config, &info, &health, &state);

    assert_eq!(readiness.authenticated, ChannelReadinessState::Ready);
    assert_eq!(readiness.listening, ChannelReadinessState::Missing);
    assert_eq!(channel_readiness_summary(&readiness), ("error", "down"));
    assert!(
        readiness
            .requirements
            .iter()
            .any(|item| item.contains("Start a channel listener"))
    );
}

#[test]
fn channel_readiness_webhook_uses_supervised_listener_health_for_custom_path() {
    let config = config_with_webhook("supervised", true, true, 42632, Some("/eyrie"));
    zeroclaw_runtime::health::mark_component_ok("channel:webhook.supervised");
    let state = test_state(config.clone());
    let health = zeroclaw_runtime::health::snapshot();
    let info = first_channel_info(&config);
    let readiness = channel_readiness(&config, &info, &health, &state);

    assert_eq!(readiness.listening, ChannelReadinessState::Ready);
    assert_eq!(channel_readiness_summary(&readiness), ("active", "healthy"));
}

#[test]
fn channel_readiness_webhook_rejects_stale_listener_health() {
    let config = config_with_webhook("stale", true, true, 42632, Some("/eyrie"));
    let component = "channel:webhook.stale".to_string();
    let old = (chrono::Utc::now()
        - chrono::Duration::seconds(CHANNEL_LISTENER_HEALTH_MAX_AGE_SECS + 5))
    .to_rfc3339();
    let health = zeroclaw_runtime::health::HealthSnapshot {
        pid: std::process::id(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        uptime_seconds: 1,
        components: std::collections::BTreeMap::from([(
            component,
            zeroclaw_runtime::health::ComponentHealth {
                status: "ok".to_string(),
                updated_at: old,
                last_ok: None,
                last_error: None,
                restart_count: 0,
            },
        )]),
    };
    let state = test_state(config.clone());
    let info = first_channel_info(&config);
    let readiness = channel_readiness(&config, &info, &health, &state);

    assert_eq!(readiness.listening, ChannelReadinessState::Missing);
    assert_eq!(channel_readiness_summary(&readiness), ("error", "down"));
}

#[test]
fn channel_readiness_webhook_uses_live_pairing_guard_for_auth() {
    let config = config_with_webhook("paired", true, true, 42632, Some("/eyrie"));
    zeroclaw_runtime::health::mark_component_ok("channel:webhook.paired");
    let mut state = test_state(config.clone());
    state.pairing = Arc::new(PairingGuard::new(true, &[]));
    let health = zeroclaw_runtime::health::snapshot();
    let info = first_channel_info(&config);
    let readiness = channel_readiness(&config, &info, &health, &state);

    assert_eq!(readiness.authenticated, ChannelReadinessState::Missing);
    assert_eq!(readiness.listening, ChannelReadinessState::Ready);
    assert_eq!(channel_readiness_summary(&readiness), ("error", "down"));
}

#[test]
fn channel_readiness_unchecked_channel_types_are_unknown_not_down() {
    let config = config_with_telegram("ops");
    let state = test_state(config.clone());
    let health = zeroclaw_runtime::health::snapshot();
    let info = first_channel_info(&config);
    let readiness = channel_readiness(&config, &info, &health, &state);

    assert_eq!(readiness.enabled, ChannelReadinessState::Ready);
    assert_eq!(readiness.bound_to_agent, ChannelReadinessState::Ready);
    assert_eq!(readiness.authenticated, ChannelReadinessState::Unknown);
    assert_eq!(readiness.listening, ChannelReadinessState::Unknown);
    assert_eq!(
        channel_readiness_summary(&readiness),
        ("unknown", "degraded")
    );
    assert!(readiness.requirements.is_empty());
    assert!(
        readiness
            .notes
            .iter()
            .any(|item| item.contains("not checked"))
    );
}

#[test]
fn channel_readiness_orphan_channel_reports_missing_agent_binding_without_broken_health() {
    let config = config_with_webhook("orphan", true, false, 42617, Some("/webhook"));
    let state = test_state(config.clone());
    let health = zeroclaw_runtime::health::snapshot();
    let info = first_channel_info(&config);
    let readiness = channel_readiness(&config, &info, &health, &state);

    assert_eq!(readiness.bound_to_agent, ChannelReadinessState::Missing);
    assert_eq!(readiness.listening, ChannelReadinessState::Unknown);
    assert_eq!(
        channel_readiness_summary(&readiness),
        ("inactive", "degraded")
    );
    assert!(
        readiness
            .requirements
            .iter()
            .any(|item| item.contains("Bind this channel"))
    );
}

#[test]
fn require_auth_rejects_empty_bearer_token() {
    let config = zeroclaw_config::schema::Config::default();
    let mut state = test_state(config);
    state.pairing = Arc::new(PairingGuard::new(true, &[]));

    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        "Bearer ".parse().unwrap(), // empty token after prefix
    );

    let result = require_auth(&state, &headers);
    assert!(result.is_err(), "empty bearer token must be rejected");
    let (status, _) = result.unwrap_err();
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn api_channels_serializes_readiness_without_duplicate_summary_fields() {
    let config = config_with_webhook("ops", true, true, 42617, Some("/webhook"));
    let state = test_state(config);

    let response = handle_api_channels(State(state), HeaderMap::new())
        .await
        .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let json = response_json(response).await;
    let channel = &json["channels"][0];
    let webhook_compiled = zeroclaw_channels::listing::is_channel_type_compiled("webhook");
    assert_eq!(channel["name"], "webhook.ops");
    assert_eq!(channel["compiled"], webhook_compiled);
    if webhook_compiled {
        assert_eq!(channel["status"], "error");
        assert_eq!(channel["health"], "down");
    } else {
        assert_eq!(channel["status"], "not_compiled");
        assert_eq!(channel["health"], "unavailable");
    }
    assert_eq!(channel["readiness"]["enabled"], "ready");
    assert_eq!(channel["readiness"]["authenticated"], "ready");
    assert_eq!(channel["readiness"]["listening"], "missing");
    assert!(channel["readiness"].get("configured").is_none());
    assert!(channel["readiness"].get("status").is_none());
    assert!(channel["readiness"].get("health").is_none());
}

fn test_state_with_session_backend(
    config: zeroclaw_config::schema::Config,
    backend: Arc<dyn SessionBackend>,
) -> AppState {
    let mut state = test_state(config);
    state.session_backend = Some(backend);
    state
}

#[tokio::test]
async fn session_message_post_persists_and_broadcasts_to_session() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("workspace"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let backend: Arc<dyn SessionBackend> = Arc::new(SessionStore::new(tmp.path()).unwrap());
    backend
        .append(
            "gw_operator-1",
            &zeroclaw_providers::ChatMessage::assistant("existing"),
        )
        .unwrap();
    let state = test_state_with_session_backend(config, backend.clone());
    let mut rx = state.event_tx.subscribe();

    let response = handle_api_session_message_post(
        State(state.clone()),
        HeaderMap::new(),
        Path("operator-1".to_string()),
        Json(
            serde_json::from_value::<SessionMessagePostBody>(serde_json::json!({
                "content": "deploy finished"
            }))
            .expect("body should deserialize"),
        ),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let json = response_json(response).await;
    assert_eq!(json["status"], "ok");
    assert_eq!(json["session_id"], "operator-1");
    assert_eq!(json["message"]["role"], "assistant");
    assert_eq!(json["message"]["content"], "deploy finished");
    assert!(json.get("message_count").is_none());

    let messages = backend.load("gw_operator-1");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].role, "assistant");
    assert_eq!(messages[1].content, "deploy finished");

    let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("broadcast event")
        .expect("broadcast value");
    assert_eq!(event["type"], "message");
    assert_eq!(event["session_id"], "operator-1");
    assert_eq!(event["role"], "assistant");
    assert_eq!(event["content"], "deploy finished");

    let history = state.event_buffer.snapshot();
    assert!(
        history.is_empty(),
        "session-scoped chat messages stay out of global event history"
    );
}

#[tokio::test]
async fn session_message_post_rejects_empty_content() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("workspace"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let backend: Arc<dyn SessionBackend> = Arc::new(SessionStore::new(tmp.path()).unwrap());
    let state = test_state_with_session_backend(config, backend);

    let response = handle_api_session_message_post(
        State(state),
        HeaderMap::new(),
        Path("operator-1".to_string()),
        Json(
            serde_json::from_value::<SessionMessagePostBody>(serde_json::json!({
                "content": "   "
            }))
            .expect("body should deserialize"),
        ),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = response_json(response).await;
    assert_eq!(json["error"], "content is required");
}

#[tokio::test]
async fn session_message_post_rejects_unknown_session_without_creating_it() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("workspace"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let backend: Arc<dyn SessionBackend> = Arc::new(SessionStore::new(tmp.path()).unwrap());
    let state = test_state_with_session_backend(config, backend.clone());

    let response = handle_api_session_message_post(
        State(state),
        HeaderMap::new(),
        Path("operator-1".to_string()),
        Json(
            serde_json::from_value::<SessionMessagePostBody>(serde_json::json!({
                "content": "deploy finished"
            }))
            .expect("body should deserialize"),
        ),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let json = response_json(response).await;
    assert_eq!(json["error"], "Session not found");
    assert!(backend.load("gw_operator-1").is_empty());
}

#[tokio::test]
async fn session_message_post_waits_for_session_queue_before_append() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("workspace"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let backend: Arc<dyn SessionBackend> = Arc::new(SessionStore::new(tmp.path()).unwrap());
    backend
        .append(
            "gw_operator-1",
            &zeroclaw_providers::ChatMessage::assistant("existing"),
        )
        .unwrap();
    let state = test_state_with_session_backend(config, backend.clone());
    let session_guard = state.session_queue.acquire("gw_operator-1").await.unwrap();

    let response_fut = handle_api_session_message_post(
        State(state),
        HeaderMap::new(),
        Path("operator-1".to_string()),
        Json(
            serde_json::from_value::<SessionMessagePostBody>(serde_json::json!({
                "content": "queued notification"
            }))
            .expect("body should deserialize"),
        ),
    );
    tokio::pin!(response_fut);

    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut response_fut)
            .await
            .is_err(),
        "POST should wait behind the active session queue guard"
    );
    assert_eq!(backend.load("gw_operator-1").len(), 1);

    drop(session_guard);
    let response = tokio::time::timeout(Duration::from_secs(1), response_fut)
        .await
        .expect("queued POST should complete")
        .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let messages = backend.load("gw_operator-1");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].content, "queued notification");
}

#[tokio::test]
async fn cron_api_shell_roundtrip_includes_delivery() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("data"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let state = test_state(with_test_agent(config));

    let add_response = handle_api_cron_add(
        State(state.clone()),
        HeaderMap::new(),
        Json(
            serde_json::from_value::<CronAddBody>(serde_json::json!({
                "name": "test-job",
                "agent": "test-agent",
                "schedule": "*/5 * * * *",
                "command": "echo hello",
                "delivery": {
                    "mode": "announce",
                    "channel": "discord",
                    "to": "1234567890",
                    "best_effort": true
                }
            }))
            .expect("body should deserialize"),
        ),
    )
    .await
    .into_response();

    let add_json = response_json(add_response).await;
    assert_eq!(add_json["status"], "ok");
    assert_eq!(add_json["job"]["delivery"]["mode"], "announce");
    assert_eq!(add_json["job"]["delivery"]["channel"], "discord");
    assert_eq!(add_json["job"]["delivery"]["to"], "1234567890");

    let list_response = handle_api_cron_list(State(state), HeaderMap::new())
        .await
        .into_response();
    let list_json = response_json(list_response).await;
    let jobs = list_json["jobs"].as_array().expect("jobs array");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0]["delivery"]["mode"], "announce");
    assert_eq!(jobs[0]["delivery"]["channel"], "discord");
    assert_eq!(jobs[0]["delivery"]["to"], "1234567890");
}

#[tokio::test]
async fn cron_api_accepts_agent_jobs() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("data"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let state = test_state(with_test_agent(config));

    let response = handle_api_cron_add(
        State(state.clone()),
        HeaderMap::new(),
        Json(
            serde_json::from_value::<CronAddBody>(serde_json::json!({
                "name": "agent-job",
                "agent": "test-agent",
                "schedule": "*/5 * * * *",
                "job_type": "agent",
                "command": "ignored shell command",
                "prompt": "summarize the latest logs"
            }))
            .expect("body should deserialize"),
        ),
    )
    .await
    .into_response();

    let json = response_json(response).await;
    assert_eq!(json["status"], "ok");

    let config = state.config.read().clone();
    let jobs = zeroclaw_runtime::cron::list_jobs(&config).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].job_type, zeroclaw_runtime::cron::JobType::Agent);
    assert_eq!(jobs[0].prompt.as_deref(), Some("summarize the latest logs"));
}

#[tokio::test]
async fn cron_api_timezone_add_persists_explicit_timezone() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("workspace"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let state = test_state(with_test_agent(config));

    let response = handle_api_cron_add(
        State(state.clone()),
        HeaderMap::new(),
        Json(
            serde_json::from_value::<CronAddBody>(serde_json::json!({
                "agent": "test-agent",
                "name": "localized-job",
                "schedule": "0 9 * * *",
                "tz": "America/New_York",
                "command": "echo hello"
            }))
            .expect("body should deserialize"),
        ),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let config = state.config.read().clone();
    let jobs = zeroclaw_runtime::cron::list_jobs(&config).unwrap();
    assert_eq!(
        jobs[0].schedule,
        zeroclaw_runtime::cron::Schedule::Cron {
            expr: "0 9 * * *".to_string(),
            tz: Some("America/New_York".to_string()),
        }
    );
}

#[tokio::test]
async fn cron_api_timezone_add_rejects_invalid_timezone_as_bad_request() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("workspace"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let state = test_state(with_test_agent(config));

    let response = handle_api_cron_add(
        State(state),
        HeaderMap::new(),
        Json(
            serde_json::from_value::<CronAddBody>(serde_json::json!({
                "agent": "test-agent",
                "name": "invalid-timezone-job",
                "schedule": "0 9 * * *",
                "tz": "Invalid/Zone",
                "command": "echo hello"
            }))
            .expect("body should deserialize"),
        ),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = response_json(response).await;
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("Invalid IANA timezone")
    );
}

#[tokio::test]
async fn cron_api_timezone_patch_schedule_preserves_existing_timezone() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("workspace"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let state = test_state(with_test_agent(config));
    let job = zeroclaw_runtime::cron::add_shell_job_with_approval(
        &state.config.read().clone(),
        "test-agent",
        Some("localized-job".to_string()),
        zeroclaw_runtime::cron::Schedule::Cron {
            expr: "0 9 * * *".to_string(),
            tz: Some("Europe/Berlin".to_string()),
        },
        "echo hello",
        None,
        true,
    )
    .expect("job added");

    let response = handle_api_cron_patch(
        State(state.clone()),
        HeaderMap::new(),
        Path(job.id.clone()),
        Json(
            serde_json::from_value::<CronPatchBody>(serde_json::json!({
                "agent": "test-agent",
                "schedule": "30 9 * * *"
            }))
            .expect("body should deserialize"),
        ),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let updated = zeroclaw_runtime::cron::get_job(&state.config.read().clone(), &job.id)
        .expect("updated job");
    assert_eq!(
        updated.schedule,
        zeroclaw_runtime::cron::Schedule::Cron {
            expr: "30 9 * * *".to_string(),
            tz: Some("Europe/Berlin".to_string()),
        }
    );
}

#[tokio::test]
async fn cron_api_timezone_patch_replaces_timezone_when_provided() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("workspace"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let state = test_state(with_test_agent(config));
    let job = zeroclaw_runtime::cron::add_shell_job_with_approval(
        &state.config.read().clone(),
        "test-agent",
        Some("localized-job".to_string()),
        zeroclaw_runtime::cron::Schedule::Cron {
            expr: "0 9 * * *".to_string(),
            tz: Some("America/New_York".to_string()),
        },
        "echo hello",
        None,
        true,
    )
    .expect("job added");

    let response = handle_api_cron_patch(
        State(state.clone()),
        HeaderMap::new(),
        Path(job.id.clone()),
        Json(
            serde_json::from_value::<CronPatchBody>(serde_json::json!({
                "agent": "test-agent",
                "schedule": "30 9 * * *",
                "tz": "Asia/Tokyo"
            }))
            .expect("body should deserialize"),
        ),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let updated = zeroclaw_runtime::cron::get_job(&state.config.read().clone(), &job.id)
        .expect("updated job");
    assert_eq!(
        updated.schedule,
        zeroclaw_runtime::cron::Schedule::Cron {
            expr: "30 9 * * *".to_string(),
            tz: Some("Asia/Tokyo".to_string()),
        }
    );
}

#[tokio::test]
async fn cron_api_timezone_patch_sets_timezone_without_schedule_change() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("workspace"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let state = test_state(with_test_agent(config));
    let job = zeroclaw_runtime::cron::add_shell_job_with_approval(
        &state.config.read().clone(),
        "test-agent",
        Some("runtime-local-job".to_string()),
        zeroclaw_runtime::cron::Schedule::Cron {
            expr: "0 9 * * *".to_string(),
            tz: None,
        },
        "echo hello",
        None,
        true,
    )
    .expect("job added");

    let response = handle_api_cron_patch(
        State(state.clone()),
        HeaderMap::new(),
        Path(job.id.clone()),
        Json(
            serde_json::from_value::<CronPatchBody>(serde_json::json!({
                "agent": "test-agent",
                "tz": "America/Chicago"
            }))
            .expect("body should deserialize"),
        ),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let updated = zeroclaw_runtime::cron::get_job(&state.config.read().clone(), &job.id)
        .expect("updated job");
    assert_eq!(
        updated.schedule,
        zeroclaw_runtime::cron::Schedule::Cron {
            expr: "0 9 * * *".to_string(),
            tz: Some("America/Chicago".to_string()),
        }
    );
}

#[tokio::test]
async fn cron_api_timezone_patch_rejects_invalid_timezone_as_bad_request() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("workspace"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let state = test_state(with_test_agent(config));
    let job = zeroclaw_runtime::cron::add_shell_job_with_approval(
        &state.config.read().clone(),
        "test-agent",
        Some("localized-job".to_string()),
        zeroclaw_runtime::cron::Schedule::Cron {
            expr: "0 9 * * *".to_string(),
            tz: Some("America/New_York".to_string()),
        },
        "echo hello",
        None,
        true,
    )
    .expect("job added");

    let response = handle_api_cron_patch(
        State(state),
        HeaderMap::new(),
        Path(job.id),
        Json(
            serde_json::from_value::<CronPatchBody>(serde_json::json!({
                "agent": "test-agent",
                "tz": "Invalid/Zone"
            }))
            .expect("body should deserialize"),
        ),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = response_json(response).await;
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("Invalid IANA timezone")
    );
}

#[tokio::test]
async fn cron_api_timezone_patch_clears_timezone_with_explicit_signal() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("workspace"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let state = test_state(with_test_agent(config));
    let job = zeroclaw_runtime::cron::add_shell_job_with_approval(
        &state.config.read().clone(),
        "test-agent",
        Some("localized-job".to_string()),
        zeroclaw_runtime::cron::Schedule::Cron {
            expr: "0 9 * * *".to_string(),
            tz: Some("America/New_York".to_string()),
        },
        "echo hello",
        None,
        true,
    )
    .expect("job added");

    let response = handle_api_cron_patch(
        State(state.clone()),
        HeaderMap::new(),
        Path(job.id.clone()),
        Json(
            serde_json::from_value::<CronPatchBody>(serde_json::json!({
                "agent": "test-agent",
                "clear_tz": true
            }))
            .expect("body should deserialize"),
        ),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let updated = zeroclaw_runtime::cron::get_job(&state.config.read().clone(), &job.id)
        .expect("updated job");
    assert_eq!(
        updated.schedule,
        zeroclaw_runtime::cron::Schedule::Cron {
            expr: "0 9 * * *".to_string(),
            tz: None,
        }
    );
}

#[tokio::test]
async fn cron_api_patch_enabled_without_agent() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("data"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let state = test_state(with_test_agent(config));
    let job = zeroclaw_runtime::cron::add_shell_job_with_approval(
        &state.config.read().clone(),
        "test-agent",
        Some("toggle-job".to_string()),
        zeroclaw_runtime::cron::Schedule::Cron {
            expr: "*/5 * * * *".to_string(),
            tz: None,
        },
        "echo hello",
        None,
        true,
    )
    .expect("job added");

    // No `agent` field at all — pause/resume must not require one.
    let response = handle_api_cron_patch(
        State(state.clone()),
        HeaderMap::new(),
        Path(job.id.clone()),
        Json(
            serde_json::from_value::<CronPatchBody>(serde_json::json!({ "enabled": false }))
                .expect("body should deserialize"),
        ),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "enable/disable toggle must not require an agent"
    );
    let updated = zeroclaw_runtime::cron::get_job(&state.config.read().clone(), &job.id)
        .expect("updated job");
    assert!(!updated.enabled, "job should be disabled after the patch");
}

#[tokio::test]
async fn cron_api_patch_name_and_schedule_without_agent() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("data"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let state = test_state(with_test_agent(config));
    let job = zeroclaw_runtime::cron::add_shell_job_with_approval(
        &state.config.read().clone(),
        "test-agent",
        Some("old-name".to_string()),
        zeroclaw_runtime::cron::Schedule::Cron {
            expr: "*/5 * * * *".to_string(),
            tz: None,
        },
        "echo hello",
        None,
        true,
    )
    .expect("job added");

    // Metadata-only patch (no command/prompt) — agent must be optional.
    let response = handle_api_cron_patch(
        State(state.clone()),
        HeaderMap::new(),
        Path(job.id.clone()),
        Json(
            serde_json::from_value::<CronPatchBody>(serde_json::json!({
                "name": "new-name",
                "schedule": "30 9 * * *"
            }))
            .expect("body should deserialize"),
        ),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "name/schedule patch must not require an agent"
    );
    let updated = zeroclaw_runtime::cron::get_job(&state.config.read().clone(), &job.id)
        .expect("updated job");
    assert_eq!(updated.name.as_deref(), Some("new-name"));
    assert_eq!(
        updated.schedule,
        zeroclaw_runtime::cron::Schedule::Cron {
            expr: "30 9 * * *".to_string(),
            tz: None,
        }
    );
}

#[tokio::test]
async fn cron_api_patch_shell_command_requires_known_agent() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("data"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let state = test_state(with_test_agent(config));
    let job = zeroclaw_runtime::cron::add_shell_job_with_approval(
        &state.config.read().clone(),
        "test-agent",
        Some("shell-job".to_string()),
        zeroclaw_runtime::cron::Schedule::Cron {
            expr: "*/5 * * * *".to_string(),
            tz: None,
        },
        "echo hello",
        None,
        true,
    )
    .expect("job added");

    // Setting a shell `command` still hits the risk gate: a missing agent
    // must be a clean 400, not a fall-through.
    let response = handle_api_cron_patch(
        State(state.clone()),
        HeaderMap::new(),
        Path(job.id.clone()),
        Json(
            serde_json::from_value::<CronPatchBody>(serde_json::json!({ "command": "echo bye" }))
                .expect("body should deserialize"),
        ),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "shell command patch with no agent must be rejected at the risk gate"
    );
    let json = response_json(response).await;
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("Unknown agent"),
        "error should name the unknown agent"
    );
}

#[tokio::test]
async fn cron_api_patch_shell_prompt_unknown_agent_is_bad_request_not_500() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("data"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let state = test_state(with_test_agent(config));
    let job = zeroclaw_runtime::cron::add_shell_job_with_approval(
        &state.config.read().clone(),
        "test-agent",
        Some("shell-job".to_string()),
        zeroclaw_runtime::cron::Schedule::Cron {
            expr: "*/5 * * * *".to_string(),
            tz: None,
        },
        "echo hello",
        None,
        true,
    )
    .expect("job added");

    // For a shell job a new command can arrive via `prompt`; it still routes
    // through the command-risk gate, so an unknown agent is a 400 — not the
    // 500 that an unguarded path would surface.
    let response = handle_api_cron_patch(
        State(state.clone()),
        HeaderMap::new(),
        Path(job.id.clone()),
        Json(
            serde_json::from_value::<CronPatchBody>(serde_json::json!({
                "agent": "ghost",
                "prompt": "echo bye"
            }))
            .expect("body should deserialize"),
        ),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "shell-job prompt with unknown agent must be 400, not 500"
    );
}

#[tokio::test]
async fn cron_api_patch_agent_prompt_without_agent() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("data"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let state = test_state(with_test_agent(config));

    let add_response = handle_api_cron_add(
        State(state.clone()),
        HeaderMap::new(),
        Json(
            serde_json::from_value::<CronAddBody>(serde_json::json!({
                "name": "agent-job",
                "agent": "test-agent",
                "schedule": "*/5 * * * *",
                "job_type": "agent",
                "command": "ignored",
                "prompt": "old prompt"
            }))
            .expect("body should deserialize"),
        ),
    )
    .await
    .into_response();
    assert_eq!(add_response.status(), StatusCode::OK);
    let id = zeroclaw_runtime::cron::list_jobs(&state.config.read().clone()).unwrap()[0]
        .id
        .clone();

    // For an agent-type job `prompt` is an LLM prompt, not a shell command,
    // so it is not agent-gated and may omit `agent`.
    let response = handle_api_cron_patch(
        State(state.clone()),
        HeaderMap::new(),
        Path(id.clone()),
        Json(
            serde_json::from_value::<CronPatchBody>(serde_json::json!({ "prompt": "new prompt" }))
                .expect("body should deserialize"),
        ),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "agent-type prompt patch must not require an agent"
    );
    let updated =
        zeroclaw_runtime::cron::get_job(&state.config.read().clone(), &id).expect("updated job");
    assert_eq!(updated.prompt.as_deref(), Some("new prompt"));
}

#[tokio::test]
async fn cron_api_rejects_announce_delivery_without_target() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("data"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let state = test_state(with_test_agent(config));

    let response = handle_api_cron_add(
        State(state.clone()),
        HeaderMap::new(),
        Json(
            serde_json::from_value::<CronAddBody>(serde_json::json!({
                "name": "invalid-delivery-job",
                "agent": "test-agent",
                "schedule": "*/5 * * * *",
                "command": "echo hello",
                "delivery": {
                    "mode": "announce",
                    "channel": "discord"
                }
            }))
            .expect("body should deserialize"),
        ),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = response_json(response).await;
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("delivery.to is required")
    );

    let config = state.config.read().clone();
    assert!(
        zeroclaw_runtime::cron::list_jobs(&config)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn cron_api_run_executes_shell_job_and_records_run() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("data"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let state = test_state(with_test_agent(config));

    let job = zeroclaw_runtime::cron::add_shell_job_with_approval(
        &state.config.read().clone(),
        "test-agent",
        None,
        zeroclaw_runtime::cron::Schedule::Cron {
            expr: "*/5 * * * *".to_string(),
            tz: None,
        },
        "echo hello-from-manual-trigger",
        None,
        true,
    )
    .expect("job added");

    // Imperative jobs get UUID ids; the scheduler resolves owning
    // agent by reverse-lookup against `agent.cron_jobs`.
    link_job_to_test_agent(&state, &job.id);

    let response =
        handle_api_cron_run(State(state.clone()), HeaderMap::new(), Path(job.id.clone()))
            .await
            .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let json = response_json(response).await;
    assert_eq!(json["status"], "ok");
    assert_eq!(json["success"], true);
    assert_eq!(json["job_id"], job.id);
    assert!(
        json["output"]
            .as_str()
            .unwrap_or_default()
            .contains("hello-from-manual-trigger")
    );

    let runs = zeroclaw_runtime::cron::list_runs(&state.config.read().clone(), &job.id, 10)
        .expect("runs listed");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, "ok");
}

#[tokio::test]
async fn cron_api_run_records_best_effort_delivery_failure_as_degraded() {
    zeroclaw_runtime::cron::scheduler::register_delivery_fn(Box::new(
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
    let config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("data"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let state = test_state(with_test_agent(config));

    let job = zeroclaw_runtime::cron::add_shell_job_with_approval(
        &state.config.read().clone(),
        "test-agent",
        None,
        zeroclaw_runtime::cron::Schedule::Cron {
            expr: "*/5 * * * *".to_string(),
            tz: None,
        },
        "echo hello-from-manual-trigger",
        Some(zeroclaw_runtime::cron::DeliveryConfig {
            mode: "announce".into(),
            channel: Some("fail-delivery".into()),
            to: Some("123456".into()),
            thread_id: None,
            best_effort: true,
        }),
        true,
    )
    .expect("job added");
    link_job_to_test_agent(&state, &job.id);

    let response =
        handle_api_cron_run(State(state.clone()), HeaderMap::new(), Path(job.id.clone()))
            .await
            .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let json = response_json(response).await;
    assert_eq!(json["status"], "degraded");
    assert_eq!(json["success"], true);
    assert!(
        json["output"]
            .as_str()
            .unwrap_or_default()
            .contains("delivery failed:")
    );

    let config = state.config.read().clone();
    let updated = zeroclaw_runtime::cron::get_job(&config, &job.id).expect("updated job");
    assert_eq!(updated.last_status.as_deref(), Some("degraded"));
    assert!(
        updated
            .last_output
            .as_deref()
            .unwrap_or_default()
            .contains("delivery failed:")
    );

    let runs = zeroclaw_runtime::cron::list_runs(&config, &job.id, 10).expect("runs listed");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, "degraded");
    assert!(
        runs[0]
            .output
            .as_deref()
            .unwrap_or_default()
            .contains("delivery failed:")
    );
}

#[tokio::test]
async fn cron_api_run_returns_not_found_for_unknown_job() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: tmp.path().join("data"),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let state = test_state(with_test_agent(config));

    let response = handle_api_cron_run(
        State(state),
        HeaderMap::new(),
        Path("does-not-exist".to_string()),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

use crate::api_pairing::{
    DeviceInfo, DeviceRegistry, revoke_device, rotate_token as rotate_device_token,
    submit_pairing_enhanced,
};
use chrono::Utc;

async fn paired_state_with_device(tmp: &tempfile::TempDir) -> (AppState, String, String) {
    let data_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&data_dir).unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: data_dir.clone(),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };

    let pairing = Arc::new(PairingGuard::new(true, &[]));
    let code = pairing.pairing_code().unwrap();
    let token = pairing.try_pair(&code, "test").await.unwrap().unwrap();
    let token_hash = PairingGuard::token_hash(&token);

    let registry = Arc::new(DeviceRegistry::new(&data_dir));
    let device_id = "dev-1".to_string();
    registry
        .register(
            token_hash,
            DeviceInfo {
                id: device_id.clone(),
                name: None,
                device_type: None,
                paired_at: Utc::now(),
                last_seen: Utc::now(),
                ip_address: None,
                capabilities: None,
            },
        )
        .expect("test device registry insert");

    let mut state = test_state(config);
    state.pairing = pairing;
    state.device_registry = Some(registry);
    (state, token, device_id)
}

fn bearer_headers(token: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        header::AUTHORIZATION,
        format!("Bearer {token}").parse().unwrap(),
    );
    h
}

#[tokio::test]
async fn reconcile_backfills_orphan_token_hashes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let registry = DeviceRegistry::new(tmp.path());

    // A real, already-registered device with a name.
    let known_hash = "a".repeat(64);
    registry
        .register(
            known_hash.clone(),
            DeviceInfo {
                id: "known".into(),
                name: Some("My Laptop".into()),
                device_type: Some("desktop".into()),
                paired_at: Utc::now(),
                last_seen: Utc::now(),
                ip_address: None,
                capabilities: None,
            },
        )
        .expect("test device registry insert");

    let orphan_a = "b".repeat(64);
    let orphan_b = "c".repeat(64);
    let inserted = registry
        .reconcile_from_token_hashes(&[known_hash.clone(), orphan_a.clone(), orphan_b.clone()])
        .unwrap();
    assert_eq!(inserted, 2, "only the two orphan hashes should be inserted");
    assert_eq!(registry.device_count(), 3);

    // Existing metadata is preserved, not clobbered.
    let known = registry
        .list()
        .expect("test device registry list")
        .into_iter()
        .find(|d| d.id == "known")
        .expect("known device still present");
    assert_eq!(known.name.as_deref(), Some("My Laptop"));

    // Re-running is a no-op (idempotent).
    let again = registry
        .reconcile_from_token_hashes(&[known_hash, orphan_a, orphan_b])
        .unwrap();
    assert_eq!(again, 0);
    assert_eq!(registry.device_count(), 3);
}

#[tokio::test]
async fn backfilled_orphan_is_revocable_by_its_real_hash() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&data_dir).unwrap();

    let pairing = PairingGuard::new(true, &[]);
    let code = pairing.pairing_code().unwrap();
    let token = pairing
        .try_pair(&code, "legacy-client")
        .await
        .unwrap()
        .unwrap();
    assert!(pairing.is_authenticated(&token));

    // Simulate the `/pair` orphan: token is paired but never registered.
    let registry = DeviceRegistry::new(&data_dir);
    assert_eq!(registry.device_count(), 0);

    let inserted = registry
        .reconcile_from_token_hashes(&pairing.tokens())
        .unwrap();
    assert_eq!(inserted, 1);

    // The backfilled row is keyed by the auth hash, so revoke returns it and
    // revoking that hash from the guard actually de-authenticates the token.
    let device = registry
        .list()
        .expect("test device registry list")
        .into_iter()
        .next()
        .expect("one backfilled device");
    let revoked_hash = registry
        .revoke(&device.id)
        .unwrap()
        .expect("device existed");
    assert_eq!(revoked_hash, PairingGuard::token_hash(&token));
    assert!(pairing.revoke_token_hash(&revoked_hash));
    assert!(
        !pairing.is_authenticated(&token),
        "token must not authenticate after revoke"
    );
}

#[tokio::test]
async fn rotate_token_invalidates_old_bearer_token() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (state, old_token, device_id) = paired_state_with_device(&tmp).await;
    assert!(state.pairing.is_authenticated(&old_token));

    let response = rotate_device_token(
        State(state.clone()),
        bearer_headers(&old_token),
        Path(device_id.clone()),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);

    assert!(
        !state.pairing.is_authenticated(&old_token),
        "old bearer token must not authenticate after rotate"
    );

    let json = response_json(response).await;
    assert_eq!(json["device_id"], device_id);
    assert!(json["pairing_code"].is_string());
}

#[tokio::test]
async fn rotate_token_persists_revocation_to_config() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (state, old_token, device_id) = paired_state_with_device(&tmp).await;
    let old_hash = PairingGuard::token_hash(&old_token);

    let response = rotate_device_token(
        State(state.clone()),
        bearer_headers(&old_token),
        Path(device_id),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);

    let persisted = state.config.read().gateway.paired_tokens.clone();
    assert!(
        !persisted.contains(&old_hash),
        "revoked token hash must not remain in gateway.paired_tokens"
    );
}

#[tokio::test]
async fn submit_pairing_enhanced_persists_new_token() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (state, _old_token, _device_id) = paired_state_with_device(&tmp).await;

    let code = state
        .pairing
        .generate_new_pairing_code()
        .expect("require_pairing was enabled");

    let response = submit_pairing_enhanced(
        State(state.clone()),
        HeaderMap::new(),
        Json(serde_json::json!({ "code": code, "device_name": "repaired" })),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);

    let json = response_json(response).await;
    assert_eq!(json["persisted"], true);
    let new_token = json["token"].as_str().expect("token in response");
    let new_hash = PairingGuard::token_hash(new_token);
    assert!(
        state
            .config
            .read()
            .gateway
            .paired_tokens
            .contains(&new_hash),
        "newly paired token hash must be persisted to gateway.paired_tokens"
    );
}

#[tokio::test]
async fn revoke_device_invalidates_bearer_token() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (state, old_token, device_id) = paired_state_with_device(&tmp).await;

    let response = revoke_device(
        State(state.clone()),
        bearer_headers(&old_token),
        Path(device_id),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);

    assert!(
        !state.pairing.is_authenticated(&old_token),
        "bearer token must not authenticate after device delete"
    );
    let old_hash = PairingGuard::token_hash(&old_token);
    assert!(
        !state
            .config
            .read()
            .gateway
            .paired_tokens
            .contains(&old_hash),
        "deleted device's token must be dropped from persisted paired_tokens"
    );
}

#[tokio::test]
async fn rotate_unknown_device_returns_not_found() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (state, token, _) = paired_state_with_device(&tmp).await;

    let response = rotate_device_token(
        State(state.clone()),
        bearer_headers(&token),
        Path("does-not-exist".into()),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(
        state.pairing.is_authenticated(&token),
        "unknown-device rotate must not touch existing tokens"
    );
}

#[tokio::test]
async fn rotate_with_pending_code_revokes_but_returns_null_code() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (state, token, device_id) = paired_state_with_device(&tmp).await;

    let pending_code = state
        .pairing
        .generate_new_pairing_code()
        .expect("require_pairing was enabled");

    let response = rotate_device_token(
        State(state.clone()),
        bearer_headers(&token),
        Path(device_id.clone()),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);

    assert!(
        !state.pairing.is_authenticated(&token),
        "old bearer token must be revoked even when a pairing code is pending"
    );
    assert_eq!(
        state.pairing.pairing_code().as_deref(),
        Some(pending_code.as_str()),
        "pending pairing code must survive rotate",
    );

    let json = response_json(response).await;
    assert!(json["pairing_code"].is_null());
    assert_eq!(json["device_id"], device_id);
}

#[tokio::test]
async fn concurrent_rotates_do_not_both_issue_a_pairing_code() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&data_dir).unwrap();
    let config = zeroclaw_config::schema::Config {
        data_dir: data_dir.clone(),
        config_path: tmp.path().join("config.toml"),
        ..zeroclaw_config::schema::Config::default()
    };

    let pairing = Arc::new(PairingGuard::new(true, &[]));
    let code = pairing.pairing_code().unwrap();
    let admin_token = pairing.try_pair(&code, "admin").await.unwrap().unwrap();

    let registry = Arc::new(DeviceRegistry::new(&data_dir));
    for id in ["dev-a", "dev-b"] {
        // Each device needs its own paired token so revoke has a hash.
        let code = pairing
            .generate_new_pairing_code()
            .expect("pairing enabled");
        let tok = pairing.try_pair(&code, id).await.unwrap().unwrap();
        registry
            .register(
                PairingGuard::token_hash(&tok),
                DeviceInfo {
                    id: id.to_string(),
                    name: None,
                    device_type: None,
                    paired_at: Utc::now(),
                    last_seen: Utc::now(),
                    ip_address: None,
                    capabilities: None,
                },
            )
            .expect("test device registry insert");
    }

    let mut state = test_state(config);
    state.pairing = pairing;
    state.device_registry = Some(registry);

    let s1 = state.clone();
    let s2 = state.clone();
    let h1 = bearer_headers(&admin_token);
    let h2 = bearer_headers(&admin_token);
    let (r1, r2) = tokio::join!(
        async move {
            rotate_device_token(State(s1), h1, Path("dev-a".into()))
                .await
                .into_response()
        },
        async move {
            rotate_device_token(State(s2), h2, Path("dev-b".into()))
                .await
                .into_response()
        },
    );

    assert_eq!(r1.status(), StatusCode::OK);
    assert_eq!(r2.status(), StatusCode::OK);
    let j1 = response_json(r1).await;
    let j2 = response_json(r2).await;
    let codes_issued =
        usize::from(j1["pairing_code"].is_string()) + usize::from(j2["pairing_code"].is_string());
    assert_eq!(
        codes_issued, 1,
        "exactly one of two racing rotates must win the pairing slot, \
             got {codes_issued} (j1={j1}, j2={j2})"
    );
}

#[cfg(feature = "a2a")]
mod a2a_auth {
    use super::*;
    use tower::ServiceExt;

    const TOKEN: &str = "a2a-test-token";

    fn paired_state() -> AppState {
        let mut config = zeroclaw_config::schema::Config::default();
        config.a2a.server.enabled = true;
        let agent = zeroclaw_config::schema::AliasedAgentConfig {
            a2a: zeroclaw_config::multi_agent::AgentA2aConfig {
                published: true,
                exposed_skills: Vec::new(),
            },
            ..Default::default()
        };
        config.agents.insert("maker".to_string(), agent);
        let mut state = test_state(config);
        state.pairing = Arc::new(PairingGuard::new(true, &[TOKEN.to_string()]));
        state
    }

    async fn status_of(
        router: axum::Router,
        req: axum::http::Request<axum::body::Body>,
    ) -> StatusCode {
        router.oneshot(req).await.expect("router response").status()
    }

    #[tokio::test]
    async fn task_endpoint_rejects_unauthenticated_request() {
        let router = crate::a2a::a2a_task_route().with_state(paired_state());
        let req = axum::http::Request::builder()
                .method("POST")
                .uri("/a2a/maker")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"message/send","params":{"message":{"parts":[{"kind":"text","text":"hi"}]}}}"#,
                ))
                .unwrap();
        assert_eq!(status_of(router, req).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn catalog_card_serves_unauthenticated_request() {
        let router = crate::a2a::a2a_routes().with_state(paired_state());
        let req = axum::http::Request::builder()
            .method("GET")
            .uri("/.well-known/agents-card.json")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(status_of(router, req).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn alias_card_serves_unauthenticated_request() {
        let router = crate::a2a::a2a_routes().with_state(paired_state());
        let req = axum::http::Request::builder()
            .method("GET")
            .uri("/a2a/maker/.well-known/agent-card.json")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(status_of(router, req).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn alias_card_serves_with_valid_token() {
        let router = crate::a2a::a2a_routes().with_state(paired_state());
        let req = axum::http::Request::builder()
            .method("GET")
            .uri("/a2a/maker/.well-known/agent-card.json")
            .header("authorization", format!("Bearer {TOKEN}"))
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(status_of(router, req).await, StatusCode::OK);
    }
}

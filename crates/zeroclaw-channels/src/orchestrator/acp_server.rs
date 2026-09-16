//! ACP (Agent Control Protocol) Server — JSON-RPC 2.0 over stdio.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;
use zeroclaw_api::elicitation::ElicitationCapabilities;
pub use zeroclaw_api::jsonrpc::RpcOutbound;
use zeroclaw_api::jsonrpc::error_codes::*;
use zeroclaw_api::jsonrpc::{
    ACP_PROTOCOL_VERSION, JsonRpcError, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
};
use zeroclaw_api::model_provider::ConversationMessage;
use zeroclaw_api::plan::PlanEntry;
use zeroclaw_config::schema::Config;
use zeroclaw_infra::acp_session_store::AcpSessionStore;
use zeroclaw_runtime::agent::agent::{Agent, TurnEvent};
use zeroclaw_runtime::tools::CanvasStore;

use crate::acp_channel::AcpChannel;

// ── Configuration ────────────────────────────────────────────────

/// ACP server configuration (optional `[acp]` section in config.toml).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AcpServerConfig {
    /// Maximum number of concurrent sessions. Default: 10.
    pub max_sessions: usize,
    /// Session inactivity timeout in seconds. Default: 3600 (1 hour).
    pub session_timeout_secs: u64,
}

impl Default for AcpServerConfig {
    fn default() -> Self {
        Self {
            max_sessions: 10,
            session_timeout_secs: 3600,
        }
    }
}

// ── Session state ────────────────────────────────────────────────

struct Session {
    agent: Agent,
    #[allow(dead_code)] // WIP: intended for session expiry logic
    created_at: Instant,
    last_active: Instant,
    /// Agent alias (e.g. `"clamps"`) for attributable span logs.
    agent_alias: String,
    /// Model-provider ref (e.g. `"anthropic.default"`) for attributable span logs.
    model_provider: String,
    /// Model identifier (e.g. `"claude-sonnet-4-6"`) for attributable span logs.
    model: String,
}

// ── ACP Server ───────────────────────────────────────────────────

enum ConfigSource {
    Standalone(Box<Config>),
    Live(Arc<parking_lot::RwLock<Config>>),
}

pub struct AcpServer {
    /// The sole authority for `Config`-backed settings. Standalone ACP owns an
    /// immutable config; gateway ACP resolves the shared daemon config.
    config_source: ConfigSource,
    acp_config: AcpServerConfig,
    sessions: Arc<Mutex<HashMap<String, Arc<Mutex<Session>>>>>,
    rpc: Arc<RpcOutbound>,
    /// Receiver for the writer task. Pulled out (replaced with `None`) the
    /// first time `run()` starts the writer loop.
    writer_rx: std::sync::Mutex<Option<mpsc::Receiver<String>>>,
    cancel_tokens: Arc<std::sync::Mutex<HashMap<String, tokio_util::sync::CancellationToken>>>,
    /// Tracks session IDs currently being loaded/resumed (between the initial
    /// check and the final insert into `sessions`). Used to prevent duplicate
    /// concurrent restores of the same session and to count in-flight slots
    /// against `max_sessions`.
    loading_sessions: Arc<tokio::sync::Mutex<HashSet<String>>>,
    store: Option<Arc<AcpSessionStore>>,
    /// Shared canvas store from the gateway / daemon supervisor.  When set,
    /// agents created by this server write canvas frames to the same store
    /// that `/ws/canvas/:id` WebSocket subscribers read from.  `None` in
    /// standalone `zeroclaw acp` mode where no gateway is running.
    canvas_store: Option<CanvasStore>,
    /// Connection-scoped default agent alias (`?agent=` on the gateway ACP
    /// endpoint). Slots into the `session/new` alias precedence chain between
    /// an explicit `agentAlias` and `[acp].default_agent`. Not a config
    /// change: it only supplies the default for new sessions on this
    /// connection (restore keeps the operator-controlled fallback chain).
    connection_default_agent: Option<String>,
    client_elicitation_caps: std::sync::RwLock<ElicitationCapabilities>,
}

impl AcpServer {
    pub fn new(config: Config, acp_config: AcpServerConfig) -> Self {
        let (writer_tx, writer_rx) = mpsc::channel::<String>(256);
        Self::with_writer(
            ConfigSource::Standalone(Box::new(config)),
            acp_config,
            writer_tx,
            Some(writer_rx),
            None,
        )
    }

    pub fn new_with_writer(
        config: Config,
        acp_config: AcpServerConfig,
        writer_tx: mpsc::Sender<String>,
    ) -> Self {
        Self::with_writer(
            ConfigSource::Standalone(Box::new(config)),
            acp_config,
            writer_tx,
            None,
            None,
        )
    }

    pub fn new_with_store(
        config: Config,
        acp_config: AcpServerConfig,
        store: Arc<AcpSessionStore>,
    ) -> Self {
        let (writer_tx, writer_rx) = mpsc::channel::<String>(256);
        Self::with_writer(
            ConfigSource::Standalone(Box::new(config)),
            acp_config,
            writer_tx,
            Some(writer_rx),
            Some(store),
        )
    }

    pub fn new_with_writer_and_store(
        config: Config,
        acp_config: AcpServerConfig,
        writer_tx: mpsc::Sender<String>,
        store: Arc<AcpSessionStore>,
    ) -> Self {
        Self::with_writer(
            ConfigSource::Standalone(Box::new(config)),
            acp_config,
            writer_tx,
            None,
            Some(store),
        )
    }

    /// Create a gateway-backed ACP server without durable session storage.
    ///
    /// The server retains no parallel `Config` clone and resolves an on-demand
    /// view whenever it handles a request.
    pub fn new_with_live_config_and_writer(
        live_config: Arc<parking_lot::RwLock<Config>>,
        acp_config: AcpServerConfig,
        writer_tx: mpsc::Sender<String>,
    ) -> Self {
        Self::with_writer(
            ConfigSource::Live(live_config),
            acp_config,
            writer_tx,
            None,
            None,
        )
    }

    /// Create a gateway-backed ACP server with durable session storage.
    ///
    /// The server retains no parallel `Config` clone and resolves an on-demand
    /// view whenever it handles a request.
    pub fn new_with_live_config_and_writer_and_store(
        live_config: Arc<parking_lot::RwLock<Config>>,
        acp_config: AcpServerConfig,
        writer_tx: mpsc::Sender<String>,
        store: Arc<AcpSessionStore>,
    ) -> Self {
        Self::with_writer(
            ConfigSource::Live(live_config),
            acp_config,
            writer_tx,
            None,
            Some(store),
        )
    }

    fn with_writer(
        config_source: ConfigSource,
        acp_config: AcpServerConfig,
        writer_tx: mpsc::Sender<String>,
        writer_rx: Option<mpsc::Receiver<String>>,
        store: Option<Arc<AcpSessionStore>>,
    ) -> Self {
        Self {
            config_source,
            acp_config,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            rpc: Arc::new(RpcOutbound::new(writer_tx)),
            writer_rx: std::sync::Mutex::new(writer_rx),
            cancel_tokens: Arc::new(std::sync::Mutex::new(HashMap::new())),
            loading_sessions: Arc::new(tokio::sync::Mutex::new(HashSet::new())),
            store,
            canvas_store: None,
            connection_default_agent: None,
            client_elicitation_caps: std::sync::RwLock::new(ElicitationCapabilities::default()),
        }
    }

    fn config_snapshot(&self) -> Config {
        match &self.config_source {
            ConfigSource::Standalone(config) => config.as_ref().clone(),
            ConfigSource::Live(config) => config.read().clone(),
        }
    }

    async fn build_agent(
        &self,
        config: &Config,
        agent_alias: &str,
        workspace_dir: &std::path::Path,
        enable_mcp: bool,
    ) -> Result<Agent> {
        if let ConfigSource::Live(live_config) = &self.config_source {
            Agent::from_live_config_with_session_cwd_and_mcp_backchannel(
                Arc::clone(live_config),
                agent_alias,
                Some(workspace_dir),
                enable_mcp,
                true,
                self.canvas_store.clone(),
            )
            .await
        } else {
            Agent::from_config_with_session_cwd_and_mcp_backchannel(
                config,
                agent_alias,
                Some(workspace_dir),
                enable_mcp,
                true,
                self.canvas_store.clone(),
            )
            .await
        }
    }

    /// Set the connection-scoped default agent alias (`?agent=` query param
    /// on the gateway ACP endpoint). Blank values are treated as absent. The
    /// alias is validated at `session/new` with the same dispatchable-agent
    /// checks as an explicit `agentAlias`. Restore paths ignore this value.
    pub fn with_connection_default_agent(mut self, alias: Option<String>) -> Self {
        self.connection_default_agent = alias
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        self
    }

    /// Attach the shared gateway [`CanvasStore`] so that agents created by
    /// this server write canvas frames to the same store that the
    /// `/ws/canvas/:id` WebSocket endpoint serves.
    pub fn with_canvas_store(mut self, canvas_store: CanvasStore) -> Self {
        self.canvas_store = Some(canvas_store);
        self
    }

    /// Run the ACP server, reading JSON-RPC requests from stdin and writing
    /// responses/notifications to stdout.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_category(::zeroclaw_log::EventCategory::Channel),
            &format!(
                "ACP server starting (max_sessions={}, timeout={}s)",
                self.acp_config.max_sessions, self.acp_config.session_timeout_secs
            )
        );

        // Pull the writer-rx out of self so we can move it into the writer
        // task. Subsequent `run()` calls would have nothing to drive — but
        // `run()` is normally invoked once per process.
        let writer_rx = self
            .writer_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_category(::zeroclaw_log::EventCategory::Channel)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                    "ACP server writer already started"
                );
                anyhow::Error::msg("ACP server writer already started")
            })?;
        zeroclaw_spawn::spawn!(writer_task(writer_rx));

        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut line = String::new();

        // Spawn session reaper
        let sessions = Arc::clone(&self.sessions);
        let timeout = Duration::from_secs(self.acp_config.session_timeout_secs);
        zeroclaw_spawn::spawn!(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                let mut sessions = sessions.lock().await;
                let before = sessions.len();
                sessions.retain(|id, session_arc| {
                    // Never reap a session whose inner lock is held — it has an
                    // active prompt turn in flight and is by definition not idle.
                    match session_arc.try_lock() {
                        Ok(session) => {
                            let expired = session.last_active.elapsed() > timeout;
                            if expired {
                                ::zeroclaw_log::record!(
                                    DEBUG,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Note
                                    )
                                    .with_category(::zeroclaw_log::EventCategory::Channel)
                                    .with_attrs(
                                        ::serde_json::json!({
                                            "id": id,
                                            "agent_alias": session.agent_alias,
                                            "model_provider": session.model_provider,
                                            "model": session.model,
                                        })
                                    ),
                                    "Session expired after inactivity"
                                );
                            }
                            !expired
                        }
                        Err(_) => true,
                    }
                });
                let reaped = before - sessions.len();
                if reaped > 0 {
                    ::zeroclaw_log::record!(
                        DEBUG,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_category(::zeroclaw_log::EventCategory::Channel)
                            .with_attrs(::serde_json::json!({"reaped": reaped})),
                        "Reaped expired session(s)"
                    );
                }
            }
        });

        loop {
            line.clear();
            let bytes_read = reader.read_line(&mut line).await?;
            if bytes_read == 0 {
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_category(::zeroclaw_log::EventCategory::Channel),
                    "ACP server: stdin closed, shutting down"
                );
                break;
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            self.process_line(trimmed).await;
        }

        Ok(())
    }

    pub async fn run_messages(self: Arc<Self>, mut input_rx: mpsc::Receiver<String>) -> Result<()> {
        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_category(::zeroclaw_log::EventCategory::Channel),
            "ACP server starting (WebSocket/framed mode)"
        );
        while let Some(line) = input_rx.recv().await {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            self.process_line(trimmed).await;
        }

        Ok(())
    }

    async fn process_line(self: &Arc<Self>, trimmed: &str) {
        // First, peek at whether this is a response (has `result` or
        // `error`) to a request *we* sent. Inbound requests/notifications
        // fall through to the JsonRpcRequest path.
        if let Ok(value) = serde_json::from_str::<Value>(trimmed)
            && value.is_object()
            && (value.get("result").is_some() || value.get("error").is_some())
            && let Some(id) = value.get("id")
        {
            let id_str = id
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| id.to_string());
            let result = value.get("result").cloned();
            let error: Option<JsonRpcError> = value
                .get("error")
                .and_then(|e| serde_json::from_value(e.clone()).ok());
            self.rpc.dispatch_response(&id_str, result, error);
            return;
        }

        match serde_json::from_str::<JsonRpcRequest>(trimmed) {
            Ok(request) => {
                if request.jsonrpc != "2.0" {
                    if let Some(id) = request.id {
                        self.write_error(id, INVALID_REQUEST, "Invalid JSON-RPC version")
                            .await;
                    }
                    return;
                }
                let server = Arc::clone(self);
                ::zeroclaw_spawn::spawn!(async move {
                    server.handle_request(request).await;
                });
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_category(::zeroclaw_log::EventCategory::Channel)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "Failed to parse JSON-RPC request"
                );
                self.write_error(Value::Null, PARSE_ERROR, &format!("Parse error: {e}"))
                    .await;
            }
        }
    }

    async fn handle_request(&self, request: JsonRpcRequest) {
        let id = request.id.clone().unwrap_or(Value::Null);
        let is_notification = request.id.is_none();

        let result = match request.method.as_str() {
            "initialize" => self.handle_initialize(&request.params),
            "session/new" => self.handle_session_new(&request.params).await,
            "session/load" => self.handle_session_load(&request.params).await,
            "session/resume" => self.handle_session_resume(&request.params).await,
            "session/close" => self.handle_session_close(&request.params).await,
            "session/prompt" => self.handle_session_prompt(&request.params, &id).await,
            "session/stop" => self.handle_session_stop(&request.params).await,
            "session/cancel" => self.handle_session_cancel(&request.params).await,
            "session/event" | "session/update" => self.handle_session_event(&request.params).await,
            _ => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_category(::zeroclaw_log::EventCategory::Channel)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"method": request.method})),
                    "ACP method not found"
                );
                Err(RpcError {
                    code: METHOD_NOT_FOUND,
                    message: format!("Method not found: {}", request.method),
                    data: None,
                })
            }
        };

        // Only send response for requests (with id), not notifications
        if !is_notification {
            match result {
                Ok(value) => self.write_result(id, value).await,
                Err(e) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                            .with_category(::zeroclaw_log::EventCategory::Channel)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "method": request.method,
                                "error_code": e.code,
                                "error": e.message,
                            })),
                        "ACP request failed"
                    );
                    self.write_error(id, e.code, &e.message).await;
                }
            }
        }
    }

    // ── Method handlers ──────────────────────────────────────────

    fn handle_initialize(&self, params: &Value) -> RpcResult {
        let elicitation = params
            .get("clientCapabilities")
            .and_then(|c| c.get("elicitation"));
        *self.client_elicitation_caps.write().unwrap() =
            ElicitationCapabilities::from_value(elicitation);

        let config = self.config_snapshot();
        let default_model = config
            .providers
            .models
            .iter_entries()
            .find_map(|(_, _, e)| e.model.clone());

        let mut zeroclaw_meta = serde_json::json!({
            "maxSessions": self.acp_config.max_sessions,
            "sessionTimeoutSecs": self.acp_config.session_timeout_secs,
        });
        if let Some(model) = default_model {
            zeroclaw_meta["defaultModel"] = serde_json::json!(model);
        }

        let session_capabilities = if self.store.is_some() {
            serde_json::json!({ "resume": {}, "close": {} })
        } else {
            serde_json::json!({})
        };

        Ok(serde_json::json!({
            "protocolVersion": ACP_PROTOCOL_VERSION,
            "agentCapabilities": {
                "loadSession": self.store.is_some(),
                "promptCapabilities": {
                    "image": false,
                    "audio": false,
                    "embeddedContext": false,
                },
                "mcpCapabilities": {
                    "http": false,
                    "sse": false,
                },
                "sessionCapabilities": session_capabilities,
            },
            "agentInfo": {
                "name": "zeroclaw-acp",
                "title": "ZeroClaw ACP",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "authMethods": [],
            "_meta": {
                "zeroclaw": zeroclaw_meta,
            }
        }))
    }

    /// True when `alias` names a configured agent that can dispatch a turn.
    fn alias_if_dispatchable(config: &Config, alias: &str) -> Option<String> {
        if alias.trim().is_empty() {
            return None;
        }
        config
            .agent_is_dispatchable(alias)
            .then(|| alias.to_string())
    }

    /// Shared validation for explicit `agentAlias`, `?agent=`, config defaults,
    /// and sole-agent auto-select.
    fn validate_dispatchable_agent_alias(
        config: &Config,
        agent_alias: &str,
    ) -> Result<(), RpcError> {
        match config.agent(agent_alias) {
            None => Err(RpcError {
                code: INVALID_PARAMS,
                message: format!(
                    "Unknown agent `{agent_alias}` — no [agents.{agent_alias}] entry configured"
                ),
                data: None,
            }),
            Some(_) if !config.agent_is_dispatchable(agent_alias) => Err(RpcError {
                code: INVALID_PARAMS,
                message: format!("Agent `{agent_alias}` is not enabled for dispatch"),
                data: None,
            }),
            Some(_) => Ok(()),
        }
    }

    /// Restore alias precedence: persisted owner (when still dispatchable) →
    /// `[acp].default_agent` → sole configured agent → `"default"`.
    ///
    /// The connection-scoped `?agent=` default is intentionally omitted: restore
    /// accepts only a session ID and must not let transport input rebind a
    /// persisted workspace/history to a different agent.
    fn resolve_restore_agent_alias(config: &Config, persisted_agent_alias: &str) -> String {
        Self::alias_if_dispatchable(config, persisted_agent_alias)
            .or_else(|| {
                config
                    .acp
                    .default_agent
                    .as_ref()
                    .and_then(|alias| Self::alias_if_dispatchable(config, alias))
            })
            .or_else(|| {
                if config.agents.len() == 1 {
                    config
                        .agents
                        .keys()
                        .next()
                        .and_then(|alias| Self::alias_if_dispatchable(config, alias))
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "default".to_string())
    }

    async fn handle_session_new(&self, params: &Value) -> RpcResult {
        let config = self.config_snapshot();
        let requested_cwd = self.requested_session_cwd(params, &config);

        let workspace_dir = std::fs::canonicalize(&requested_cwd)
            .map_err(|e| RpcError {
                code: INVALID_PARAMS,
                message: format!(
                    "cwd is not a usable directory ({}): {e}",
                    requested_cwd.display()
                ),
                data: None,
            })?
            .to_string_lossy()
            .into_owned();

        // Every ACP session is bound to an explicit agent alias.
        // Accept `agentAlias` (camelCase) or `agent_alias` / `agent`,
        // then the connection-scoped default (`?agent=`), then
        // `[acp].default_agent`. When all are absent and exactly one agent
        // is configured, auto-select it so single-agent setups work without
        // extra config.
        let agent_alias = params
            .get("agentAlias")
            .or_else(|| params.get("agent_alias"))
            .or_else(|| params.get("agent"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| self.connection_default_agent.clone())
            .or_else(|| config.acp.default_agent.clone())
            .or_else(|| {
                let mut keys = config.agents.keys();
                if config.agents.len() == 1 {
                    keys.next().cloned()
                } else {
                    None
                }
            })
            .ok_or_else(|| RpcError {
                code: INVALID_PARAMS,
                message: "session/new requires `agentAlias` (alias of a configured \
                          [agents.<alias>] entry)"
                    .to_string(),
                data: None,
            })?;
        Self::validate_dispatchable_agent_alias(&config, &agent_alias)?;

        let session_id = Uuid::new_v4().to_string();

        {
            let sessions = self.sessions.lock().await;
            let mut loading = self.loading_sessions.lock().await;
            if sessions.len() + loading.len() >= self.acp_config.max_sessions {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_category(::zeroclaw_log::EventCategory::Channel)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "active": sessions.len(),
                            "loading": loading.len(),
                            "max": self.acp_config.max_sessions,
                        })),
                    "ACP session/new rejected: session limit reached"
                );
                return Err(RpcError {
                    code: SESSION_LIMIT_REACHED,
                    message: format!(
                        "Maximum session limit reached ({})",
                        self.acp_config.max_sessions
                    ),
                    data: None,
                });
            }
            loading.insert(session_id.clone());
        }

        // Build agent from global config, with the session's cwd pinned as
        // the file/shell sandbox boundary. The agent's data directory
        // (identity, scheduled tasks) still lives under `config.data_dir`.
        // ACP sessions exclude persistent memory — context comes from the
        // persisted session history, not the agent's long-term memory store.
        // MCP init is opt-in per agent (`[agents.<alias>].acp_enable_mcp`): off
        // by default to keep `session/new` prompt; on to load this agent's
        // `mcp_bundles` tools. Runs without the sessions lock held (see above).
        let enable_mcp = config.agent(&agent_alias).is_some_and(|a| a.acp_enable_mcp);
        let mut agent = match self
            .build_agent(
                &config,
                &agent_alias,
                std::path::Path::new(&workspace_dir),
                enable_mcp,
            )
            .await
        {
            Ok(agent) => agent,
            Err(e) => {
                self.loading_sessions.lock().await.remove(&session_id);
                let model_provider = config
                    .agent(&agent_alias)
                    .map(|a| a.model_provider.to_string())
                    .unwrap_or_default();
                let model = config
                    .model_provider_for_agent(&agent_alias)
                    .and_then(|mp| mp.model.clone())
                    .unwrap_or_default();
                let error = zeroclaw_runtime::security::scrub(
                    &zeroclaw_providers::sanitize_api_error(&e.to_string()),
                );
                ::zeroclaw_log::scope!(
                    session_key: session_id.as_str(),
                    agent_alias: agent_alias.as_str(),
                    model_provider: model_provider.as_str(),
                    model: model.as_str(),
                    channel: "acp",
                    => async {
                        ::zeroclaw_log::record!(
                            ERROR,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Fail,
                            )
                            .with_category(::zeroclaw_log::EventCategory::Channel)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "workspace_dir": workspace_dir,
                                "error": error.as_str(),
                            })),
                            "ACP session/new failed: agent init error"
                        );
                    }
                )
                .await;
                return Err(RpcError {
                    code: INTERNAL_ERROR,
                    message: format!("Failed to create agent: {error}"),
                    data: None,
                });
            }
        };

        // Wire an ACP back-channel so tools like `ask_user`,
        // `escalate_to_human`, and `reaction` can talk to the IDE/CLI client
        // for this session. Registered as `"acp"`; channel_name must match so
        // interactive tools default here instead of an arbitrary map entry.
        let acp_channel = Arc::new(AcpChannel::new(
            "acp",
            session_id.clone(),
            Arc::clone(&self.rpc),
            Duration::from_secs(self.acp_config.session_timeout_secs),
            *self.client_elicitation_caps.read().unwrap(),
        ));
        agent.set_channel_name("acp".to_string());
        agent.channel_handles().register_channel("acp", acp_channel);

        // Persist before publishing the session, so a failed write never
        // leaves a live-but-unpersisted session; release the reservation on
        // failure. The slot stays accounted for (still in `loading`) until the
        // insert below.
        if let Some(store) = &self.store {
            let store = store.clone();
            let sid = session_id.clone();
            let alias = agent_alias.clone();
            let wsd = workspace_dir.clone();
            let created =
                tokio::task::spawn_blocking(move || store.create_session(&sid, &alias, &wsd)).await;
            let error = match created {
                Ok(Ok(_)) => None,
                Ok(Err(e)) => Some(e.to_string()),
                Err(join) => Some(join.to_string()),
            };
            if let Some(detail) = error {
                self.loading_sessions.lock().await.remove(&session_id);
                return Err(RpcError {
                    code: INTERNAL_ERROR,
                    message: format!("Failed to persist session: {detail}"),
                    data: None,
                });
            }
        }

        let now = Instant::now();
        // Atomically insert and release the reservation.
        {
            let mut sessions = self.sessions.lock().await;
            let mut loading = self.loading_sessions.lock().await;
            loading.remove(&session_id);
            sessions.insert(
                session_id.clone(),
                Arc::new(Mutex::new(Session {
                    agent,
                    created_at: now,
                    last_active: now,
                    agent_alias: agent_alias.clone(),
                    model_provider: config
                        .agent(&agent_alias)
                        .map(|a| a.model_provider.to_string())
                        .unwrap_or_default(),
                    model: config
                        .model_provider_for_agent(&agent_alias)
                        .and_then(|mp| mp.model.clone())
                        .unwrap_or_default(),
                })),
            );
        }

        let mp = config
            .agent(&agent_alias)
            .map(|a| a.model_provider.to_string())
            .unwrap_or_default();
        let model_name = config
            .model_provider_for_agent(&agent_alias)
            .and_then(|mp| mp.model.clone())
            .unwrap_or_default();
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Start)
                .with_category(::zeroclaw_log::EventCategory::Channel)
                .with_outcome(::zeroclaw_log::EventOutcome::Success)
                .with_attrs(::serde_json::json!({
                    "session_id": session_id,
                    "workspace_dir": workspace_dir,
                    "agent_alias": agent_alias,
                    "model_provider": mp,
                    "model": model_name,
                })),
            "ACP session created"
        );

        Ok(serde_json::json!({
            "sessionId": session_id,
            "workspaceDir": workspace_dir,
        }))
    }

    async fn handle_session_load(&self, params: &Value) -> RpcResult {
        let session_id = params
            .get("sessionId")
            .or_else(|| params.get("session_id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| RpcError {
                code: INVALID_PARAMS,
                message: "Missing required parameter: sessionId".to_string(),
                data: None,
            })?
            .to_string();

        let store = self.store.as_ref().ok_or_else(|| RpcError {
            code: SESSION_NOT_FOUND,
            message: format!("Session not found: {session_id}"),
            data: None,
        })?;

        // Atomically check and reserve the session slot
        {
            let sessions = self.sessions.lock().await;
            let mut loading = self.loading_sessions.lock().await;
            if sessions.len() + loading.len() >= self.acp_config.max_sessions {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_category(::zeroclaw_log::EventCategory::Channel)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "session_id": session_id,
                            "active": sessions.len(),
                            "loading": loading.len(),
                            "max": self.acp_config.max_sessions,
                        })),
                    "ACP session/load rejected: session limit reached"
                );
                return Err(RpcError {
                    code: SESSION_LIMIT_REACHED,
                    message: format!(
                        "Maximum session limit reached ({})",
                        self.acp_config.max_sessions
                    ),
                    data: None,
                });
            }
            if sessions.contains_key(&session_id) || loading.contains(&session_id) {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_category(::zeroclaw_log::EventCategory::Channel)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"session_id": session_id})),
                    "ACP session/load rejected: session already active"
                );
                return Err(RpcError {
                    code: INVALID_PARAMS,
                    message: format!(
                        "Session already active: {session_id}. Call session/close first."
                    ),
                    data: None,
                });
            }
            loading.insert(session_id.clone());
        }

        // Flatten both the SQLite error and the not-found case into a single
        // Result so the cleanup match below runs for every failure after the
        // reservation was inserted.
        let data = store
            .load_session(&session_id)
            .map_err(|e| RpcError {
                code: INTERNAL_ERROR,
                message: format!("Failed to load session: {e}"),
                data: None,
            })
            .and_then(|opt| {
                opt.ok_or_else(|| RpcError {
                    code: SESSION_NOT_FOUND,
                    message: format!("Session not found: {session_id}"),
                    data: None,
                })
            });

        // On error (SQLite failure or not-found), release the reservation.
        let data = match data {
            Ok(d) => d,
            Err(e) => {
                self.loading_sessions.lock().await.remove(&session_id);
                return Err(e);
            }
        };

        let workspace_dir = std::path::PathBuf::from(&data.workspace_dir);
        let config = self.config_snapshot();

        // Restore the agent the session was created with — its alias is
        // persisted on the session row. Fall back to the operator-controlled
        // ACP default (or sole agent, or "default") only when the persisted
        // owner is missing or not dispatchable. `?agent=` is not consulted.
        let restore_alias = Self::resolve_restore_agent_alias(&config, &data.agent_alias);

        // MCP init follows the restored agent's own opt-in
        // (`[agents.<alias>].acp_enable_mcp`), matching `session/new`.
        let enable_mcp = config
            .agent(&restore_alias)
            .is_some_and(|a| a.acp_enable_mcp);
        let agent_result = self
            .build_agent(&config, &restore_alias, &workspace_dir, enable_mcp)
            .await
            .map_err(|e| RpcError {
                code: INTERNAL_ERROR,
                message: format!("Failed to create agent: {e}"),
                data: None,
            });

        let mut agent = match agent_result {
            Ok(a) => a,
            Err(e) => {
                self.loading_sessions.lock().await.remove(&session_id);
                return Err(e);
            }
        };

        let stored_messages: Vec<_> = data
            .messages
            .into_iter()
            .filter(|message| {
                !matches!(message, ConversationMessage::Chat(chat) if chat.role == "system")
            })
            .collect();
        let restore_trim_event =
            agent.seed_conversation_history_with_event(stored_messages.clone());
        let dropped_messages = match &restore_trim_event {
            Some(TurnEvent::HistoryTrimmed {
                dropped_messages, ..
            }) => *dropped_messages,
            _ => 0,
        };
        let restored_messages = stored_messages.into_iter().skip(dropped_messages);

        let acp_channel = Arc::new(AcpChannel::new(
            "acp",
            session_id.clone(),
            Arc::clone(&self.rpc),
            Duration::from_secs(self.acp_config.session_timeout_secs),
            *self.client_elicitation_caps.read().unwrap(),
        ));
        agent.set_channel_name("acp".to_string());
        agent.channel_handles().register_channel("acp", acp_channel);

        let now = Instant::now();
        // Atomically insert and release reservation
        {
            let mut sessions = self.sessions.lock().await;
            let mut loading = self.loading_sessions.lock().await;
            loading.remove(&session_id);
            sessions.insert(
                session_id.clone(),
                Arc::new(Mutex::new(Session {
                    agent,
                    created_at: now,
                    last_active: now,
                    agent_alias: restore_alias.clone(),
                    model_provider: config
                        .agent(&restore_alias)
                        .map(|a| a.model_provider.to_string())
                        .unwrap_or_default(),
                    model: config
                        .model_provider_for_agent(&restore_alias)
                        .and_then(|mp| mp.model.clone())
                        .unwrap_or_default(),
                })),
            );
        }

        if let Some(event) = restore_trim_event
            && let Some(notification) = notification_for_turn_event(&session_id, &event)
        {
            self.write_notification(&notification).await;
        }

        // Replay exactly the history retained by the agent. Replaying the
        // stored pre-trim rows would make the client display context that the
        // restored agent has already discarded.
        let mut replayed_messages = 0;
        for msg in restored_messages {
            replayed_messages += 1;
            for notification in history_notifications_for_message(&session_id, &msg) {
                self.write_notification(&notification).await;
            }
        }

        let mp = config
            .agent(&restore_alias)
            .map(|a| a.model_provider.to_string())
            .unwrap_or_default();
        let model_name = config
            .model_provider_for_agent(&restore_alias)
            .and_then(|mp| mp.model.clone())
            .unwrap_or_default();
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Start)
                .with_category(::zeroclaw_log::EventCategory::Channel)
                .with_outcome(::zeroclaw_log::EventOutcome::Success)
                .with_attrs(::serde_json::json!({
                    "session_id": session_id,
                    "message_count": replayed_messages,
                    "agent_alias": restore_alias,
                    "model_provider": mp,
                    "model": model_name,
                })),
            "ACP session loaded"
        );
        Ok(serde_json::json!({}))
    }

    async fn handle_session_resume(&self, params: &Value) -> RpcResult {
        let session_id = params
            .get("sessionId")
            .or_else(|| params.get("session_id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| RpcError {
                code: INVALID_PARAMS,
                message: "Missing required parameter: sessionId".to_string(),
                data: None,
            })?
            .to_string();

        let store = self.store.as_ref().ok_or_else(|| RpcError {
            code: SESSION_NOT_FOUND,
            message: format!("Session not found: {session_id}"),
            data: None,
        })?;

        // Atomically check and reserve the session slot
        {
            let sessions = self.sessions.lock().await;
            let mut loading = self.loading_sessions.lock().await;
            if sessions.len() + loading.len() >= self.acp_config.max_sessions {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_category(::zeroclaw_log::EventCategory::Channel)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "session_id": session_id,
                            "active": sessions.len(),
                            "loading": loading.len(),
                            "max": self.acp_config.max_sessions,
                        })),
                    "ACP session/resume rejected: session limit reached"
                );
                return Err(RpcError {
                    code: SESSION_LIMIT_REACHED,
                    message: format!(
                        "Maximum session limit reached ({})",
                        self.acp_config.max_sessions
                    ),
                    data: None,
                });
            }
            if sessions.contains_key(&session_id) || loading.contains(&session_id) {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_category(::zeroclaw_log::EventCategory::Channel)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"session_id": session_id})),
                    "ACP session/resume rejected: session already active"
                );
                return Err(RpcError {
                    code: INVALID_PARAMS,
                    message: format!(
                        "Session already active: {session_id}. Call session/close first."
                    ),
                    data: None,
                });
            }
            loading.insert(session_id.clone());
        }

        let data = store
            .load_session(&session_id)
            .map_err(|e| RpcError {
                code: INTERNAL_ERROR,
                message: format!("Failed to load session: {e}"),
                data: None,
            })
            .and_then(|opt| {
                opt.ok_or_else(|| RpcError {
                    code: SESSION_NOT_FOUND,
                    message: format!("Session not found: {session_id}"),
                    data: None,
                })
            });

        // On error (SQLite failure or not-found), release the reservation.
        let data = match data {
            Ok(d) => d,
            Err(e) => {
                self.loading_sessions.lock().await.remove(&session_id);
                return Err(e);
            }
        };

        let workspace_dir = std::path::PathBuf::from(&data.workspace_dir);
        let config = self.config_snapshot();

        // Restore the agent the session was created with — its alias is
        // persisted on the session row. Fall back to the operator-controlled
        // ACP default (or sole agent, or "default") only when the persisted
        // owner is missing or not dispatchable. `?agent=` is not consulted.
        let restore_alias = Self::resolve_restore_agent_alias(&config, &data.agent_alias);

        // MCP init follows the restored agent's own opt-in
        // (`[agents.<alias>].acp_enable_mcp`), matching `session/new`.
        let enable_mcp = config
            .agent(&restore_alias)
            .is_some_and(|a| a.acp_enable_mcp);
        let agent_result = self
            .build_agent(&config, &restore_alias, &workspace_dir, enable_mcp)
            .await
            .map_err(|e| RpcError {
                code: INTERNAL_ERROR,
                message: format!("Failed to create agent: {e}"),
                data: None,
            });

        let mut agent = match agent_result {
            Ok(a) => a,
            Err(e) => {
                self.loading_sessions.lock().await.remove(&session_id);
                return Err(e);
            }
        };

        let restore_trim_event = agent.seed_conversation_history_with_event(data.messages);

        let acp_channel = Arc::new(AcpChannel::new(
            "acp",
            session_id.clone(),
            Arc::clone(&self.rpc),
            Duration::from_secs(self.acp_config.session_timeout_secs),
            *self.client_elicitation_caps.read().unwrap(),
        ));
        agent.set_channel_name("acp".to_string());
        agent.channel_handles().register_channel("acp", acp_channel);

        let now = Instant::now();
        // Atomically insert and release reservation
        {
            let mut sessions = self.sessions.lock().await;
            let mut loading = self.loading_sessions.lock().await;
            loading.remove(&session_id);
            sessions.insert(
                session_id.clone(),
                Arc::new(Mutex::new(Session {
                    agent,
                    created_at: now,
                    last_active: now,
                    agent_alias: restore_alias.clone(),
                    model_provider: config
                        .agent(&restore_alias)
                        .map(|a| a.model_provider.to_string())
                        .unwrap_or_default(),
                    model: config
                        .model_provider_for_agent(&restore_alias)
                        .and_then(|mp| mp.model.clone())
                        .unwrap_or_default(),
                })),
            );
        }

        if let Some(event) = restore_trim_event
            && let Some(notification) = notification_for_turn_event(&session_id, &event)
        {
            self.write_notification(&notification).await;
        }

        let mp = config
            .agent(&restore_alias)
            .map(|a| a.model_provider.to_string())
            .unwrap_or_default();
        let model_name = config
            .model_provider_for_agent(&restore_alias)
            .and_then(|mp| mp.model.clone())
            .unwrap_or_default();

        // Replay the durable TodoWrite plan so the resuming client's tracker
        // repopulates without a model round-trip — parity with the daemon RPC
        // ACP bridge. Best-effort: a load failure or empty plan emits nothing.
        if let Some(store) = self.store.as_ref() {
            let entries = store.get_plan(&session_id).unwrap_or_default();
            if !entries.is_empty()
                && let Some(notification) =
                    notification_for_turn_event(&session_id, &TurnEvent::Plan { entries })
            {
                self.write_notification(&notification).await;
            }
        }

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Start)
                .with_category(::zeroclaw_log::EventCategory::Channel)
                .with_outcome(::zeroclaw_log::EventOutcome::Success)
                .with_attrs(::serde_json::json!({
                    "session_id": session_id,
                    "agent_alias": restore_alias,
                    "model_provider": mp,
                    "model": model_name,
                })),
            "ACP session resumed"
        );
        Ok(serde_json::json!({}))
    }
    async fn handle_session_close(&self, params: &Value) -> RpcResult {
        let session_id = params
            .get("sessionId")
            .or_else(|| params.get("session_id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| RpcError {
                code: INVALID_PARAMS,
                message: "Missing required parameter: sessionId".to_string(),
                data: None,
            })?;

        // Fire the cancel token for any in-flight turn before acquiring the session lock.
        let token = self
            .cancel_tokens
            .lock()
            .expect("cancel_tokens lock poisoned — invariant: all guarded critical sections are short, infallible HashMap ops")
            .get(session_id)
            .cloned();
        if let Some(token) = token {
            token.cancel();
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_category(::zeroclaw_log::EventCategory::Channel)
                    .with_attrs(::serde_json::json!({"session_id": session_id})),
                "ACP session/close: cancelled active turn"
            );
        }

        let session_arc = {
            let mut sessions = self.sessions.lock().await;
            sessions.remove(session_id).ok_or_else(|| RpcError {
                code: SESSION_NOT_FOUND,
                message: format!("Session not found: {session_id}"),
                data: None,
            })?
        };

        // Wait for any in-flight turn to finish (the cancel token may have already stopped it).
        let session = session_arc.lock().await;
        let agent_alias = session.agent_alias.clone();
        let model_provider = session.model_provider.clone();
        let model = session.model.clone();
        session.agent.channel_handles().unregister_channel("acp");
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Complete)
                .with_category(::zeroclaw_log::EventCategory::Channel)
                .with_outcome(::zeroclaw_log::EventOutcome::Success)
                .with_attrs(::serde_json::json!({
                    "session_id": session_id,
                    "agent_alias": agent_alias,
                    "model_provider": model_provider,
                    "model": model,
                })),
            "ACP session closed"
        );

        Ok(serde_json::json!({}))
    }

    fn requested_session_cwd(&self, params: &Value, config: &Config) -> PathBuf {
        params
            .get("cwd")
            .or_else(|| params.get("workspaceDir"))
            .or_else(|| params.get("workspace_dir"))
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| config.data_dir.clone()))
    }

    async fn handle_session_prompt(&self, params: &Value, _request_id: &Value) -> RpcResult {
        let session_id = params
            .get("sessionId")
            .or_else(|| params.get("session_id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| RpcError {
                code: INVALID_PARAMS,
                message: "Missing required parameter: sessionId".to_string(),
                data: None,
            })?
            .to_string();

        let prompt = Self::parse_prompt(params)?;

        // Clone the Arc so the session stays visible in the map throughout the
        // turn. `session/stop` and the reaper can still find it; they will
        // block on the inner Mutex until the turn completes.
        let session_arc = {
            let sessions = self.sessions.lock().await;
            sessions.get(&session_id).cloned().ok_or_else(|| RpcError {
                code: SESSION_NOT_FOUND,
                message: format!("Session not found: {session_id}"),
                data: None,
            })?
        };

        // Snapshot attribution fields before releasing the outer lock.
        let (agent_alias, model_provider, model) = {
            // Try-lock: if the inner lock is held by an active turn, we'll
            // reject below via register_cancel_token anyway. Use a brief
            // non-blocking peek so we can log the alias even on the error path.
            if let Ok(s) = session_arc.try_lock() {
                (
                    s.agent_alias.clone(),
                    s.model_provider.clone(),
                    s.model.clone(),
                )
            } else {
                (String::new(), String::new(), String::new())
            }
        };

        let session_id_s = session_id.clone();
        let agent_alias_s = agent_alias.clone();
        let model_provider_s = model_provider.clone();
        let model_s = model.clone();
        ::zeroclaw_log::scope!(
            agent_alias: agent_alias_s.as_str(),
            model_provider: model_provider_s.as_str(),
            model: model_s.as_str(),
            session_key: session_id_s.as_str(),
            channel: "acp",
        => async move {

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Start).with_category(::zeroclaw_log::EventCategory::Channel)
                .with_attrs(::serde_json::json!({
                    "prompt_len": prompt.len(),
                })),
            "ACP session/prompt turn starting"
        );

        let cancel_token = tokio_util::sync::CancellationToken::new();
        self.register_cancel_token(&session_id, cancel_token.clone())?;
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<TurnEvent>(100);

        let config = self.config_snapshot();
        let cost_tracker = zeroclaw_runtime::cost::CostTracker::get_or_init_global(
            config.cost.clone(),
            &config.data_dir,
        );
        let cost_pricing = std::sync::Arc::new(
            zeroclaw_runtime::agent::cost::build_model_provider_pricing(&config),
        );

        // Move the Arc into the spawned task and lock inside it.  The inner
        // Mutex stays locked for the duration of the turn, preventing
        // concurrent stop/reap from touching the agent mid-turn. The outer
        // map entry remains in place.
        let session_id_for_task = session_id.clone();
        let turn_handle = zeroclaw_spawn::spawn!(async move {
            let mut session = session_arc.lock().await;
            let (turn_alias, turn_provider, turn_model) = session.agent.attribution_fields();
            // Stamp the resolved per-turn alias so `/api/cost?agent=<alias>`
            // attributes this spend.
            let cost_context = cost_tracker.map(|tracker| {
                zeroclaw_runtime::agent::cost::ToolLoopCostTrackingContext::new(
                    tracker,
                    cost_pricing,
                )
                .with_agent_alias(&turn_alias)
            });
            let span_session = session_id_for_task.clone();
            let result = {
                use ::zeroclaw_log::Instrument as _;
                let span = ::zeroclaw_log::info_span!(
                    target: "zeroclaw_log_internal_scope",
                    "zeroclaw_scope",
                    session_key = %span_session,
                    agent_alias = %turn_alias,
                    model_provider = %turn_provider,
                    model = %turn_model,
                    channel = "acp",
                );
                zeroclaw_runtime::agent::loop_::scope_session_key(
                    Some(session_id_for_task),
                    zeroclaw_runtime::agent::cost::TOOL_LOOP_COST_TRACKING_CONTEXT.scope(
                        cost_context,
                        session
                            .agent
                            .turn_streamed(&prompt, event_tx, Some(cancel_token))
                            .instrument(span),
                    ),
                )
                .await
            };
            session.last_active = Instant::now();
            result
            // guard drops here, releasing the inner lock
        });

        let mut accumulated_text = String::new();
        let mut tool_call_count: u32 = 0;
        // Latest whole-list plan emitted this turn (TodoWrite). Persisted
        // durably after the turn so external ACP clients get the same
        // resume/restart replay the daemon RPC bridge already provides.
        let mut latest_plan: Option<Vec<PlanEntry>> = None;
        while let Some(event) = event_rx.recv().await {
            if let TurnEvent::Usage { input_tokens, .. } = &event {
                if let (Some(store), Some(it)) = (&self.store, input_tokens) {
                    let store = store.clone();
                    let sid = session_id.clone();
                    let it = *it;
                    zeroclaw_spawn::spawn!(async move {
                        let persisted =
                            tokio::task::spawn_blocking(move || store.set_token_count(&sid, it))
                                .await;
                        let error = match persisted {
                            Ok(Ok(())) => return,
                            Ok(Err(e)) => e.to_string(),
                            Err(join) => join.to_string(),
                        };
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Write,
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "input_tokens": it,
                                "error": error,
                            })),
                            "Failed to persist ACP session token_count"
                        );
                    });
                }
                continue;
            }
            // Emit attributable span logs for every tool call and result.
            // Attribution (agent_alias, model_provider, session_key) flows
            // from the enclosing spans — not repeated here in attrs.
            match &event {
                TurnEvent::ToolCall { id, name, args } => {
                    tool_call_count += 1;
                    ::zeroclaw_log::record!(
                        DEBUG,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Start).with_category(::zeroclaw_log::EventCategory::Channel)
                            .with_attrs(::serde_json::json!({
                                "tool_call_id": id,
                                "tool": name,
                                "args_len": args.to_string().len(),
                            })),
                        "ACP tool call dispatched"
                    );
                }
                TurnEvent::ToolResult { id, name, output } => {
                    ::zeroclaw_log::record!(
                        DEBUG,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Complete).with_category(::zeroclaw_log::EventCategory::Channel)
                            .with_outcome(::zeroclaw_log::EventOutcome::Success)
                            .with_attrs(::serde_json::json!({
                                "tool_call_id": id,
                                "tool": name,
                                "output_len": output.len(),
                            })),
                        "ACP tool call completed"
                    );
                }
                TurnEvent::Chunk { delta } => {
                    accumulated_text.push_str(delta);
                }
                TurnEvent::Plan { entries } => {
                    // Whole-list replace: keep only the latest plan for
                    // post-turn durable persistence.
                    latest_plan = Some(entries.clone());
                }
                _ => {}
            }
            if let Some(notification) = notification_for_turn_event(&session_id, &event) {
                self.write_notification(&notification).await;
            }
        }

        // Remove the cancel token regardless of outcome — the turn is over.
        // Lock poisoned invariant: same as the insert site above.
        self.remove_cancel_token(&session_id);

        let turn_result = turn_handle.await.map_err(|e| RpcError {
            code: INTERNAL_ERROR,
            message: format!("Agent task panicked: {e}"),
            data: None,
        })?;

        // Per ACP spec: a cancelled turn must respond with stopReason "cancelled",
        // not an error. Detect via ToolLoopCancelled propagated through anyhow.
        let was_cancelled = match &turn_result {
            Err(e) => zeroclaw_runtime::agent::loop_::is_tool_loop_cancelled(e),
            Ok(_) => false,
        };

        if was_cancelled {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Complete).with_category(::zeroclaw_log::EventCategory::Channel)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({
                        "tool_calls": tool_call_count,
                        "stop_reason": "cancelled",
                    })),
                "ACP session/prompt turn cancelled"
            );
            self.write_notification(&Self::turn_cancelled_notification(&session_id))
                .await;
            return Ok(Self::cancelled_prompt_result(session_id, &accumulated_text));
        }

        let (result_text, new_turn_msgs) = turn_result.map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail).with_category(::zeroclaw_log::EventCategory::Channel)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "error": e.to_string(),
                    })),
                "ACP session/prompt turn failed"
            );
            RpcError {
                code: INTERNAL_ERROR,
                message: format!("Agent turn failed: {e}"),
                data: None,
            }
        })?;

        // Persist new messages on successful, non-cancelled turns.
        if let Some(store) = &self.store
            && !new_turn_msgs.is_empty()
        {
            let store = store.clone();
            let sid = session_id.clone();
            let msgs = new_turn_msgs;
            let persisted =
                tokio::task::spawn_blocking(move || store.append_turn(&sid, &msgs)).await;
            let error = match persisted {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(e.to_string()),
                Err(join) => Some(join.to_string()),
            };
            if let Some(detail) = error {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_category(::zeroclaw_log::EventCategory::Channel)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "error": detail,
                        })),
                    "Failed to persist turn; session continues in memory"
                );
            }
        }

        // Durably persist the latest TodoWrite plan for this turn so it
        // replays on session/resume (best-effort; the live emission above
        // already reached the client). Whole-list replace, including an
        // empty list (a cleared plan).
        if let Some(store) = &self.store
            && let Some(entries) = latest_plan
        {
            let store = store.clone();
            let sid = session_id.clone();
            let persisted =
                tokio::task::spawn_blocking(move || store.set_plan(&sid, &entries)).await;
            let error = match persisted {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(e.to_string()),
                Err(join) => Some(join.to_string()),
            };
            if let Some(detail) = error {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_category(::zeroclaw_log::EventCategory::Channel)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({ "error": detail })),
                    "Failed to persist TodoWrite plan; session continues in memory"
                );
            }
        }

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Complete).with_category(::zeroclaw_log::EventCategory::Channel)
                .with_outcome(::zeroclaw_log::EventOutcome::Success)
                .with_attrs(::serde_json::json!({
                    "tool_calls": tool_call_count,
                    "response_len": result_text.len(),
                    "stop_reason": "end_turn",
                })),
            "ACP session/prompt turn complete"
        );

        Ok(Self::prompt_result(session_id, "end_turn", result_text))

        }).await
    }

    fn register_cancel_token(
        &self,
        session_id: &str,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> std::result::Result<(), RpcError> {
        let mut tokens = self
            .cancel_tokens
            .lock()
            .expect("cancel_tokens lock poisoned — invariant: all guarded critical sections are short, infallible HashMap ops");
        if tokens.contains_key(session_id) {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_category(::zeroclaw_log::EventCategory::Channel)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"session_id": session_id})),
                "ACP session/prompt rejected: session already has an active turn"
            );
            return Err(RpcError {
                code: SESSION_BUSY,
                message: format!("Session already has an active prompt turn: {session_id}"),
                data: None,
            });
        }
        tokens.insert(session_id.to_string(), cancel_token);
        Ok(())
    }

    fn remove_cancel_token(&self, session_id: &str) {
        self.cancel_tokens
            .lock()
            .expect("cancel_tokens lock poisoned — invariant: all guarded critical sections are short, infallible HashMap ops")
            .remove(session_id);
    }

    fn prompt_result(session_id: String, stop_reason: &'static str, text: String) -> Value {
        serde_json::json!({
            "sessionId": session_id,
            "stopReason": stop_reason,
            "content": text,
        })
    }

    fn cancelled_prompt_result(session_id: String, accumulated_text: &str) -> Value {
        let marker = zeroclaw_runtime::i18n::get_required_cli_string("turn-cancelled-client-rpc");
        let content = if accumulated_text.is_empty() {
            marker
        } else {
            format!("{accumulated_text}\n\n{marker}")
        };
        Self::prompt_result(session_id, "cancelled", content)
    }

    fn turn_cancelled_notification(session_id: &str) -> JsonRpcNotification {
        let marker = zeroclaw_runtime::i18n::get_required_cli_string("turn-cancelled-client-rpc");
        JsonRpcNotification {
            jsonrpc: "2.0",
            method: "session/update",
            params: serde_json::json!({
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": format!("turn-cancelled-{session_id}"),
                    "name": "turn-cancelled",
                    "title": "turn-cancelled",
                    "kind": "think",
                    "status": "completed",
                    "content": [{
                        "type": "content",
                        "content": { "type": "text", "text": marker }
                    }]
                }
            }),
        }
    }

    fn parse_prompt(params: &Value) -> std::result::Result<String, RpcError> {
        match params.get("prompt") {
            Some(Value::String(s)) => Ok(s.clone()),
            Some(Value::Array(arr)) => {
                let mut joined = String::new();
                for part in arr {
                    let mut added = false;
                    if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                        if !joined.is_empty() {
                            joined.push_str("\n\n");
                        }
                        joined.push_str(text);
                        added = true;
                    }
                    // Support ACP resource blocks for @-notation file attachments
                    // (clients send {"type":"resource","resource":{"uri":"...","text":"..."}})
                    if let Some(res) = part.get("resource")
                        && let Some(text) = res.get("text").and_then(|v| v.as_str())
                    {
                        if added || !joined.is_empty() {
                            joined.push_str("\n\n");
                        }
                        joined.push_str(text);
                    }
                }
                if joined.is_empty() {
                    return Err(RpcError {
                        code: INVALID_PARAMS,
                        message: "Parameter 'prompt' array must contain at least one text part"
                            .to_string(),
                        data: None,
                    });
                }
                Ok(joined)
            }
            _ => Err(RpcError {
                code: INVALID_PARAMS,
                message: "Missing required parameter: prompt (must be string or array of parts)"
                    .to_string(),
                data: None,
            }),
        }
    }

    async fn handle_session_stop(&self, params: &Value) -> RpcResult {
        let session_id = params
            .get("sessionId")
            .or_else(|| params.get("session_id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| RpcError {
                code: INVALID_PARAMS,
                message: "Missing required parameter: sessionId".to_string(),
                data: None,
            })?;

        let session_arc = {
            let mut sessions = self.sessions.lock().await;
            sessions.remove(session_id).ok_or_else(|| RpcError {
                code: SESSION_NOT_FOUND,
                message: format!("Session not found: {session_id}"),
                data: None,
            })?
        };

        // Wait for any in-flight prompt turn to finish before cleaning up.
        // The inner lock is held by the turn task; this blocks until it drops.
        let session = session_arc.lock().await;
        let agent_alias = session.agent_alias.clone();
        let model_provider = session.model_provider.clone();
        let model = session.model.clone();
        // Drop the ACP back-channel from each tool's channel map so the
        // session's RpcOutbound clone isn't kept alive by stale entries.
        session.agent.channel_handles().unregister_channel("acp");
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Complete)
                .with_category(::zeroclaw_log::EventCategory::Channel)
                .with_outcome(::zeroclaw_log::EventOutcome::Success)
                .with_attrs(::serde_json::json!({
                    "session_id": session_id,
                    "agent_alias": agent_alias,
                    "model_provider": model_provider,
                    "model": model,
                })),
            "ACP session stopped"
        );
        Ok(serde_json::json!({
            "sessionId": session_id,
            "stopped": true,
        }))
    }

    async fn handle_session_cancel(&self, params: &Value) -> RpcResult {
        let session_id = params
            .get("sessionId")
            .or_else(|| params.get("session_id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| RpcError {
                code: INVALID_PARAMS,
                message: "Missing required parameter: sessionId".to_string(),
                data: None,
            })?;

        let token = self
            .cancel_tokens
            .lock()
            .expect("cancel_tokens lock poisoned — invariant: all guarded critical sections are short, infallible HashMap ops")
            .get(session_id)
            .cloned();

        if let Some(token) = token {
            token.cancel();
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_category(::zeroclaw_log::EventCategory::Channel)
                    .with_attrs(::serde_json::json!({"session_id": session_id})),
                "ACP session/cancel: fired cancel token for active turn"
            );
        }

        Ok(serde_json::json!({}))
    }

    async fn handle_session_event(&self, params: &Value) -> RpcResult {
        let session_id = params
            .get("sessionId")
            .or_else(|| params.get("session_id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| RpcError {
                code: INVALID_PARAMS,
                message: "Missing required parameter: sessionId".to_string(),
                data: None,
            })?
            .to_string();

        let event_type = params
            .get("type")
            .or_else(|| params.get("update").and_then(|u| u.get("sessionUpdate")))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_category(::zeroclaw_log::EventCategory::Channel)
                .with_attrs(
                    ::serde_json::json!({"event_type": event_type, "session_id": session_id})
                ),
            "Received session update (type=) for session"
        );

        let session_arc = {
            let sessions = self.sessions.lock().await;
            sessions.get(&session_id).cloned()
        };

        if let Some(session_arc) = session_arc {
            // Best-effort last_active update. If the inner lock is held by an
            // active turn, skip it — the turn itself updates last_active on completion.
            if let Ok(mut session) = session_arc.try_lock() {
                session.last_active = Instant::now();
            }
            Ok(serde_json::json!({
                "sessionId": session_id,
                "type": event_type,
                "status": "processed"
            }))
        } else {
            Err(RpcError {
                code: SESSION_NOT_FOUND,
                message: format!("Session not found: {session_id}"),
                data: None,
            })
        }
    }

    // ── I/O helpers ──────────────────────────────────────────────

    async fn write_result(&self, id: Value, result: Value) {
        let response = JsonRpcResponse {
            jsonrpc: "2.0",
            result: Some(result),
            error: None,
            id,
        };
        self.write_json(&response).await;
    }

    async fn write_error(&self, id: Value, code: i32, message: &str) {
        let response = JsonRpcResponse {
            jsonrpc: "2.0",
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.to_string(),
                data: None,
            }),
            id,
        };
        self.write_json(&response).await;
    }

    async fn write_notification(&self, notification: &JsonRpcNotification) {
        self.write_json(notification).await;
    }

    async fn write_json<T: Serialize>(&self, value: &T) {
        match serde_json::to_string(value) {
            Ok(json) => {
                if !self.rpc.send_raw(json).await {
                    ::zeroclaw_log::record!(
                        ERROR,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                            .with_category(::zeroclaw_log::EventCategory::Channel)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                        "ACP writer task closed; dropping outbound message"
                    );
                }
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_category(::zeroclaw_log::EventCategory::Channel)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "Failed to serialize JSON-RPC message"
                );
            }
        }
    }
}

/// Single writer task that owns stdout. All outbound JSON-RPC messages flow
/// through here, so concurrent notifications and outbound requests don't
/// interleave bytes.
async fn writer_task(mut rx: mpsc::Receiver<String>) {
    let mut stdout = tokio::io::stdout();
    while let Some(line) = rx.recv().await {
        if let Err(e) = stdout.write_all(line.as_bytes()).await {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_category(::zeroclaw_log::EventCategory::Channel)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "Failed to write to stdout"
            );
            continue;
        }
        if let Err(e) = stdout.write_all(b"\n").await {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_category(::zeroclaw_log::EventCategory::Channel)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "Failed to write newline to stdout"
            );
            continue;
        }
        if let Err(e) = stdout.flush().await {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_category(::zeroclaw_log::EventCategory::Channel)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "Failed to flush stdout"
            );
        }
    }
}

fn to_acp_raw_input(name: &str, args: &Value) -> Value {
    match name {
        "file_edit" => {
            let path = args.get("path").cloned().unwrap_or(Value::Null);
            let old_text = args.get("old_string").cloned().unwrap_or(Value::Null);
            let new_text = args.get("new_string").cloned().unwrap_or(Value::Null);
            serde_json::json!({ "path": path, "oldText": old_text, "newText": new_text })
        }
        "file_write" => {
            let path = args.get("path").cloned().unwrap_or(Value::Null);
            let new_text = args.get("content").cloned().unwrap_or(Value::Null);
            serde_json::json!({ "path": path, "newText": new_text })
        }
        _ => args.clone(),
    }
}

fn to_acp_content(name: &str, args: &Value) -> Value {
    match name {
        "file_edit" => {
            let path = args.get("path").cloned().unwrap_or(Value::Null);
            let old_text = args.get("old_string").cloned().unwrap_or(Value::Null);
            let new_text = args.get("new_string").cloned().unwrap_or(Value::Null);
            serde_json::json!([{ "type": "diff", "path": path, "oldText": old_text, "newText": new_text }])
        }
        "file_write" => {
            let path = args.get("path").cloned().unwrap_or(Value::Null);
            let new_text = args.get("content").cloned().unwrap_or(Value::Null);
            serde_json::json!([{ "type": "diff", "path": path, "newText": new_text }])
        }
        _ => serde_json::json!([]),
    }
}

fn map_tool_kind(name: &str) -> &'static str {
    match name {
        "ask_user" | "calculator" | "composio" | "delegate" | "escalate_to_human"
        | "execute_pipeline" | "jira" | "llm_task" | "schedule" | "security_ops" | "shell"
        | "vi_verify" => "execute",
        "backup" | "browser_open" | "canvas" | "cloud_ops" | "file_edit" | "file_write"
        | "memory_export" | "memory_store" | "report_template" => "edit",
        "cron_add" | "poll" | "reaction" => "edit",
        "memory_forget" | "memory_purge" => "delete",
        // ACP clients often treat `read`/`search`/`fetch` calls as noisy
        // background context gathering and keep their content collapsed. These
        // ZeroClaw tools return user-visible text, so use `other` to keep the
        // result content surfaced consistently across clients.
        "content_search" | "discord_search" | "glob_search" | "knowledge" | "search"
        | "tool_search" | "web_search_tool" => "other",
        "browser"
        | "cloud_patterns"
        | "data_management"
        | "file_read"
        | "git_operations"
        | "google_workspace"
        | "hardware_board_info"
        | "hardware_memory_map"
        | "hardware_memory_read"
        | "image_info"
        | "linkedin"
        | "microsoft365"
        | "model_routing_config"
        | "model_switch"
        | "project_intel"
        | "proxy_config"
        | "read_skill"
        | "sessions_history"
        | "sessions_list"
        | "text_browser"
        | "weather"
        | "workspace" => "other",
        "cron_list" | "cron_runs" | "memory_recall" => "other",
        "http_request" | "web_fetch" => "other",
        "image_gen" => "other",
        "cron_remove" => "delete",
        "cron_run" => "execute",
        "sessions_send" => "execute",
        _ => "other",
    }
}

fn notification_for_turn_event(session_id: &str, event: &TurnEvent) -> Option<JsonRpcNotification> {
    Some(match event {
        TurnEvent::Chunk { delta } => JsonRpcNotification {
            jsonrpc: "2.0",
            method: "session/update",
            params: serde_json::json!({
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {
                        "type": "text",
                        "text": delta
                    }
                }
            }),
        },
        TurnEvent::ToolCall { id, name, args } => {
            let acp_content = to_acp_content(name, args);
            let mut update = serde_json::json!({
                "sessionUpdate": "tool_call",
                "toolCallId": id,
                "name": name,
                "title": name,
                "kind": map_tool_kind(name),
                "rawInput": to_acp_raw_input(name, args),
                "status": "pending"
            });
            if acp_content
                .as_array()
                .is_some_and(|items| !items.is_empty())
            {
                update["content"] = acp_content;
            }
            JsonRpcNotification {
                jsonrpc: "2.0",
                method: "session/update",
                params: serde_json::json!({
                    "sessionId": session_id,
                    "update": update
                }),
            }
        }
        TurnEvent::ToolResult { id, name, output } => JsonRpcNotification {
            jsonrpc: "2.0",
            method: "session/update",
            params: serde_json::json!({
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": id,
                    "name": name,
                    "title": name,
                    "kind": map_tool_kind(name),
                    "status": "completed",
                    "rawOutput": output,
                    "body": output,
                    "content": [{
                        "type": "content",
                        "content": {
                            "type": "text",
                            "text": output
                        }
                    }]
                }
            }),
        },
        TurnEvent::Thinking { delta } => JsonRpcNotification {
            jsonrpc: "2.0",
            method: "session/update",
            params: serde_json::json!({
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "agent_thought_chunk",
                    "content": {
                        "type": "text",
                        "text": delta
                    }
                }
            }),
        },
        TurnEvent::ApprovalRequest { .. } => return None,
        TurnEvent::HistoryTrimmed {
            dropped_messages,
            kept_turns,
            reason,
        } => JsonRpcNotification {
            jsonrpc: "2.0",
            // ACP's SessionUpdate union is closed. Custom notifications use
            // underscore-prefixed methods so clients can safely ignore them.
            method: "_zeroclaw/history_trimmed",
            params: serde_json::json!({
                "sessionId": session_id,
                "droppedMessages": dropped_messages,
                "keptTurns": kept_turns,
                "reason": reason,
            }),
        },
        TurnEvent::Plan { entries } => JsonRpcNotification {
            jsonrpc: "2.0",
            method: "session/update",
            params: serde_json::json!({
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "plan",
                    // PlanEntry serializes to the ACP-faithful
                    // { content, priority, status } shape (+ additive
                    // activeForm when present, which strict ACP clients
                    // ignore). Whole-list replace per the ACP plan spec.
                    "entries": entries,
                }
            }),
        },
        // Usage events are filtered out at every call site (ACP has no
        // `session/update` shape for them; the cost tracker records them
        // out-of-band). Reaching this arm means a caller forgot the filter.
        TurnEvent::Usage { .. } => unreachable!(
            "TurnEvent::Usage must be filtered before notification_for_turn_event; \
             ACP has no session/update notification for token usage"
        ),
    })
}

fn history_notifications_for_message(
    session_id: &str,
    msg: &ConversationMessage,
) -> Vec<JsonRpcNotification> {
    match msg {
        ConversationMessage::Chat(chat) => {
            let update_type = match chat.role.as_str() {
                "user" => "user_message_chunk",
                "assistant" => "agent_message_chunk",
                _ => return vec![],
            };
            vec![JsonRpcNotification {
                jsonrpc: "2.0",
                method: "session/update",
                params: serde_json::json!({
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": update_type,
                        "content": { "type": "text", "text": &chat.content }
                    }
                }),
            }]
        }
        ConversationMessage::AssistantToolCalls {
            text, tool_calls, ..
        } => {
            let mut notifications = Vec::new();
            if let Some(t) = text
                && !t.is_empty()
            {
                notifications.push(JsonRpcNotification {
                    jsonrpc: "2.0",
                    method: "session/update",
                    params: serde_json::json!({
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "content": { "type": "text", "text": t }
                        }
                    }),
                });
            }
            for tc in tool_calls {
                let args: serde_json::Value =
                    serde_json::from_str(&tc.arguments).unwrap_or(serde_json::Value::Null);
                let acp_content = to_acp_content(&tc.name, &args);
                let mut update = serde_json::json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": &tc.id,
                    "name": &tc.name,
                    "title": &tc.name,
                    "kind": map_tool_kind(&tc.name),
                    "rawInput": to_acp_raw_input(&tc.name, &args),
                    "status": "completed"
                });
                if acp_content
                    .as_array()
                    .is_some_and(|items| !items.is_empty())
                {
                    update["content"] = acp_content;
                }
                notifications.push(JsonRpcNotification {
                    jsonrpc: "2.0",
                    method: "session/update",
                    params: serde_json::json!({
                        "sessionId": session_id,
                        "update": update
                    }),
                });
            }
            notifications
        }
        ConversationMessage::ToolResults(results) => results
            .iter()
            .map(|r| JsonRpcNotification {
                jsonrpc: "2.0",
                method: "session/update",
                params: serde_json::json!({
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": &r.tool_call_id,
                        "status": "completed",
                        "rawOutput": &r.content,
                        "body": &r.content,
                        "content": [{
                            "type": "content",
                            "content": { "type": "text", "text": &r.content }
                        }]
                    }
                }),
            })
            .collect(),
    }
}

// ── Error helper ─────────────────────────────────────────────────

#[derive(Debug)]
struct RpcError {
    code: i32,
    message: String,
    #[allow(dead_code)] // JSON-RPC spec field, used for structured error data
    data: Option<Value>,
}

type RpcResult = std::result::Result<Value, RpcError>;

#[cfg(test)]
mod tests;

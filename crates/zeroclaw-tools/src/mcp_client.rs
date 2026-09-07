//! MCP (Model Context Protocol) client — connects to external tool servers.
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
use tokio::sync::{Mutex, RwLock};
use tokio::time::{Duration, Instant, timeout, timeout_at};

use crate::mcp_era::{
    CreateTask, DiscoverNegotiateError, MCP_MODERN_PROTOCOL_VERSION, McpInputRequiredError,
    McpResultKind, PeerEra, PeerProtocol, ResultTypeError, TaskPollState, VersionQuality,
    attach_input_retry, attach_request_meta, cache_hints_from_result, classify_mcp_result,
    is_recognized_modern_error, local_cache_ttl, parse_task_poll_result, redact_known_task_id,
    versions_from_unsupported_error,
};
use crate::mcp_prompt::{McpGetPromptResult, McpPromptsListResult};
use crate::mcp_protocol::{JsonRpcRequest, MCP_PROTOCOL_VERSION, McpToolDef, McpToolsListResult};
use crate::mcp_resource::{McpResourceContents, McpResourcesListResult};
use crate::mcp_task::{
    MAX_TASK_POLL_WALL, MAX_TASK_POLLS, McpTaskPending, McpTaskStore, TaskContinuation,
    TaskHandleError, parse_continuation, poll_delay, require_responses_if_needed,
};
use crate::mcp_transport::{
    McpRecoveryGate, McpRequestLifecycle, McpTransportError, SharedMcpTransportConn,
    create_shared_transport,
};
use zeroclaw_config::schema::{McpServerConfig, McpTransport};

/// Timeout for receiving a response from an MCP server during init/list.
/// Prevents a hung server from blocking the daemon indefinitely.
const RECV_TIMEOUT_SECS: u64 = 30;

/// Default timeout for tool calls (seconds) when not configured per-server.
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 180;

/// Maximum allowed tool call timeout (seconds) — hard safety ceiling.
const MAX_TOOL_TIMEOUT_SECS: u64 = 600;

/// Maximum automatic reconnect attempts when a request is known not to have
/// been written. Outcome-unknown requests are never replayed.
const MAX_RECONNECT_ATTEMPTS: u32 = 2;

/// JSON-RPC id reserved for the `server/discover` era probe so the legacy
/// `initialize` id stays `1` (stdio fixtures and session traces).
const DISCOVER_PROBE_REQUEST_ID: u64 = 0;

/// Bound for the era probe. Legacy servers may ignore unknown methods; the
/// spec says to fall back after a reasonable timeout rather than hang.
const DISCOVER_PROBE_TIMEOUT_SECS: u64 = 5;

/// Outcome of the `server/discover` backward-compatibility probe.
enum ProbeOutcome {
    /// Peer answered as a modern server. Stage 2 speaks `_meta` / headers
    /// and skips `initialize`.
    Modern {
        peer: PeerProtocol,
        capabilities: McpServerCapabilities,
    },
    /// Probe unanswered or not a recognized modern error: legacy peer.
    Legacy,
    /// Modern peer listed no revision this client knows.
    Incompatible(DiscoverNegotiateError),
    /// Discover classified Modern, but the result's `resultType` was missing
    /// or malformed. Fail closed; do not guess `complete` or fall back to
    /// Legacy.
    InvalidModernResult(ResultTypeError),
}

struct OpenedSession {
    capabilities: McpServerCapabilities,
    peer: PeerProtocol,
}

fn log_version_quality(server_name: &str, peer: &PeerProtocol) {
    let era = match peer.era {
        PeerEra::Legacy => "legacy",
        PeerEra::Modern => "modern",
    };
    match peer.quality {
        VersionQuality::Known => {}
        VersionQuality::Malformed => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({
                        "mcp_server": server_name,
                        "raw_protocol_version": &peer.advertised,
                        "resolved_version": &peer.version,
                        "era": era,
                    })),
                "mcp_client: malformed MCP protocolVersion; falling back to oldest known revision"
            );
        }
        VersionQuality::UnknownRevision => {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({
                        "mcp_server": server_name,
                        "advertised_version": &peer.advertised,
                        "resolved_version": &peer.version,
                        "era": era,
                    })),
                "mcp_client: unknown MCP protocol version; using nearest known revision"
            );
        }
    }
}

fn supported_versions_from_discover_result(result: &serde_json::Value) -> Option<Vec<String>> {
    result
        .get("supportedVersions")
        .and_then(|value| value.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .filter(|versions| !versions.is_empty())
}

fn classify_discover_response(resp: crate::mcp_protocol::JsonRpcResponse) -> ProbeOutcome {
    if let Some(error) = resp.error {
        if is_recognized_modern_error(error.code) {
            let supported = versions_from_unsupported_error(&error);
            return match PeerProtocol::from_discover_supported(&supported) {
                Ok(peer) => match peer.era {
                    PeerEra::Modern => ProbeOutcome::Modern {
                        peer,
                        capabilities: McpServerCapabilities::default(),
                    },
                    PeerEra::Legacy => ProbeOutcome::Legacy,
                },
                Err(err) => ProbeOutcome::Incompatible(err),
            };
        }
        return ProbeOutcome::Legacy;
    }
    let Some(result) = resp.result.as_ref() else {
        return ProbeOutcome::Legacy;
    };
    let Some(supported) = supported_versions_from_discover_result(result) else {
        return ProbeOutcome::Legacy;
    };
    match PeerProtocol::from_discover_supported(&supported) {
        Ok(peer) => match peer.era {
            PeerEra::Modern => {
                match classify_mcp_result(PeerEra::Modern, "server/discover", result) {
                    Ok(McpResultKind::Complete) => ProbeOutcome::Modern {
                        peer,
                        capabilities: McpServerCapabilities::from_init_result(result),
                    },
                    Ok(McpResultKind::InputRequired(_)) => ProbeOutcome::InvalidModernResult(
                        ResultTypeError::InputRequiredNotAllowed {
                            method: "server/discover".to_string(),
                        },
                    ),
                    Ok(McpResultKind::Task(_)) => {
                        ProbeOutcome::InvalidModernResult(ResultTypeError::TaskNotAllowed {
                            method: "server/discover".to_string(),
                        })
                    }
                    Err(err) => ProbeOutcome::InvalidModernResult(err),
                }
            }
            PeerEra::Legacy => ProbeOutcome::Legacy,
        },
        Err(err) => ProbeOutcome::Incompatible(err),
    }
}

fn discover_probe_request() -> JsonRpcRequest {
    JsonRpcRequest::new(
        DISCOVER_PROBE_REQUEST_ID,
        "server/discover",
        json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": MCP_MODERN_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientInfo": {
                    "name": "zeroclaw",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }),
    )
}

fn is_modern_wire_rejection(resp: &crate::mcp_protocol::JsonRpcResponse) -> bool {
    resp.error
        .as_ref()
        .is_some_and(|error| is_recognized_modern_error(error.code))
}

async fn send_discover_probe(
    transport: &dyn SharedMcpTransportConn,
    epoch: u64,
    peer: &PeerProtocol,
) -> Option<crate::mcp_protocol::JsonRpcResponse> {
    let lifecycle = McpRequestLifecycle::uncoordinated_for_peer(epoch, peer);
    match timeout(
        Duration::from_secs(DISCOVER_PROBE_TIMEOUT_SECS),
        transport.send_and_recv(&discover_probe_request(), &lifecycle),
    )
    .await
    {
        Ok(Ok(resp)) => Some(resp),
        Ok(Err(_)) | Err(_) => None,
    }
}

/// Probe `server/discover`. The first attempt uses the Stage 1 legacy
/// HTTP lifecycle (no modern headers) so a Legacy peer sees the same
/// bytes as master. A recognized modern rejection (`HeaderMismatch` and
/// siblings) triggers one retry with modern headers.
async fn probe_peer_era(transport: &dyn SharedMcpTransportConn, epoch: u64) -> ProbeOutcome {
    let first = match send_discover_probe(transport, epoch, &PeerProtocol::legacy_default()).await {
        Some(resp) => resp,
        None => return ProbeOutcome::Legacy,
    };
    let resp = if is_modern_wire_rejection(&first) {
        send_discover_probe(transport, epoch, &PeerProtocol::modern_default())
            .await
            .unwrap_or(first)
    } else {
        first
    };
    classify_discover_response(resp)
}

/// Perform the MCP `initialize` + `notifications/initialized` handshake on a
/// transport. Shared by [`open_session`] (Legacy arm) and the
/// reconnect-after-stale-session path in [`McpServer::reestablish`].
async fn handshake(
    transport: &dyn SharedMcpTransportConn,
    server_name: &str,
    epoch: u64,
) -> Result<(McpServerCapabilities, PeerProtocol)> {
    let init_req = JsonRpcRequest::new(
        1,
        "initialize",
        json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": { "resources": {}, "prompts": {} },
            "clientInfo": {
                "name": "zeroclaw",
                "version": env!("CARGO_PKG_VERSION")
            }
        }),
    );

    let init_lifecycle = McpRequestLifecycle::uncoordinated(epoch);
    let init_resp = timeout(
        Duration::from_secs(RECV_TIMEOUT_SECS),
        transport.send_and_recv(&init_req, &init_lifecycle),
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

    // Parse server-advertised capabilities and protocol version from the
    // initialize result. The version was previously sent and then ignored.
    let capabilities = init_resp
        .result
        .as_ref()
        .map(McpServerCapabilities::from_init_result)
        .unwrap_or_default();
    let version_field = init_resp
        .result
        .as_ref()
        .and_then(|result| result.get("protocolVersion"));
    let peer = PeerProtocol::from_initialize_field(version_field);
    log_version_quality(server_name, &peer);

    // Notify the server the client is initialized (notifications expect no
    // response). Best effort — ignore errors.
    let notif = JsonRpcRequest::notification("notifications/initialized", json!({}));
    let notif_lifecycle = McpRequestLifecycle::uncoordinated(epoch);
    let _ = transport.send_and_recv(&notif, &notif_lifecycle).await;

    Ok((capabilities, peer))
}

/// Resolve [`PeerEra`] via `server/discover`. Modern peers skip the
/// initialize handshake and speak per-request `_meta`; Legacy peers keep
/// initialize. The client declares [`MCP_PROTOCOL_VERSION`]; a Legacy
/// server that answers with an older date is recorded via Stage 1
/// negotiation.
async fn open_session(
    transport: &dyn SharedMcpTransportConn,
    server_name: &str,
    epoch: u64,
) -> Result<OpenedSession> {
    match probe_peer_era(transport, epoch).await {
        ProbeOutcome::Incompatible(err) => {
            bail!("MCP server `{server_name}` is incompatible: {err}");
        }
        ProbeOutcome::InvalidModernResult(err) => {
            bail!("MCP server `{server_name}` is incompatible: {err}");
        }
        ProbeOutcome::Modern { peer, capabilities } => {
            log_version_quality(server_name, &peer);
            Ok(OpenedSession { capabilities, peer })
        }
        ProbeOutcome::Legacy => {
            let (capabilities, peer) = handshake(transport, server_name, epoch).await?;
            Ok(OpenedSession { capabilities, peer })
        }
    }
}

/// Server-advertised MCP capabilities parsed from the `initialize` result.
/// Sub-flags `subscribe` / `listChanged` are captured but currently unused
/// (reserved for a future subscriptions spec).
#[derive(Debug, Clone, Default)]
pub struct McpServerCapabilities {
    pub(crate) resources: bool,
    pub(crate) prompts: bool,
}

impl McpServerCapabilities {
    /// Parse from the raw `initialize` result value. A capability counts as
    /// supported when its object key is present under `capabilities`.
    pub fn from_init_result(result: &serde_json::Value) -> Self {
        let caps = result.get("capabilities");
        let has = |key: &str| caps.and_then(|c| c.get(key)).is_some();
        Self {
            resources: has("resources"),
            prompts: has("prompts"),
        }
    }

    pub fn supports_resources(&self) -> bool {
        self.resources
    }

    pub fn supports_prompts(&self) -> bool {
        self.prompts
    }
}

fn check_result_is_error(result: &serde_json::Value, op: &str, server_name: &str) -> Result<()> {
    if result.get("isError").and_then(serde_json::Value::as_bool) != Some(true) {
        return Ok(());
    }
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
    let detail = zeroclaw_providers::sanitize_api_error(&detail);
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
            .with_attrs(::serde_json::json!({
                "mcp_server": server_name,
                "op": op,
                "detail": &detail,
            })),
        "mcp_client: MCP result returned isError:true"
    );
    bail!("MCP `{op}` (server `{server_name}`) returned isError: {detail}");
}

/// Consume a JSON-RPC `result` under [`PeerEra`]. Complete payloads pass
/// through; `input_required` is a typed error used by list/connect paths
/// that must not mint a handle; malformed modern envelopes fail closed.
fn require_complete_result(
    era: PeerEra,
    method: &str,
    result: serde_json::Value,
) -> Result<serde_json::Value> {
    match classify_mcp_result(era, method, &result) {
        Ok(McpResultKind::Complete) => Ok(result),
        Ok(McpResultKind::InputRequired(input_required)) => Err(McpInputRequiredError {
            method: method.to_string(),
            input_required,
        }
        .into()),
        Ok(McpResultKind::Task(_)) => Err(ResultTypeError::TaskNotAllowed {
            method: method.to_string(),
        }
        .into()),
        Err(err) => Err(anyhow::Error::msg(format!(
            "MCP `{method}` resultType rejected: {err}"
        ))),
    }
}

fn cached_list<T: Clone>(
    cache: &HashMap<Option<String>, ListCacheEntry<T>>,
    cursor: &Option<String>,
) -> Option<T> {
    let entry = cache.get(cursor)?;
    (Instant::now() < entry.expires_at).then(|| entry.value.clone())
}

/// Cap list TTLs so `Duration`/`Instant` construction cannot panic.
const MAX_LIST_TTL_MS: u64 = 86_400_000 * 366;

fn expiry_from_ttl_ms(ttl_ms: u64) -> Option<Instant> {
    if ttl_ms == 0 || ttl_ms > MAX_LIST_TTL_MS {
        return None;
    }
    Instant::now().checked_add(Duration::from_millis(ttl_ms))
}

fn store_list_cache<T>(
    cache: &mut HashMap<Option<String>, ListCacheEntry<T>>,
    cursor: Option<String>,
    value: T,
    raw: &serde_json::Value,
) {
    let Some(hints) = cache_hints_from_result(raw) else {
        return;
    };
    let Some(expires_at) = expiry_from_ttl_ms(hints.ttl_ms) else {
        return;
    };
    if local_cache_ttl(hints).is_none() {
        return;
    }
    cache.insert(cursor, ListCacheEntry { value, expires_at });
}

fn tools_ttl_from_list_result(era: PeerEra, raw: &serde_json::Value) -> ToolsTtl {
    if era != PeerEra::Modern {
        return ToolsTtl::Sticky;
    }
    match cache_hints_from_result(raw) {
        Some(hints) if hints.ttl_ms == 0 => ToolsTtl::AlwaysRefresh,
        Some(hints) => match expiry_from_ttl_ms(hints.ttl_ms) {
            Some(expires_at) => ToolsTtl::Until(expires_at),
            None => ToolsTtl::AlwaysRefresh,
        },
        None => ToolsTtl::Sticky,
    }
}

fn tools_list_stale(inner: &McpServerInner) -> bool {
    if inner.peer.era != PeerEra::Modern {
        return false;
    }
    match inner.tools_ttl {
        ToolsTtl::Sticky => false,
        ToolsTtl::AlwaysRefresh => true,
        ToolsTtl::Until(expires_at) => Instant::now() >= expires_at,
    }
}

// ── Internal server state ──────────────────────────────────────────────────

struct ListCacheEntry<T> {
    value: T,
    expires_at: Instant,
}

#[derive(Clone, Copy, Default)]
enum ToolsTtl {
    #[default]
    Sticky,
    Until(Instant),
    AlwaysRefresh,
}

#[derive(Default)]
struct ListCaches {
    resources: HashMap<Option<String>, ListCacheEntry<McpResourcesListResult>>,
    prompts: HashMap<Option<String>, ListCacheEntry<McpPromptsListResult>>,
}

struct McpServerInner {
    config: McpServerConfig,
    #[cfg(target_has_atomic = "64")]
    next_id: AtomicU64,
    #[cfg(not(target_has_atomic = "64"))]
    next_id: AtomicU32,
    tools: Vec<McpToolDef>,
    capabilities: McpServerCapabilities,
    peer: PeerProtocol,
    list_caches: ListCaches,
    tools_ttl: ToolsTtl,
    tasks: McpTaskStore,
}

// ── Recovery barrier ────────────────────────────────────────────────────────

/// Synchronously-published gate that blocks new writes while a post-write
/// outcome-unknown request is being recovered.
///
/// When a request's outcome becomes unknown after its bytes may have reached
/// the server, `arm` is called *synchronously* — before any lock the failing
/// request held is released — so that a second write already queued on the
/// serial/epoch gate observes the recovery-needed state and waits, instead of
/// racing ahead onto the ambiguous session. `finish` clears the gate once
/// reset + re-handshake succeed; `poison` leaves it permanently closed after a
/// failed recovery so later writes fail closed rather than proceeding without a
/// successful MCP handshake.
struct RecoveryBarrier {
    /// The epoch that must be recovered before writes may resume. `None` means
    /// no recovery is pending.
    needed_epoch: std::sync::Mutex<Option<u64>>,
    /// Set once recovery has permanently failed; the connection is unusable.
    poisoned: std::sync::atomic::AtomicBool,
    /// Pulsed whenever the recovery-needed state changes (cleared or poisoned)
    /// so writers waiting in `wait_ready` wake up.
    notify: tokio::sync::Notify,
}

impl RecoveryBarrier {
    fn new() -> Self {
        Self {
            needed_epoch: std::sync::Mutex::new(None),
            poisoned: std::sync::atomic::AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }

    /// Publish that the observed epoch now needs recovery before any further
    /// write. Idempotent for a given epoch; a newer epoch supersedes an older
    /// pending one. This is intentionally synchronous (no `.await`) so it takes
    /// effect the instant an outcome becomes unknown.
    fn arm(&self, epoch: u64) {
        let mut needed = self.needed_epoch.lock().unwrap();
        match *needed {
            Some(existing) if existing >= epoch => {}
            _ => *needed = Some(epoch),
        }
    }

    /// Clear the recovery-needed state after a successful reset + re-handshake
    /// for `recovered_epoch`, then wake any waiting writers.
    fn finish(&self, recovered_epoch: u64) {
        {
            let mut needed = self.needed_epoch.lock().unwrap();
            if matches!(*needed, Some(pending) if pending <= recovered_epoch) {
                *needed = None;
            }
        }
        self.notify.notify_waiters();
    }

    /// Mark recovery as permanently failed. Subsequent writers fail closed.
    fn poison(&self) {
        self.poisoned
            .store(true, std::sync::atomic::Ordering::Release);
        self.notify.notify_waiters();
    }

    fn is_poisoned(&self) -> bool {
        self.poisoned.load(std::sync::atomic::Ordering::Acquire)
    }

    fn recovery_pending(&self) -> bool {
        self.needed_epoch.lock().unwrap().is_some()
    }
}

impl McpRecoveryGate for RecoveryBarrier {
    fn arm(&self, epoch: u64) {
        RecoveryBarrier::arm(self, epoch);
    }

    fn write_blocked(&self) -> bool {
        self.is_poisoned() || self.recovery_pending()
    }
}

/// RAII guard that arms the recovery barrier synchronously if a write's outcome
/// became unknown, at the moment its `send_request` future completes or is
/// cancelled. Held under the serial gate and dropped before it, so a queued
/// writer observes the armed barrier before it can acquire the gate.
struct WriteBarrierArm<'a> {
    recovery: &'a RecoveryBarrier,
    lifecycle: &'a McpRequestLifecycle,
}

impl Drop for WriteBarrierArm<'_> {
    fn drop(&mut self) {
        if let Some(epoch) = self.lifecycle.outcome_unknown_epoch() {
            self.recovery.arm(epoch);
        }
    }
}

/// Discards an in-process extension handle if the poll future is dropped
/// (cancellation) before a terminal or model-visible pending outcome.
struct ExtensionHandleGuard {
    inner: Arc<Mutex<McpServerInner>>,
    handle: Option<String>,
}

impl ExtensionHandleGuard {
    fn new(inner: Arc<Mutex<McpServerInner>>, handle: Option<String>) -> Self {
        Self { inner, handle }
    }

    fn defuse(&mut self) -> Option<String> {
        self.handle.take()
    }
}

impl Drop for ExtensionHandleGuard {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        if let Ok(mut inner) = self.inner.try_lock() {
            inner.tasks.discard(&handle);
            return;
        }
        let inner = Arc::clone(&self.inner);
        zeroclaw_spawn::spawn!(async move {
            inner.lock().await.tasks.discard(&handle);
        });
    }
}

// ── McpServer ──────────────────────────────────────────────────────────────

/// A live connection to one MCP server (any transport).
#[derive(Clone)]
pub struct McpServer {
    inner: Arc<Mutex<McpServerInner>>,
    transport: Arc<dyn SharedMcpTransportConn>,
    epoch_gate: Arc<RwLock<u64>>,
    /// Preserves the existing single-request behavior for HTTP/SSE while
    /// allowing stdio requests to multiplex by response id.
    serial_gate: Option<Arc<Mutex<()>>>,
    /// Synchronously-published gate that holds back new writes until an
    /// outcome-unknown request has been recovered (or fails them closed after a
    /// failed recovery).
    recovery: Arc<RecoveryBarrier>,
}

struct OutcomeUnknownGuard {
    server: McpServer,
    lifecycle: Arc<McpRequestLifecycle>,
    operation: String,
    armed: bool,
}

impl OutcomeUnknownGuard {
    fn new(server: McpServer, lifecycle: Arc<McpRequestLifecycle>, operation: String) -> Self {
        Self {
            server,
            lifecycle,
            operation,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for OutcomeUnknownGuard {
    fn drop(&mut self) {
        if self.armed
            && let Some(epoch) = self.lifecycle.outcome_unknown_epoch()
        {
            self.server.spawn_recovery(epoch, self.operation.clone());
        }
    }
}

impl McpServer {
    /// Connect to the server, perform the initialize handshake, and fetch the tool list.
    pub async fn connect(config: McpServerConfig) -> Result<Self> {
        // Create transport based on config
        let transport: Arc<dyn SharedMcpTransportConn> =
            Arc::from(create_shared_transport(&config).with_context(|| {
                format!(
                    "failed to create transport for MCP server `{}`",
                    config.name
                )
            })?);
        let epoch_gate = Arc::new(RwLock::new(0));
        let serial_gate =
            (config.transport != McpTransport::Stdio).then(|| Arc::new(Mutex::new(())));

        // Era probe (`server/discover`). Modern peers skip initialize and
        // speak `_meta` / standard POST headers; Legacy peers keep
        // initialize and negotiate the peer date from the handshake result.
        let opened = open_session(transport.as_ref(), &config.name, 0).await?;
        let capabilities = opened.capabilities;
        let peer = opened.peer;

        let (list_id, next_id) = match peer.era {
            PeerEra::Legacy => (2u64, 3u64),
            PeerEra::Modern => (1u64, 2u64),
        };
        let list_params = match peer.era {
            PeerEra::Modern => attach_request_meta(json!({}), &peer.version),
            PeerEra::Legacy => json!({}),
        };
        let list_req = JsonRpcRequest::new(list_id, "tools/list", list_params);

        let list_lifecycle = McpRequestLifecycle::uncoordinated_for_peer(0, &peer);
        let list_resp = timeout(
            Duration::from_secs(RECV_TIMEOUT_SECS),
            transport.send_and_recv(&list_req, &list_lifecycle),
        )
        .await
        .with_context(|| {
            format!(
                "MCP server `{}` timed out after {}s waiting for tools/list response",
                config.name, RECV_TIMEOUT_SECS
            )
        })??;

        if let Some(err) = &list_resp.error {
            bail!(
                "tools/list from `{}` error {}: {}",
                config.name,
                err.code,
                err.message
            );
        }

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
        let result = require_complete_result(peer.era, "tools/list", result)
            .with_context(|| format!("tools/list from `{}` rejected resultType", config.name))?;
        let tools_ttl = tools_ttl_from_list_result(peer.era, &result);
        let tool_list: McpToolsListResult = serde_json::from_value(result)
            .with_context(|| format!("failed to parse tools/list from `{}`", config.name))?;

        let tool_count = tool_list.tools.len();

        let inner = McpServerInner {
            config,
            #[cfg(target_has_atomic = "64")]
            next_id: AtomicU64::new(next_id), // Legacy: 1=initialize, 2=list; Modern: 0=discover, 1=list
            #[cfg(not(target_has_atomic = "64"))]
            next_id: AtomicU32::new(next_id as u32),
            tools: tool_list.tools,
            capabilities,
            peer,
            list_caches: ListCaches::default(),
            tools_ttl,
            tasks: McpTaskStore::new(),
        };

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
                ::serde_json::json!({
                    "mcp_server": &inner.config.name,
                    "tool_count": tool_count,
                    "protocol_version": &inner.peer.version,
                    "era": match inner.peer.era {
                        PeerEra::Legacy => "legacy",
                        PeerEra::Modern => "modern",
                    },
                })
            ),
            &format!(
                "MCP server `{}` connected — {} tool(s) available",
                inner.config.name, tool_count
            )
        );

        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
            transport,
            epoch_gate,
            serial_gate,
            recovery: Arc::new(RecoveryBarrier::new()),
        })
    }

    /// Tools advertised by this server.
    pub async fn tools(&self) -> Vec<McpToolDef> {
        {
            let inner = self.inner.lock().await;
            if !tools_list_stale(&inner) {
                return inner.tools.clone();
            }
        }
        match self.refresh_tools_list().await {
            Ok(tools) => tools,
            Err(_) => self.inner.lock().await.tools.clone(),
        }
    }

    async fn refresh_tools_list(&self) -> Result<Vec<McpToolDef>> {
        let raw = self.dispatch_method("tools/list", json!({})).await?;
        let parsed: McpToolsListResult =
            serde_json::from_value(raw.clone()).context("failed to parse tools/list result")?;
        let mut inner = self.inner.lock().await;
        inner.tools_ttl = tools_ttl_from_list_result(inner.peer.era, &raw);
        inner.tools = parsed.tools;
        Ok(inner.tools.clone())
    }

    /// Server display name.
    pub async fn name(&self) -> String {
        self.inner.lock().await.config.name.clone()
    }

    /// Server-advertised capabilities captured at handshake.
    pub async fn capabilities(&self) -> McpServerCapabilities {
        self.inner.lock().await.capabilities.clone()
    }

    /// Era resolved from the `server/discover` probe (or the legacy fallback).
    pub async fn peer_era(&self) -> PeerEra {
        self.inner.lock().await.peer.era
    }

    /// Protocol version this client will speak to the peer.
    pub async fn peer_protocol_version(&self) -> String {
        self.inner.lock().await.peer.version.clone()
    }

    /// Health-check the underlying transport without sending a real request.
    /// Returns `true` when the transport is alive, `false` otherwise.
    ///
    /// This reads transport-owned atomic connection state and does not acquire
    /// the async server metadata lock.
    pub fn health_check(&self) -> bool {
        self.transport.health_check()
    }

    /// Identity comparison on the underlying transport handle. Two
    /// `McpServer` values share the same connection iff `ptr_eq`
    /// returns `true` — i.e. their inner `Arc<Mutex<McpServerInner>>`
    /// points to the same allocation. Cheap Arc-level comparison, no
    /// async, no lock.
    ///
    /// Used by the daemon's reconciliation layer to verify that a
    /// "preserved" healthy server's live connection survives a
    /// recovery tick without being silently disconnected and
    /// respawned (the additive merge contract: a healthy handle
    /// covers its name and is reused verbatim via `Arc::clone`).
    pub fn ptr_eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.inner, &other.inner)
    }

    async fn send_request(
        &self,
        request: &JsonRpcRequest,
        lifecycle: &McpRequestLifecycle,
    ) -> Result<crate::mcp_protocol::JsonRpcResponse> {
        loop {
            // Fail closed / wait for recovery before touching the write path. A
            // request whose outcome became unknown publishes the recovery-needed
            // state synchronously, so any writer that reaches here after that point
            // must not POST/write on the ambiguous session until reset +
            // re-handshake succeed.
            self.wait_recovery_ready().await?;
            let serial_guard = match &self.serial_gate {
                Some(gate) => Some(gate.lock().await),
                None => None,
            };
            // Re-check under the serial gate. A concurrent HTTP/SSE writer may have
            // been queued on this gate when the outcome-unknown state was armed;
            // acquiring the gate serializes us behind it, so this second check
            // guarantees we never write on a session that still needs recovery.
            if self.recovery.is_poisoned() || self.recovery.recovery_pending() {
                drop(serial_guard);
                continue;
            }
            // Arm the recovery barrier synchronously the instant this write's
            // outcome becomes unknown — *before* the serial gate is released.
            // Declared after `serial_guard` so it drops first: the next queued
            // writer therefore observes the armed barrier under the gate and waits,
            // instead of racing onto the ambiguous session.
            let barrier_arm = WriteBarrierArm {
                recovery: &self.recovery,
                lifecycle,
            };
            let result = self.transport.send_and_recv(request, lifecycle).await;
            drop(barrier_arm);
            drop(serial_guard);

            if matches!(
                result
                    .as_ref()
                    .err()
                    .and_then(|error| error.downcast_ref::<McpTransportError>()),
                Some(McpTransportError::RecoveryPending)
            ) {
                continue;
            }
            return result;
        }
    }

    /// Block until no recovery is pending, or fail closed if recovery has
    /// permanently failed. Returns immediately when the connection is healthy.
    async fn wait_recovery_ready(&self) -> Result<()> {
        loop {
            if self.recovery.is_poisoned() {
                let server_name = self.inner.lock().await.config.name.clone();
                bail!(
                    "MCP server `{server_name}` is unavailable: a prior request's outcome became \
                     unknown and recovery failed; not writing on an unrecovered session"
                );
            }
            if !self.recovery.recovery_pending() {
                return Ok(());
            }
            // Register for a wakeup *before* re-checking so we cannot miss a
            // concurrent `finish`/`poison` pulse.
            let notified = self.recovery.notify.notified();
            if self.recovery.is_poisoned() {
                continue;
            }
            if !self.recovery.recovery_pending() {
                return Ok(());
            }
            notified.await;
        }
    }

    fn start_recovery(
        &self,
        observed_epoch: u64,
        operation: String,
    ) -> tokio::task::JoinHandle<Result<()>> {
        let server = self.clone();
        zeroclaw_spawn::spawn!(async move {
            let result = server.reestablish(observed_epoch).await;
            if let Err(error) = &result {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reconnect)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "operation": operation,
                            "error": format!("{error:#}"),
                        })),
                    "mcp_client: asynchronous recovery failed after outcome-unknown request"
                );
            }
            result
        })
    }

    fn spawn_recovery(&self, observed_epoch: u64, operation: String) {
        // Publish the recovery-needed state synchronously so any writer that
        // queues after this point waits for reset + re-handshake instead of
        // racing onto the ambiguous session. This runs before the detached
        // recovery task is scheduled and before the failing request releases
        // its locks.
        self.recovery.arm(observed_epoch);
        // Dropping a Tokio JoinHandle detaches the task. Recovery therefore
        // continues even if the request future that initiated it is cancelled.
        drop(self.start_recovery(observed_epoch, operation));
    }

    async fn reestablish(&self, observed_epoch: u64) -> Result<()> {
        // Keep HTTP/SSE reset ordering consistent with ordinary calls:
        // serial gate first, then the epoch write gate.
        let serial_guard = match &self.serial_gate {
            Some(gate) => Some(gate.lock().await),
            None => None,
        };
        let mut epoch = self.epoch_gate.write().await;
        if *epoch != observed_epoch {
            // Another recovery already advanced past this epoch; the connection
            // is live again, so release any writers still waiting on the
            // barrier for this (or an older) epoch.
            self.recovery.finish(observed_epoch);
            return Ok(());
        }
        let server_name = self.inner.lock().await.config.name.clone();

        if let Err(reset_error) = self.transport.reset().await {
            // A failed reset leaves the session unrecoverable: fail closed so
            // later writes do not proceed without a successful handshake.
            self.recovery.poison();
            let close_result = self.transport.close().await;
            return match close_result {
                Ok(()) => Err(reset_error).with_context(|| {
                    format!("MCP server `{server_name}` failed to reset transport during recovery")
                }),
                Err(close_error) => Err(anyhow::Error::msg(format!(
                    "MCP server `{server_name}` failed to reset transport during recovery: \
                     {reset_error:#}; cleanup also failed: {close_error:#}"
                ))),
            };
        }

        let era = self.inner.lock().await.peer.era;
        if era == PeerEra::Modern {
            // Modern peers are stateless: a transport reset is the recovery.
            // Sending `initialize` would be a Legacy-arm byte and a protocol
            // error on a 2026-07-28 server.
            *epoch = epoch.wrapping_add(1);
            self.recovery.finish(observed_epoch);
            drop(serial_guard);
            return Ok(());
        }

        let refreshed = match handshake(self.transport.as_ref(), &server_name, *epoch).await {
            Ok((capabilities, mut peer)) => {
                peer.era = era;
                self.inner.lock().await.peer = peer;
                capabilities
            }
            Err(handshake_error) => {
                // A failed re-handshake leaves the connection without a live
                // MCP session; poison the barrier so later tool calls fail
                // closed instead of writing on an unhandshaken transport.
                self.recovery.poison();
                let close_result = self.transport.close().await;
                return match close_result {
                    Ok(()) => Err(handshake_error).with_context(|| {
                        format!("MCP server `{server_name}` failed to re-handshake during recovery")
                    }),
                    Err(close_error) => Err(anyhow::Error::msg(format!(
                        "MCP server `{server_name}` failed to re-handshake during recovery: \
                         {handshake_error:#}; cleanup also failed: {close_error:#}"
                    ))),
                };
            }
        };

        self.inner.lock().await.capabilities = refreshed;
        *epoch = epoch.wrapping_add(1);
        // Reset + re-handshake succeeded: clear the recovery-needed state and
        // release any writers waiting on the barrier.
        self.recovery.finish(observed_epoch);
        drop(serial_guard);
        Ok(())
    }

    async fn dispatch_rpc(
        &self,
        rpc_method: &str,
        params: serde_json::Value,
        timeout_secs: u64,
        operation: &str,
    ) -> Result<crate::mcp_protocol::JsonRpcResponse> {
        self.dispatch_rpc_until(
            rpc_method,
            params,
            Instant::now() + Duration::from_secs(timeout_secs),
            timeout_secs,
            operation,
        )
        .await
    }

    async fn dispatch_rpc_until(
        &self,
        rpc_method: &str,
        params: serde_json::Value,
        deadline: Instant,
        timeout_secs: u64,
        operation: &str,
    ) -> Result<crate::mcp_protocol::JsonRpcResponse> {
        let mut pre_write_retries = 0;

        loop {
            let (id, server_name, peer) = {
                let inner = self.inner.lock().await;
                (
                    inner.next_id.fetch_add(1, Ordering::Relaxed),
                    inner.config.name.clone(),
                    inner.peer.clone(),
                )
            };
            let params = match peer.era {
                PeerEra::Modern => attach_request_meta(params.clone(), &peer.version),
                PeerEra::Legacy => params.clone(),
            };
            let request = JsonRpcRequest::new(id, rpc_method, params);
            let recovery_gate: Arc<dyn McpRecoveryGate> = self.recovery.clone();
            let lifecycle = Arc::new(McpRequestLifecycle::coordinated(
                Arc::clone(&self.epoch_gate),
                Some(recovery_gate),
                &peer,
            ));
            let mut cancellation_guard = OutcomeUnknownGuard::new(
                self.clone(),
                Arc::clone(&lifecycle),
                operation.to_string(),
            );

            let send_result = timeout_at(deadline, self.send_request(&request, &lifecycle)).await;
            match send_result {
                Err(_) => {
                    let unknown_epoch = lifecycle.outcome_unknown_epoch();
                    cancellation_guard.disarm();
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Timeout)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "mcp_server": &server_name,
                                "rpc_method": rpc_method,
                                "timeout_secs": timeout_secs,
                                "outcome_unknown": unknown_epoch.is_some(),
                            })),
                        "mcp_client: MCP request timed out"
                    );
                    if let Some(epoch) = unknown_epoch {
                        self.spawn_recovery(epoch, operation.to_string());
                        bail!(
                            "MCP server `{server_name}` timed out after {timeout_secs}s during \
                             {operation}; outcome unknown and request was not replayed"
                        );
                    }
                    bail!(
                        "MCP server `{server_name}` timed out after {timeout_secs}s before writing \
                         {operation}"
                    );
                }
                Ok(Ok(response)) => {
                    cancellation_guard.disarm();
                    return Ok(response);
                }
                Ok(Err(error)) => {
                    if let Some(epoch) = lifecycle.outcome_unknown_epoch() {
                        cancellation_guard.disarm();
                        self.spawn_recovery(epoch, operation.to_string());
                        return Err(error).with_context(|| {
                            format!(
                                "MCP server `{server_name}` failed during {operation}; outcome \
                                 unknown and request was not replayed"
                            )
                        });
                    }

                    cancellation_guard.disarm();
                    let recoverable = error.downcast_ref::<McpTransportError>().is_some();
                    if recoverable && pre_write_retries < MAX_RECONNECT_ATTEMPTS {
                        pre_write_retries += 1;
                        let observed_epoch = lifecycle.pre_write_epoch().unwrap_or(0);
                        let recovery = self.start_recovery(observed_epoch, operation.to_string());
                        match timeout_at(deadline, recovery).await {
                            Ok(Ok(result)) => result?,
                            Ok(Err(join_error)) => {
                                return Err(anyhow::Error::new(join_error)).with_context(|| {
                                    format!(
                                        "MCP server `{server_name}` recovery task failed before \
                                         writing {operation}"
                                    )
                                });
                            }
                            Err(_) => {
                                bail!(
                                    "MCP server `{server_name}` exhausted the {timeout_secs}s \
                                     budget recovering before writing {operation}"
                                );
                            }
                        }
                        continue;
                    }
                    return Err(error).with_context(|| {
                        format!("MCP server `{server_name}` error during {operation}")
                    });
                }
            }
        }
    }

    /// Call a tool on this server. Returns the raw JSON result.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let tool_timeout = {
            let inner = self.inner.lock().await;
            inner
                .config
                .tool_timeout_secs
                .unwrap_or(DEFAULT_TOOL_TIMEOUT_SECS)
                .min(MAX_TOOL_TIMEOUT_SECS)
        };
        let operation = format!("tool call `{tool_name}`");
        let era = {
            let inner = self.inner.lock().await;
            inner.peer.era
        };
        if era == PeerEra::Modern
            && let Some(continuation) = parse_continuation(&arguments)?
        {
            return self
                .continue_pending_task(
                    "tools/call",
                    Some(tool_name),
                    continuation,
                    tool_timeout,
                    &operation,
                )
                .await;
        }
        let params = json!({ "name": tool_name, "arguments": arguments });
        let resp = self
            .dispatch_rpc("tools/call", params.clone(), tool_timeout, &operation)
            .await?;

        if let Some(err) = resp.error {
            bail!("MCP tool `{tool_name}` error {}: {}", err.code, err.message);
        }

        let result = resp.result.unwrap_or(serde_json::Value::Null);

        // MCP servers signal *tool-execution* failures (as opposed to JSON-RPC
        // protocol errors) with HTTP 200 + `result.isError: true` and the detail
        // in `result.content[].text`, per the MCP spec. Surface it (scrubbed and
        // length-bounded) so the failure is visible to the model and the log.
        let server_name = {
            let inner = self.inner.lock().await;
            inner.config.name.clone()
        };
        let result = self
            .finalize_classified_result("tools/call", params, result)
            .await?;
        check_result_is_error(&result, tool_name, &server_name)?;

        Ok(result)
    }

    /// Generic JSON-RPC method dispatch with the same timeout, bounded
    /// reconnect, and error surfacing as `call_tool`. Returns the raw
    /// `result` value; callers apply any method-specific envelope handling.
    pub(crate) async fn dispatch_method(
        &self,
        rpc_method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let tool_timeout = {
            let inner = self.inner.lock().await;
            inner
                .config
                .tool_timeout_secs
                .unwrap_or(DEFAULT_TOOL_TIMEOUT_SECS)
                .min(MAX_TOOL_TIMEOUT_SECS)
        };
        let operation = format!("`{rpc_method}`");
        let resp = self
            .dispatch_rpc(rpc_method, params.clone(), tool_timeout, &operation)
            .await?;

        if let Some(err) = resp.error {
            bail!("MCP `{rpc_method}` error {}: {}", err.code, err.message);
        }
        let result = resp.result.unwrap_or(serde_json::Value::Null);
        let server_name = {
            let inner = self.inner.lock().await;
            inner.config.name.clone()
        };
        let result = self
            .finalize_classified_result(rpc_method, params, result)
            .await?;
        check_result_is_error(&result, rpc_method, &server_name)?;
        Ok(result)
    }

    /// Classify a JSON-RPC `result`. Complete payloads pass through.
    /// Well-formed `input_required` on `tools/call` mints an in-process
    /// handle; the same envelope on `prompts/get` / `resources/read` stays a
    /// typed error (no handle). Well-formed `resultType: "task"` on
    /// `tools/call` is mapped into the same table and polled via
    /// `tasks/get`. Malformed modern envelopes fail closed. Legacy never
    /// reaches `InputRequired` or `Task`.
    async fn finalize_classified_result(
        &self,
        method: &str,
        params: serde_json::Value,
        result: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let era = {
            let inner = self.inner.lock().await;
            inner.peer.era
        };
        match classify_mcp_result(era, method, &result) {
            Ok(McpResultKind::Complete) => Ok(result),
            Ok(McpResultKind::InputRequired(input_required)) => {
                if method != "tools/call" {
                    return Err(McpInputRequiredError {
                        method: method.to_string(),
                        input_required,
                    }
                    .into());
                }
                let pending: McpTaskPending = {
                    let mut inner = self.inner.lock().await;
                    inner.tasks.mint(method, params, input_required)?
                };
                Err(anyhow::Error::new(pending))
            }
            Ok(McpResultKind::Task(task)) => {
                self.resolve_extension_task(method, params, task).await
            }
            Err(err) => Err(anyhow::Error::msg(format!(
                "MCP `{method}` resultType rejected: {err}"
            ))),
        }
    }

    async fn resolve_extension_task(
        &self,
        method: &str,
        params: serde_json::Value,
        task: CreateTask,
    ) -> Result<serde_json::Value> {
        let (pending, timeout_secs) = {
            let mut inner = self.inner.lock().await;
            let timeout_secs = inner
                .config
                .tool_timeout_secs
                .unwrap_or(DEFAULT_TOOL_TIMEOUT_SECS)
                .min(MAX_TOOL_TIMEOUT_SECS);
            let pending =
                inner
                    .tasks
                    .mint_extension(method, params.clone(), task.task_id.clone())?;
            (pending, timeout_secs)
        };
        let mut guard =
            ExtensionHandleGuard::new(Arc::clone(&self.inner), Some(pending.handle.clone()));
        let operation = format!("`{method}` task poll");
        self.poll_extension_task(
            &task.task_id,
            method,
            params,
            &mut guard,
            timeout_secs,
            &operation,
            task.poll_interval_ms,
        )
        .await
    }

    async fn poll_extension_task(
        &self,
        task_id: &str,
        origin_method: &str,
        origin_params: serde_json::Value,
        guard: &mut ExtensionHandleGuard,
        timeout_secs: u64,
        operation: &str,
        initial_poll_interval_ms: Option<u64>,
    ) -> Result<serde_json::Value> {
        let wall = Duration::from_secs(timeout_secs).min(MAX_TASK_POLL_WALL);
        let deadline = Instant::now() + wall;
        if let Some(delay) = poll_delay(initial_poll_interval_ms) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(TaskHandleError::PollLimitExceeded.into());
            }
            tokio::time::sleep(delay.min(remaining)).await;
        }
        for poll in 0..MAX_TASK_POLLS {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(TaskHandleError::PollLimitExceeded.into());
            }
            let resp = self
                .dispatch_rpc_until(
                    "tasks/get",
                    json!({ "taskId": task_id }),
                    deadline,
                    timeout_secs,
                    operation,
                )
                .await?;
            if let Some(err) = resp.error {
                let message = redact_known_task_id(
                    &zeroclaw_providers::sanitize_api_error(&err.message),
                    task_id,
                );
                bail!("MCP `tasks/get` error {}: {message}", err.code);
            }
            let result = resp.result.unwrap_or(serde_json::Value::Null);
            let state = match parse_task_poll_result(task_id, &result) {
                Ok(state) => state,
                Err(err) => {
                    return Err(anyhow::Error::msg(format!(
                        "MCP `tasks/get` resultType rejected: {err}"
                    )));
                }
            };
            match state {
                TaskPollState::Working { poll_interval_ms } => {
                    if poll + 1 >= MAX_TASK_POLLS {
                        break;
                    }
                    if let Some(delay) = poll_delay(poll_interval_ms) {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            break;
                        }
                        tokio::time::sleep(delay.min(remaining)).await;
                    }
                }
                TaskPollState::Completed(inner) => {
                    if let Some(handle) = guard.defuse() {
                        self.inner.lock().await.tasks.discard(&handle);
                    }
                    return self
                        .consume_origin_result(origin_method, origin_params, inner)
                        .await;
                }
                TaskPollState::Failed { message } => {
                    return Err(TaskHandleError::TaskFailed { message }.into());
                }
                TaskPollState::Cancelled => {
                    return Err(TaskHandleError::TaskCancelled.into());
                }
                TaskPollState::InputRequired(input_required) => {
                    let handle = match guard.defuse() {
                        Some(handle) => handle,
                        None => {
                            let mut inner = self.inner.lock().await;
                            let pending = inner.tasks.mint_extension(
                                origin_method,
                                origin_params,
                                task_id.to_string(),
                            )?;
                            pending.handle
                        }
                    };
                    let ttl_secs = {
                        let mut inner = self.inner.lock().await;
                        inner
                            .tasks
                            .bind_input_required(&handle, input_required.clone())?;
                        inner.tasks.ttl_secs()
                    };
                    return Err(anyhow::Error::new(McpTaskPending {
                        handle,
                        method: origin_method.to_string(),
                        input_required,
                        ttl_secs,
                    }));
                }
            }
        }
        Err(TaskHandleError::PollLimitExceeded.into())
    }

    async fn consume_origin_result(
        &self,
        method: &str,
        _params: serde_json::Value,
        result: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let era = {
            let inner = self.inner.lock().await;
            inner.peer.era
        };
        match classify_mcp_result(era, method, &result) {
            Ok(McpResultKind::Complete) => Ok(result),
            Ok(McpResultKind::InputRequired(_)) => Err(ResultTypeError::NestedInputRequired.into()),
            Ok(McpResultKind::Task(_)) => Err(ResultTypeError::NestedTask.into()),
            Err(err) => Err(anyhow::Error::msg(format!(
                "MCP `{method}` resultType rejected: {err}"
            ))),
        }
    }

    /// Redeem a handle and retry the original request with MRTR fields.
    async fn continue_pending_task(
        &self,
        expected_method: &str,
        expected_binding: Option<&str>,
        continuation: TaskContinuation,
        timeout_secs: u64,
        operation: &str,
    ) -> Result<serde_json::Value> {
        let redeemed = {
            let mut inner = self.inner.lock().await;
            inner
                .tasks
                .redeem(&continuation.handle, expected_method, expected_binding)?
        };
        require_responses_if_needed(&redeemed.input_required, &continuation.input_responses)?;
        let original_params = redeemed.params.clone();
        if let Some(task_id) = redeemed.extension_task_id {
            let mut update_params = json!({ "taskId": task_id });
            if let Some(responses) = continuation.input_responses
                && let serde_json::Value::Object(map) = &mut update_params
            {
                map.insert("inputResponses".to_string(), responses);
            }
            let resp = self
                .dispatch_rpc("tasks/update", update_params, timeout_secs, operation)
                .await?;
            if let Some(err) = resp.error {
                let message = redact_known_task_id(
                    &zeroclaw_providers::sanitize_api_error(&err.message),
                    &task_id,
                );
                bail!("MCP `tasks/update` error {}: {message}", err.code);
            }
            let ack = resp.result.unwrap_or(serde_json::Value::Null);
            require_complete_result(PeerEra::Modern, "tasks/update", ack)?;
            let mut guard = ExtensionHandleGuard::new(Arc::clone(&self.inner), None);
            return self
                .poll_extension_task(
                    &task_id,
                    &redeemed.method,
                    original_params,
                    &mut guard,
                    timeout_secs,
                    operation,
                    None,
                )
                .await;
        }
        let retry_params = attach_input_retry(
            original_params.clone(),
            continuation.input_responses.as_ref(),
            redeemed.input_required.request_state.as_deref(),
        );
        let resp = self
            .dispatch_rpc(&redeemed.method, retry_params, timeout_secs, operation)
            .await?;
        if let Some(err) = resp.error {
            bail!(
                "MCP `{}` error {}: {}",
                redeemed.method,
                err.code,
                err.message
            );
        }
        let result = resp.result.unwrap_or(serde_json::Value::Null);
        let server_name = {
            let inner = self.inner.lock().await;
            inner.config.name.clone()
        };
        let result = self
            .finalize_classified_result(&redeemed.method, original_params, result)
            .await?;
        check_result_is_error(
            &result,
            expected_binding.unwrap_or(&redeemed.method),
            &server_name,
        )?;
        Ok(result)
    }

    /// `resources/list` — capability-gated.
    pub async fn list_resources(&self, cursor: Option<String>) -> Result<McpResourcesListResult> {
        {
            let inner = self.inner.lock().await;
            if !inner.capabilities.supports_resources() {
                bail!(
                    "MCP server `{}` does not support resources",
                    inner.config.name
                );
            }
            if inner.peer.era == PeerEra::Modern
                && let Some(cached) = cached_list(&inner.list_caches.resources, &cursor)
            {
                return Ok(cached);
            }
        }
        let cursor_key = cursor.clone();
        let params = match cursor {
            Some(c) => json!({ "cursor": c }),
            None => json!({}),
        };
        let raw = self.dispatch_method("resources/list", params).await?;
        let parsed: McpResourcesListResult =
            serde_json::from_value(raw.clone()).context("failed to parse resources/list result")?;
        {
            let mut inner = self.inner.lock().await;
            if inner.peer.era == PeerEra::Modern {
                store_list_cache(
                    &mut inner.list_caches.resources,
                    cursor_key,
                    parsed.clone(),
                    &raw,
                );
            }
        }
        Ok(parsed)
    }

    /// `resources/read` — capability-gated.
    pub async fn read_resource(&self, uri: &str) -> Result<McpResourceContents> {
        {
            let inner = self.inner.lock().await;
            if !inner.capabilities.supports_resources() {
                bail!(
                    "MCP server `{}` does not support resources",
                    inner.config.name
                );
            }
        }
        let raw = self
            .dispatch_method("resources/read", json!({ "uri": uri }))
            .await?;
        serde_json::from_value(raw).context("failed to parse resources/read result")
    }

    /// `prompts/list` — capability-gated.
    pub async fn list_prompts(&self, cursor: Option<String>) -> Result<McpPromptsListResult> {
        {
            let inner = self.inner.lock().await;
            if !inner.capabilities.supports_prompts() {
                bail!(
                    "MCP server `{}` does not support prompts",
                    inner.config.name
                );
            }
            if inner.peer.era == PeerEra::Modern
                && let Some(cached) = cached_list(&inner.list_caches.prompts, &cursor)
            {
                return Ok(cached);
            }
        }
        let cursor_key = cursor.clone();
        let params = match cursor {
            Some(c) => json!({ "cursor": c }),
            None => json!({}),
        };
        let raw = self.dispatch_method("prompts/list", params).await?;
        let parsed: McpPromptsListResult =
            serde_json::from_value(raw.clone()).context("failed to parse prompts/list result")?;
        {
            let mut inner = self.inner.lock().await;
            if inner.peer.era == PeerEra::Modern {
                store_list_cache(
                    &mut inner.list_caches.prompts,
                    cursor_key,
                    parsed.clone(),
                    &raw,
                );
            }
        }
        Ok(parsed)
    }

    /// `prompts/get` — capability-gated.
    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<McpGetPromptResult> {
        {
            let inner = self.inner.lock().await;
            if !inner.capabilities.supports_prompts() {
                bail!(
                    "MCP server `{}` does not support prompts",
                    inner.config.name
                );
            }
        }
        let raw = self
            .dispatch_method(
                "prompts/get",
                json!({ "name": name, "arguments": arguments }),
            )
            .await?;
        serde_json::from_value(raw).context("failed to parse prompts/get result")
    }
}

// ── McpRegistry ───────────────────────────────────────────────────────────

/// Registry of all connected MCP servers, with a flat tool index.
pub struct McpRegistry {
    servers: Vec<McpServer>,
    /// prefixed_name → (server_index, original_tool_name)
    tool_index: HashMap<String, (usize, String)>,
    /// server name → index in `servers`.
    server_index: HashMap<String, usize>,
}

impl McpRegistry {
    /// Connect to all configured servers. Non-fatal: failures are logged and skipped.
    pub async fn connect_all(configs: &[McpServerConfig]) -> Result<Self> {
        let mut servers = Vec::new();
        let mut tool_index = HashMap::new();
        let mut server_index = HashMap::new();

        for config in configs {
            match McpServer::connect(config.clone()).await {
                Ok(server) => {
                    let server_idx = servers.len();
                    server_index.insert(config.name.clone(), server_idx);
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
            server_index,
        })
    }

    /// Build a registry with `n` placeholder servers, each backed by a no-op
    /// transport. The server names are `stub_0`, `stub_1`, ..., `stub_{n-1}`.
    ///
    /// Test-only: gated behind the `test-helpers` feature so it is NOT
    /// available in production builds. Downstream test suites (e.g.
    /// `zeroclaw-runtime::daemon`) enable the feature via their
    /// `[dev-dependencies]` declaration and use this helper to build an
    /// `Arc<McpRegistry>` whose `server_count() == n` without spawning a
    /// real stdio child, so unit tests can exercise
    /// "registry-completeness" decisions purely on `server_count()`. The
    /// transport is a local no-op so the registry is safe to drop in tests
    /// without leaking any OS resources — but it MUST NOT be used in
    /// production code: any real MCP tool call on the resulting registry
    /// will panic in the `unreachable!()` branch.
    #[cfg(feature = "test-helpers")]
    pub fn for_test_with_server_count(n: usize) -> Self {
        use crate::mcp_protocol::JsonRpcResponse;
        use async_trait::async_trait;

        /// No-op transport: never contacted in the daemon-side tests that
        /// exercise `server_count`-driven decisions. Returning `Err` would
        /// panic any caller that actually tries to use the registry; the
        /// daemon tests only read `server_count` and compare Arc pointers,
        /// so the unreachable body is acceptable.
        struct NoopTransport;

        #[async_trait]
        impl SharedMcpTransportConn for NoopTransport {
            async fn send_and_recv(
                &self,
                _request: &JsonRpcRequest,
                _lifecycle: &McpRequestLifecycle,
            ) -> Result<JsonRpcResponse> {
                unreachable!(
                    "for_test_with_server_count registry is only used for server_count/Arc equality"
                )
            }

            async fn close(&self) -> Result<()> {
                Ok(())
            }
        }

        fn stub_server(name: &str) -> McpServer {
            let transport: Arc<dyn SharedMcpTransportConn> = Arc::new(NoopTransport);
            let inner = McpServerInner {
                config: McpServerConfig {
                    name: name.to_string(),
                    ..McpServerConfig::default()
                },
                #[cfg(target_has_atomic = "64")]
                next_id: AtomicU64::new(0),
                #[cfg(not(target_has_atomic = "64"))]
                next_id: AtomicU32::new(0),
                tools: Vec::new(),
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

        let mut servers = Vec::with_capacity(n);
        let tool_index: HashMap<String, (usize, String)> = HashMap::new();
        let mut server_index = HashMap::new();
        for i in 0..n {
            let name = format!("stub_{i}");
            let server_idx = servers.len();
            server_index.insert(name.clone(), server_idx);
            servers.push(stub_server(&name));
        }
        Self {
            servers,
            tool_index,
            server_index,
        }
    }

    /// Snapshot the live `(server_name, McpServer)` pairs registered
    /// in this registry. Returned pairs are sorted by `server_name`
    /// for deterministic ordering across ticks. Each `McpServer` is
    /// a cheap `Arc` clone of the registered handle — re-inserting it
    /// into another `McpRegistry` shares the underlying transport
    /// (no disconnect, no new stdio child).
    ///
    /// Used by the daemon heartbeat's additive reconciliation layer
    /// to preserve a healthy live connection across recovery ticks:
    /// when `current` has a healthy server whose
    /// identity still matches `fresh`, the daemon re-uses that
    /// handle instead of forcing `connect_all` to spawn a duplicate
    /// stdio child for the same endpoint.
    pub fn server_handles(&self) -> Vec<(String, McpServer)> {
        let mut out: Vec<(String, McpServer)> = self
            .server_index
            .iter()
            .map(|(name, &idx)| (name.clone(), self.servers[idx].clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Build a new registry from a list of pre-existing `McpServer`
    /// handles. The handles are cheaply Arc-cloned; transports
    /// remain alive across the move. The internal `tool_index` and
    /// `server_index` are rebuilt from each handle's advertised
    /// capabilities (synchronous `tools()` call).
    ///
    /// Companion to [`Self::server_handles`]: callers wanting to
    /// carry a healthy live connection into a fresh registry (the
    /// additive recovery path) read the handle via `server_handles`
    /// and rebuild via `from_servers`.
    pub async fn from_servers(servers: Vec<McpServer>) -> Self {
        let mut tool_index: HashMap<String, (usize, String)> = HashMap::new();
        let mut server_index: HashMap<String, usize> = HashMap::with_capacity(servers.len());
        for (idx, server) in servers.iter().enumerate() {
            let name = server.name().await;
            let tools = server.tools().await;
            for tool in &tools {
                let prefixed = format!("{}__{}", name, tool.name);
                tool_index.insert(prefixed, (idx, tool.name.clone()));
            }
            server_index.insert(name, idx);
        }
        Self {
            servers,
            tool_index,
            server_index,
        }
    }

    /// Test-only: build a registry from pre-existing `(name, handle)`
    /// pairs. Used by regression tests that need to assert the
    /// daemon's reconciliation layer preserves a healthy server's
    /// `McpServer` identity (cheap Arc pointer) across a recovery
    /// tick. The handles are not re-validated — caller is responsible
    /// for ensuring each `McpServer`'s `inner.config.name` matches the
    /// paired name.
    ///
    /// Tool index is left empty: callers in regression tests
    /// exercise identity / `server_count` / `server_names` only,
    /// never tool lookup. Use [`Self::from_servers`] in production
    /// paths where tool routing must remain valid.
    #[cfg(feature = "test-helpers")]
    pub fn for_test_with_server_handles(handles: Vec<(String, McpServer)>) -> Self {
        let mut servers: Vec<McpServer> = Vec::with_capacity(handles.len());
        let tool_index: HashMap<String, (usize, String)> = HashMap::new();
        let mut server_index: HashMap<String, usize> = HashMap::with_capacity(handles.len());
        for (idx, (name, server)) in handles.into_iter().enumerate() {
            server_index.insert(name, idx);
            servers.push(server);
        }
        Self {
            servers,
            tool_index,
            server_index,
        }
    }

    /// Test-only: build a single stub `McpServer` handle with the
    /// given `name`. The transport is a no-op (any actual call
    /// panics in `unreachable!()`); only safe to use in regression
    /// tests that exercise identity (`ptr_eq`) / `server_count` /
    /// `server_names` / `health_check_all` and never make a real
    /// tool call.
    ///
    /// Used to construct test registries where two registry builders
    /// share a server handle — e.g. a healthy A handle that must
    /// survive across a recovery tick into a freshly-merged registry.
    #[cfg(feature = "test-helpers")]
    pub fn for_test_make_stub_server(name: &str) -> McpServer {
        use crate::mcp_protocol::JsonRpcResponse;
        use async_trait::async_trait;

        struct NoopTransport;

        #[async_trait]
        impl SharedMcpTransportConn for NoopTransport {
            async fn send_and_recv(
                &self,
                _request: &JsonRpcRequest,
                _lifecycle: &McpRequestLifecycle,
            ) -> Result<JsonRpcResponse> {
                unreachable!(
                    "for_test_make_stub_server is only used for identity / \
                     ptr_eq / server_count assertions — never for actual tool calls"
                )
            }

            async fn close(&self) -> Result<()> {
                Ok(())
            }
        }

        let transport: Arc<dyn SharedMcpTransportConn> = Arc::new(NoopTransport);
        let inner = McpServerInner {
            config: McpServerConfig {
                name: name.to_string(),
                ..McpServerConfig::default()
            },
            #[cfg(target_has_atomic = "64")]
            next_id: AtomicU64::new(0),
            #[cfg(not(target_has_atomic = "64"))]
            next_id: AtomicU32::new(0),
            tools: Vec::new(),
            capabilities: McpServerCapabilities::default(),
            peer: PeerProtocol::legacy_default(),
            list_caches: ListCaches::default(),
            tools_ttl: ToolsTtl::Sticky,
            tasks: McpTaskStore::new(),
        };
        McpServer {
            inner: std::sync::Arc::new(Mutex::new(inner)),
            transport,
            epoch_gate: Arc::new(RwLock::new(0)),
            serial_gate: None,
            recovery: Arc::new(RecoveryBarrier::new()),
        }
    }

    /// All prefixed tool names across all connected servers.
    pub fn tool_names(&self) -> Vec<String> {
        self.tool_index.keys().cloned().collect()
    }

    /// Tool definition for a given prefixed name (cloned).
    pub async fn get_tool_def(&self, prefixed_name: &str) -> Option<McpToolDef> {
        let (server_idx, original_name) = self.tool_index.get(prefixed_name)?;
        let inner = self.servers[*server_idx].inner.lock().await;
        inner
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

    /// Names of all connected servers.
    pub fn server_names(&self) -> Vec<String> {
        self.server_index.keys().cloned().collect()
    }

    /// Check health of every connected server. Returns names of dead servers.
    pub fn health_check_all(&self) -> Vec<String> {
        let names = self.server_names();
        let mut dead = Vec::new();
        for name in names {
            if let Some(&idx) = self.server_index.get(&name)
                && !self.servers[idx].health_check()
            {
                dead.push(name);
            }
        }
        dead
    }

    /// Remove servers whose transport is dead.
    /// Returns the names of servers that were removed.
    ///
    /// This is a no-op for the `for_test_with_server_count` registries used in
    /// unit tests — those have no-op transports that always report alive, so
    /// no server will ever be removed.
    pub async fn kill_dead_connections(&mut self) -> Vec<String> {
        let dead = self.health_check_all();
        if dead.is_empty() {
            return dead;
        }

        // Rebuild the registry without dead servers, updating indices
        // so tool_index references remain valid.
        let dead_set: std::collections::HashSet<_> = dead.iter().cloned().collect();

        let mut new_servers = Vec::with_capacity(self.servers.len());
        let mut new_server_index = HashMap::with_capacity(self.server_index.len());
        let mut old_to_new_idx = HashMap::with_capacity(self.server_index.len());

        for (name, &old_idx) in &self.server_index {
            if dead_set.contains(name) {
                continue;
            }
            let new_idx = new_servers.len();
            new_servers.push(self.servers[old_idx].clone());
            old_to_new_idx.insert(old_idx, new_idx);
            new_server_index.insert(name.clone(), new_idx);
        }

        let mut new_tool_index = HashMap::with_capacity(self.tool_index.len());
        for (prefixed, (old_srv_idx, tool_name)) in &self.tool_index {
            if let Some(&new_srv_idx) = old_to_new_idx.get(old_srv_idx) {
                new_tool_index.insert(prefixed.clone(), (new_srv_idx, tool_name.clone()));
            }
        }

        self.servers = new_servers;
        self.server_index = new_server_index;
        self.tool_index = new_tool_index;

        dead
    }

    /// Split a `<server>__<rest>` prefixed name. Returns None if no prefix.
    pub fn split_prefixed(prefixed: &str) -> Option<(String, String)> {
        prefixed
            .split_once("__")
            .map(|(s, r)| (s.to_string(), r.to_string()))
    }

    fn server_by_name(&self, name: &str) -> Option<&McpServer> {
        self.server_index.get(name).map(|i| &self.servers[*i])
    }

    /// Whether the named server advertised resource capability.
    pub async fn server_supports_resources(&self, name: &str) -> bool {
        match self.server_by_name(name) {
            Some(srv) => srv.capabilities().await.supports_resources(),
            None => false,
        }
    }

    /// Whether the named server advertised prompt capability.
    pub async fn server_supports_prompts(&self, name: &str) -> bool {
        match self.server_by_name(name) {
            Some(srv) => srv.capabilities().await.supports_prompts(),
            None => false,
        }
    }

    /// Read a resource by prefixed uri (`<server>__<uri>`).
    pub async fn read_resource(
        &self,
        prefixed_uri: &str,
    ) -> Result<crate::mcp_resource::McpResourceContents> {
        let (server, uri) = Self::split_prefixed(prefixed_uri).ok_or_else(|| {
            anyhow::Error::msg(format!("missing server prefix in `{prefixed_uri}`"))
        })?;
        let srv = self
            .server_by_name(&server)
            .ok_or_else(|| anyhow::Error::msg(format!("unknown MCP server `{server}`")))?;
        srv.read_resource(&uri).await
    }

    /// Get a prompt by prefixed name (`<server>__<name>`).
    pub async fn get_prompt(
        &self,
        prefixed_name: &str,
        arguments: serde_json::Value,
    ) -> Result<crate::mcp_prompt::McpGetPromptResult> {
        let (server, name) = Self::split_prefixed(prefixed_name).ok_or_else(|| {
            anyhow::Error::msg(format!("missing server prefix in `{prefixed_name}`"))
        })?;
        let srv = self
            .server_by_name(&server)
            .ok_or_else(|| anyhow::Error::msg(format!("unknown MCP server `{server}`")))?;
        srv.get_prompt(&name, arguments).await
    }

    /// List one server's resources with optional pagination cursor. Returns the
    /// prefixed defs and the server's `next_cursor` (if any). The `cursor` is the
    /// opaque token from a prior page's `next_cursor` for this same server.
    pub async fn list_server_resources(
        &self,
        server: &str,
        cursor: Option<String>,
    ) -> Result<(Vec<crate::mcp_resource::McpResourceDef>, Option<String>)> {
        let srv = self
            .server_by_name(server)
            .ok_or_else(|| anyhow::Error::msg(format!("unknown MCP server `{server}`")))?;
        let list = srv.list_resources(cursor).await?;
        let next = list.next_cursor.clone();
        let defs = list
            .resources
            .into_iter()
            .map(|mut def| {
                def.uri = format!("{server}__{}", def.uri);
                def
            })
            .collect();
        Ok((defs, next))
    }

    /// List one server's prompts with optional pagination cursor. Returns the
    /// prefixed defs and the server's `next_cursor` (if any).
    pub async fn list_server_prompts(
        &self,
        server: &str,
        cursor: Option<String>,
    ) -> Result<(Vec<crate::mcp_prompt::McpPromptDef>, Option<String>)> {
        let srv = self
            .server_by_name(server)
            .ok_or_else(|| anyhow::Error::msg(format!("unknown MCP server `{server}`")))?;
        let list = srv.list_prompts(cursor).await?;
        let next = list.next_cursor.clone();
        let defs = list
            .prompts
            .into_iter()
            .map(|mut def| {
                def.name = format!("{server}__{}", def.name);
                def
            })
            .collect();
        Ok((defs, next))
    }

    /// List resources across all servers that support them. Each entry's uri is
    /// returned prefixed with `<server>__`. Per-server errors are skipped.
    pub async fn list_all_resources(&self) -> Vec<(String, crate::mcp_resource::McpResourceDef)> {
        let mut out = Vec::new();
        for (name, idx) in &self.server_index {
            let srv = &self.servers[*idx];
            if let Ok(list) = srv.list_resources(None).await {
                for mut def in list.resources {
                    let prefixed_uri = format!("{name}__{}", def.uri);
                    def.uri = prefixed_uri.clone();
                    out.push((prefixed_uri, def));
                }
            }
        }
        out
    }

    /// List prompts across all servers that support them, prefixed by server.
    pub async fn list_all_prompts(&self) -> Vec<(String, crate::mcp_prompt::McpPromptDef)> {
        let mut out = Vec::new();
        for (name, idx) in &self.server_index {
            let srv = &self.servers[*idx];
            if let Ok(list) = srv.list_prompts(None).await {
                for mut def in list.prompts {
                    // Rewrite the def's name to the prefixed form so the value
                    // emitted by `mcp_prompts list` can be passed straight back
                    // to `mcp_prompts get` (mirrors `list_all_resources`).
                    let prefixed = format!("{name}__{}", def.name);
                    def.name = prefixed.clone();
                    out.push((prefixed, def));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests;

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
/// today's initialize wire byte-for-byte.
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
        // speak `_meta` / standard POST headers; Legacy peers keep the
        // handshake wire unchanged.
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
mod tests {
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
        let transport: Arc<dyn SharedMcpTransportConn> =
            Arc::new(CancellationSafeRecoveryTransport {
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
            error.to_string().contains("outcome unknown")
                && error.to_string().contains("not replayed"),
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
        let first = zeroclaw_spawn::spawn!(async move {
            first_server.call_tool("side_effect", json!({})).await
        });
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
        let first = zeroclaw_spawn::spawn!(async move {
            first_server.call_tool("side_effect", json!({})).await
        });
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
        let first =
            zeroclaw_spawn::spawn!(async move { first_server.call_tool("A", json!({})).await });
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
        let first = zeroclaw_spawn::spawn!(async move {
            first_server.call_tool("side_effect", json!({})).await
        });
        timeout(Duration::from_secs(3), hook.wait_for_payload_pause())
            .await
            .expect("first stdio write did not reach the post-payload pause");

        // Enter a second call before cancellation and prove it reached the
        // transport, where it is queued on the same stdio state lock.
        let second_server = server.clone();
        let second =
            zeroclaw_spawn::spawn!(
                async move { second_server.call_tool("probe", json!({})).await }
            );
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
            Some("2024-11-05")
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
            .find(|req| {
                request_json(req).get("method").and_then(|m| m.as_str()) == Some("initialize")
            })
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
                    request_json(req).get("method").and_then(|m| m.as_str())
                        == Some("server/discover")
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
            msg.contains("tools/list")
                || msg.contains("no result")
                || msg.contains("not initialized"),
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
}

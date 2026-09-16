//! MCP transport abstraction — supports stdio, SSE, and HTTP transports.

use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use parking_lot::Mutex as ParkingMutex;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify, OwnedRwLockReadGuard, RwLock, oneshot};
use tokio::time::{Duration, timeout};
use tokio_stream::StreamExt;

use crate::mcp_era::{
    MCP_METHOD_HEADER, MCP_NAME_HEADER, MCP_PROTOCOL_VERSION_HEADER, PeerEra, PeerProtocol,
    encode_mcp_header_value, mcp_name_header_source,
};
use crate::mcp_protocol::{JsonRpcRequest, JsonRpcResponse};
use zeroclaw_config::schema::{McpServerConfig, McpTransport};

/// Maximum bytes for a single JSON-RPC response.
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024; // 4 MB

/// How often the stdio child-exit watcher polls the direct child process for
/// exit. Short enough that a dead child is surfaced to health checks promptly,
/// long enough to stay negligible against idle transports.
const STDIO_CHILD_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Courtesy window granted to a stdio MCP server to exit on its own after its
/// stdin is closed (EOF), before the reaper escalates to `start_kill`. A server
/// that shuts down on EOF exits near-instantly; this only bounds how long a
/// server that ignores EOF delays teardown before being signalled.
const STDIO_CLOSE_GRACE: Duration = Duration::from_secs(2);

/// Timeout for init/list operations.
const RECV_TIMEOUT_SECS: u64 = 30;

/// Legacy default HTTP request timeout for non-tool MCP HTTP/SSE requests.
const DEFAULT_HTTP_REQUEST_TIMEOUT_SECS: u64 = 120;

/// JSON-RPC method name for MCP tool calls.
const TOOLS_CALL_METHOD: &str = "tools/call";

/// Streamable HTTP Accept header required by MCP HTTP transport.
const MCP_STREAMABLE_ACCEPT: &str = "application/json, text/event-stream";

/// Default media type for MCP JSON-RPC request bodies.
const MCP_JSON_CONTENT_TYPE: &str = "application/json";
/// Streamable HTTP session header used to preserve MCP server state.
const MCP_SESSION_ID_HEADER: &str = "Mcp-Session-Id";

fn http_request_timeout_secs(
    request: &JsonRpcRequest,
    tool_timeout_secs: Option<u64>,
) -> Option<u64> {
    if request.method == TOOLS_CALL_METHOD {
        tool_timeout_secs
    } else {
        Some(DEFAULT_HTTP_REQUEST_TIMEOUT_SECS)
    }
}

fn http_sse_read_timeout_secs(
    request: &JsonRpcRequest,
    tool_timeout_secs: Option<u64>,
) -> Option<u64> {
    if request.method == TOOLS_CALL_METHOD {
        tool_timeout_secs
    } else {
        Some(RECV_TIMEOUT_SECS)
    }
}

fn apply_request_timeout(
    req: reqwest::RequestBuilder,
    timeout_secs: Option<u64>,
) -> reqwest::RequestBuilder {
    if let Some(timeout_secs) = timeout_secs {
        req.timeout(Duration::from_secs(timeout_secs))
    } else {
        req
    }
}

/// Apply user-configured headers. Modern peers must not send `Mcp-Session-Id`
/// even when it is present in the server config map.
fn apply_configured_headers(
    mut req: reqwest::RequestBuilder,
    headers: &std::collections::HashMap<String, String>,
    era: PeerEra,
) -> reqwest::RequestBuilder {
    for (key, value) in headers {
        if era == PeerEra::Modern && key.eq_ignore_ascii_case(MCP_SESSION_ID_HEADER) {
            continue;
        }
        req = req.header(key, value);
    }
    req
}

/// Modern Streamable HTTP POST headers. `MCP-Protocol-Version` is taken from
/// the request's [`PeerProtocol`], not re-parsed from the body.
fn apply_modern_post_headers(
    mut req: reqwest::RequestBuilder,
    request: &JsonRpcRequest,
    protocol_version: &str,
) -> reqwest::RequestBuilder {
    req = req.header(MCP_PROTOCOL_VERSION_HEADER, protocol_version);
    req = req.header(MCP_METHOD_HEADER, request.method.as_str());
    if let Some(name) = mcp_name_header_source(&request.method, request.params.as_ref()) {
        req = req.header(MCP_NAME_HEADER, encode_mcp_header_value(name));
    }
    req
}

// ── Transport Errors ───────────────────────────────────────────────────────

/// Transport-level failures that require reconnecting and re-running the MCP
/// handshake. The client may retry only when the request is known not to have
/// been written; failures after a possible write are surfaced without replay.
/// Distinct from a genuine tool/application error, which is always reported
/// as-is and never retried.
#[derive(Debug, thiserror::Error)]
pub enum McpTransportError {
    /// The server no longer recognizes our session (typically after it
    /// restarted). Surfaced from HTTP 404/410 responses.
    #[error("MCP session is stale (HTTP {status})")]
    StaleSession { status: u16 },

    /// The underlying stream/connection dropped before a response arrived
    /// (e.g. SSE EOF or connection reset).
    #[error("MCP transport connection closed")]
    TransportClosed,

    /// A recovery was published after this request entered the transport but
    /// before it crossed the concrete writer boundary. The caller must wait
    /// for that recovery instead of treating this as a connection failure.
    #[error("MCP transport write blocked by pending recovery")]
    RecoveryPending,
}

const REQUEST_PRE_WRITE: u8 = 0;
const REQUEST_OUTCOME_UNKNOWN: u8 = 1;
const REQUEST_COMPLETED: u8 = 2;

/// Tracks whether a request can still be proved not to have reached the
/// server. The client uses this state to recover cancelled post-write calls
/// without replaying a possibly side-effecting operation.
pub(crate) struct McpRequestLifecycle {
    phase: AtomicU8,
    epoch: AtomicU64,
    epoch_gate: Option<Arc<RwLock<u64>>>,
    recovery_gate: Option<Arc<dyn McpRecoveryGate>>,
    fixed_epoch: u64,
    /// Spoken wire for this request. Always taken from the session's
    /// [`PeerProtocol`] (or the discover probe's modern pin) — not re-derived
    /// from the body.
    era: PeerEra,
    protocol_version: String,
}

impl McpRequestLifecycle {
    pub(crate) fn coordinated(
        epoch_gate: Arc<RwLock<u64>>,
        recovery_gate: Option<Arc<dyn McpRecoveryGate>>,
        peer: &PeerProtocol,
    ) -> Self {
        Self {
            phase: AtomicU8::new(REQUEST_PRE_WRITE),
            epoch: AtomicU64::new(0),
            epoch_gate: Some(epoch_gate),
            recovery_gate,
            fixed_epoch: 0,
            era: peer.era,
            protocol_version: peer.version.clone(),
        }
    }

    pub(crate) fn uncoordinated(epoch: u64) -> Self {
        Self::uncoordinated_for_peer(epoch, &PeerProtocol::legacy_default())
    }

    pub(crate) fn uncoordinated_for_peer(epoch: u64, peer: &PeerProtocol) -> Self {
        Self {
            phase: AtomicU8::new(REQUEST_PRE_WRITE),
            epoch: AtomicU64::new(0),
            epoch_gate: None,
            recovery_gate: None,
            fixed_epoch: epoch,
            era: peer.era,
            protocol_version: peer.version.clone(),
        }
    }

    pub(crate) fn era(&self) -> PeerEra {
        self.era
    }

    fn protocol_version(&self) -> &str {
        &self.protocol_version
    }

    async fn begin_write(&self) -> McpWritePermit {
        let permit = match &self.epoch_gate {
            Some(gate) => {
                let guard = Arc::clone(gate).read_owned().await;
                let epoch = *guard;
                McpWritePermit {
                    epoch,
                    guard: Some(guard),
                }
            }
            None => McpWritePermit {
                epoch: self.fixed_epoch,
                guard: None,
            },
        };
        self.epoch.store(permit.epoch(), Ordering::Release);
        permit
    }

    pub(crate) fn mark_outcome_unknown(&self, epoch: u64) {
        self.epoch.store(epoch, Ordering::Release);
        self.phase.store(REQUEST_OUTCOME_UNKNOWN, Ordering::Release);
    }

    fn mark_completed(&self) {
        self.phase.store(REQUEST_COMPLETED, Ordering::Release);
    }

    pub(crate) fn outcome_unknown_epoch(&self) -> Option<u64> {
        (self.phase.load(Ordering::Acquire) == REQUEST_OUTCOME_UNKNOWN)
            .then(|| self.epoch.load(Ordering::Acquire))
    }

    pub(crate) fn pre_write_epoch(&self) -> Option<u64> {
        (self.phase.load(Ordering::Acquire) == REQUEST_PRE_WRITE)
            .then(|| self.epoch.load(Ordering::Acquire))
    }

    fn check_writer_boundary(&self) -> Result<()> {
        let blocked = self
            .recovery_gate
            .as_ref()
            .is_some_and(|gate| gate.write_blocked());
        if blocked {
            return Err(McpTransportError::RecoveryPending.into());
        }
        Ok(())
    }

    fn arm_recovery_if_unknown(&self) {
        if let Some(epoch) = self.outcome_unknown_epoch()
            && let Some(gate) = &self.recovery_gate
        {
            gate.arm(epoch);
        }
    }
}

/// Coordination surface shared by the client recovery state and concrete
/// transport writer boundaries.
pub(crate) trait McpRecoveryGate: Send + Sync {
    fn arm(&self, epoch: u64);
    fn write_blocked(&self) -> bool;
}

/// Owns the concrete stdio state boundary while a write is being prepared.
///
/// Its explicit `Drop` publishes recovery before releasing `state`; this is
/// stronger than relying on local-variable drop order across an async
/// cancellation point.
struct StdioWriterBoundary<'a> {
    state: Option<tokio::sync::MutexGuard<'a, StdioState>>,
    lifecycle: &'a McpRequestLifecycle,
}

impl<'a> StdioWriterBoundary<'a> {
    fn new(
        state: tokio::sync::MutexGuard<'a, StdioState>,
        lifecycle: &'a McpRequestLifecycle,
    ) -> Result<Self> {
        lifecycle.check_writer_boundary()?;
        Ok(Self {
            state: Some(state),
            lifecycle,
        })
    }

    fn state_mut(&mut self) -> Result<&mut StdioState> {
        self.state
            .as_deref_mut()
            .ok_or_else(|| anyhow::Error::msg("stdio writer state was already released"))
    }

    fn release_state(&mut self) {
        drop(self.state.take());
    }
}

impl Drop for StdioWriterBoundary<'_> {
    fn drop(&mut self) {
        self.lifecycle.arm_recovery_if_unknown();
        self.release_state();
    }
}

struct McpWritePermit {
    epoch: u64,
    guard: Option<OwnedRwLockReadGuard<u64>>,
}

impl McpWritePermit {
    fn epoch(&self) -> u64 {
        self.epoch
    }
}

impl Drop for McpWritePermit {
    fn drop(&mut self) {
        drop(self.guard.take());
    }
}

// ── Transport Traits ─────────────────────────────────────────────────────

/// Public compatibility surface for direct transport users.
///
/// The MCP client uses the cancellation-aware shared transport trait below;
/// this mutable facade preserves the existing downstream API.
#[async_trait::async_trait]
pub trait McpTransportConn: Send + Sync {
    async fn send_and_recv(&mut self, request: &JsonRpcRequest) -> Result<JsonRpcResponse>;

    async fn reset(&mut self) -> Result<()> {
        Ok(())
    }

    fn health_check(&mut self) -> bool {
        true
    }

    async fn close(&mut self) -> Result<()>;
}

#[async_trait::async_trait]
pub(crate) trait SharedMcpTransportConn: Send + Sync {
    /// Send a JSON-RPC request and receive the response.
    async fn send_and_recv(
        &self,
        request: &JsonRpcRequest,
        lifecycle: &McpRequestLifecycle,
    ) -> Result<JsonRpcResponse>;

    /// Reset per-connection session state so the next operation re-establishes
    /// a fresh session. Default is a no-op for stateless transports (stdio).
    async fn reset(&self) -> Result<()> {
        Ok(())
    }

    /// Check whether the underlying transport is still alive without sending a
    /// real request.  The HTTP and SSE transports always return `Ok(true)` —
    /// connection drops surface through `send_and_recv` errors.  The stdio
    /// transport verifies the child process is still running via `try_wait()`.
    fn health_check(&self) -> bool {
        true
    }

    /// Close the connection.
    async fn close(&self) -> Result<()>;
}

// ── Stdio Transport ──────────────────────────────────────────────────────

type PendingMap = Arc<ParkingMutex<HashMap<(u64, u64), oneshot::Sender<JsonRpcResponse>>>>;

struct StdioPendingGuard {
    pending: PendingMap,
    key: (u64, u64),
}

impl Drop for StdioPendingGuard {
    fn drop(&mut self) {
        self.pending.lock().remove(&self.key);
    }
}

struct StdioConn {
    generation: u64,
    /// Shared so both the child-exit watcher (nonblocking `try_wait`) and the
    /// reaper (`start_kill` + `wait`) can access the direct child without a
    /// second `Child` handle.
    child: Arc<tokio::sync::Mutex<Child>>,
    stdin: tokio::process::ChildStdin,
    reader: tokio::task::JoinHandle<()>,
    /// Set to `true` by the child-exit watcher when the *direct* child process
    /// exits, independent of whether its stdout pipe has reached EOF (a
    /// descendant may keep the inherited pipe open). Health checks consult this
    /// so a dead child is never reported healthy.
    child_exited: Arc<AtomicBool>,
    /// Background task that watches the direct child for exit.
    exit_watcher: tokio::task::JoinHandle<()>,
}

impl Drop for StdioConn {
    fn drop(&mut self) {
        // Abort the background tasks so their `Arc<Mutex<Child>>` clone is
        // released. This lets the last `Child` owner drop, which — combined
        // with `kill_on_drop(true)` — reaps the direct child when a connection
        // is dropped without going through `reap_conn` (e.g. registry teardown).
        self.reader.abort();
        self.exit_watcher.abort();
    }
}

#[derive(Default)]
struct StdioState {
    conn: Option<StdioConn>,
    closed: bool,
}

/// Stdio-based transport (spawn local process).
pub struct StdioTransport {
    config: McpServerConfig,
    state: Mutex<StdioState>,
    pending: PendingMap,
    alive: Arc<AtomicBool>,
    active_generation: Arc<AtomicU64>,
    /// Direct-child exit signal for the active connection, independent of
    /// stdout EOF. Reset to `false` on every spawn and set to `true` by the
    /// child-exit watcher when the direct child process exits. Read
    /// synchronously by `health_check`.
    child_exited: Arc<AtomicBool>,
    #[cfg(all(test, unix))]
    write_test_hook: ParkingMutex<Option<Arc<StdioWriteTestHook>>>,
}

#[cfg(all(test, unix))]
pub(crate) struct StdioWriteTestHook {
    attempts: std::sync::atomic::AtomicUsize,
    attempts_changed: Notify,
    pause_next_payload: AtomicBool,
    payload_paused: Notify,
    release_payload: Notify,
}

#[cfg(all(test, unix))]
impl StdioWriteTestHook {
    pub(crate) fn new() -> Self {
        Self {
            attempts: std::sync::atomic::AtomicUsize::new(0),
            attempts_changed: Notify::new(),
            pause_next_payload: AtomicBool::new(false),
            payload_paused: Notify::new(),
            release_payload: Notify::new(),
        }
    }

    pub(crate) fn pause_next_payload(&self) {
        self.pause_next_payload.store(true, Ordering::Release);
    }

    pub(crate) async fn wait_for_attempts(&self, expected: usize) {
        loop {
            let changed = self.attempts_changed.notified();
            if self.attempts.load(Ordering::Acquire) >= expected {
                return;
            }
            changed.await;
        }
    }

    pub(crate) async fn wait_for_payload_pause(&self) {
        self.payload_paused.notified().await;
    }

    fn note_attempt(&self) {
        self.attempts.fetch_add(1, Ordering::AcqRel);
        self.attempts_changed.notify_waiters();
    }

    async fn pause_after_payload_if_armed(&self) {
        if self.pause_next_payload.swap(false, Ordering::AcqRel) {
            self.payload_paused.notify_one();
            self.release_payload.notified().await;
        }
    }
}

impl StdioTransport {
    pub fn new(config: &McpServerConfig) -> Result<Self> {
        let pending = Arc::new(ParkingMutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(false));
        let active_generation = Arc::new(AtomicU64::new(1));
        let child_exited = Arc::new(AtomicBool::new(false));
        let conn = Self::spawn(
            config,
            1,
            Arc::clone(&pending),
            Arc::clone(&alive),
            Arc::clone(&active_generation),
            Arc::clone(&child_exited),
        )?;
        Ok(Self {
            config: config.clone(),
            state: Mutex::new(StdioState {
                conn: Some(conn),
                closed: false,
            }),
            pending,
            alive,
            active_generation,
            child_exited,
            #[cfg(all(test, unix))]
            write_test_hook: ParkingMutex::new(None),
        })
    }

    #[cfg(all(test, unix))]
    pub(crate) fn set_write_test_hook(&self, hook: Arc<StdioWriteTestHook>) {
        *self.write_test_hook.lock() = Some(hook);
    }

    fn spawn(
        config: &McpServerConfig,
        generation: u64,
        pending: PendingMap,
        alive: Arc<AtomicBool>,
        active_generation: Arc<AtomicU64>,
        child_exited: Arc<AtomicBool>,
    ) -> Result<StdioConn> {
        let mut child = Command::new(&config.command)
            .args(&config.args)
            .envs(&config.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to spawn MCP server `{}`", config.name))?;

        let stdin = child.stdin.take().ok_or_else(|| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "mcp_server": &config.name,
                        "missing": "stdin",
                    })),
                "mcp_transport: no stdin on spawned MCP server"
            );
            anyhow::Error::msg(format!("no stdin on MCP server `{}`", config.name))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "mcp_server": &config.name,
                        "missing": "stdout",
                    })),
                "mcp_transport: no stdout on spawned MCP server"
            );
            anyhow::Error::msg(format!("no stdout on MCP server `{}`", config.name))
        })?;
        // Fresh generation starts alive and not-exited.
        alive.store(true, Ordering::Release);
        child_exited.store(false, Ordering::Release);
        let server_name = config.name.clone();
        let reader = zeroclaw_spawn::spawn!(stdio_read_loop(
            server_name,
            generation,
            stdout,
            pending,
            alive,
            active_generation,
        ));

        let child = Arc::new(tokio::sync::Mutex::new(child));
        let watcher_child = Arc::clone(&child);
        let watcher_flag = Arc::clone(&child_exited);
        let exit_watcher =
            zeroclaw_spawn::spawn!(stdio_child_exit_watcher(watcher_child, watcher_flag,));

        Ok(StdioConn {
            generation,
            child,
            stdin,
            reader,
            child_exited,
            exit_watcher,
        })
    }

    async fn send_raw(&self, stdin: &mut tokio::process::ChildStdin, line: &str) -> Result<()> {
        stdin
            .write_all(line.as_bytes())
            .await
            .context("failed to write to MCP server stdin")?;
        #[cfg(all(test, unix))]
        let write_test_hook = self.write_test_hook.lock().clone();
        #[cfg(all(test, unix))]
        if let Some(hook) = write_test_hook {
            hook.pause_after_payload_if_armed().await;
        }
        stdin
            .write_all(b"\n")
            .await
            .context("failed to write newline to MCP server stdin")?;
        stdin.flush().await.context("failed to flush stdin")?;
        Ok(())
    }

    async fn reap_conn(conn: StdioConn, server_name: &str) -> Result<()> {
        // Clone the shared handles needed after the connection is gone.
        let child = Arc::clone(&conn.child);
        let child_exited = Arc::clone(&conn.child_exited);
        // Dropping the connection runs `StdioConn::Drop`, which aborts the
        // background tasks (releasing their `Arc<Mutex<Child>>` clones so they
        // cannot race the reaping `wait`) AND drops the sole `ChildStdin`. That
        // closes the server's stdin, delivering EOF so a server that shuts down
        // on EOF can exit on its own before any signal. (`AsyncWriteExt::shutdown`
        // is a no-op on `ChildStdin` in tokio: it returns `Ready(Ok(()))`
        // without closing the fd; only dropping the handle closes the pipe.)
        drop(conn);

        let mut child = child.lock().await;
        // Give the server a bounded courtesy window to exit on the EOF it just
        // saw. A server that honors EOF exits near-instantly, so this only adds
        // latency when a server ignores EOF and must be signalled regardless.
        // Escalate to a signal only if the child is still running afterward.
        if timeout(STDIO_CLOSE_GRACE, child.wait()).await.is_err() {
            child
                .start_kill()
                .with_context(|| format!("failed to kill MCP server `{server_name}` child"))?;
            child
                .wait()
                .await
                .with_context(|| format!("failed to reap MCP server `{server_name}` child"))?;
        }
        // The direct child is now gone regardless of stdout pipe state.
        child_exited.store(true, Ordering::Release);
        Ok(())
    }
}

/// Watch the *direct* child process for exit, independent of its stdout pipe.
///
/// A misbehaving MCP server can spawn a descendant that inherits stdout and
/// keeps the pipe open after the direct child exits; the stdout reader would
/// then never see EOF. This nonblocking watcher polls `try_wait` so a dead
/// direct child is observed and surfaced through `health_check` even while the
/// inherited pipe stays open.
async fn stdio_child_exit_watcher(
    child: Arc<tokio::sync::Mutex<Child>>,
    child_exited: Arc<AtomicBool>,
) {
    loop {
        {
            let mut guard = child.lock().await;
            match guard.try_wait() {
                Ok(Some(_status)) => {
                    child_exited.store(true, Ordering::Release);
                    return;
                }
                Ok(None) => {}
                // Treat an inspection error as a dead child: fail closed rather
                // than reporting a possibly-exited process as healthy.
                Err(_) => {
                    child_exited.store(true, Ordering::Release);
                    return;
                }
            }
        }
        tokio::time::sleep(STDIO_CHILD_POLL_INTERVAL).await;
    }
}

enum BoundedLine {
    Line(Vec<u8>),
    Oversized,
    Eof,
}

async fn read_bounded_line(
    reader: &mut BufReader<tokio::process::ChildStdout>,
) -> std::io::Result<BoundedLine> {
    let mut line = Vec::new();
    let mut oversized = false;
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return if line.is_empty() {
                Ok(BoundedLine::Eof)
            } else if oversized {
                Ok(BoundedLine::Oversized)
            } else {
                Ok(BoundedLine::Line(line))
            };
        }

        let newline = buf.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(buf.len(), |index| index + 1);
        let content_len = newline.unwrap_or(buf.len());
        if !oversized {
            if line.len().saturating_add(content_len) > MAX_LINE_BYTES {
                oversized = true;
                line.clear();
            } else {
                line.extend_from_slice(&buf[..content_len]);
            }
        }
        reader.consume(consumed);
        if newline.is_some() {
            if oversized {
                return Ok(BoundedLine::Oversized);
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(BoundedLine::Line(line));
        }
    }
}

fn drain_pending_generation(pending: &PendingMap, generation: u64) {
    let senders = {
        let mut guard = pending.lock();
        let keys: Vec<(u64, u64)> = guard
            .keys()
            .filter(|(entry_generation, _)| *entry_generation == generation)
            .copied()
            .collect();
        keys.into_iter()
            .filter_map(|key| guard.remove(&key))
            .collect::<Vec<_>>()
    };
    drop(senders);
}

fn register_pending(
    pending: &PendingMap,
    generation: u64,
    id: u64,
    sender: oneshot::Sender<JsonRpcResponse>,
) -> Result<()> {
    match pending.lock().entry((generation, id)) {
        Entry::Vacant(entry) => {
            entry.insert(sender);
            Ok(())
        }
        Entry::Occupied(_) => bail!("duplicate in-flight MCP request id {id}"),
    }
}

fn deliver_stdio_response(
    pending: &PendingMap,
    generation: u64,
    response: JsonRpcResponse,
) -> bool {
    let Some(id) = response.id.as_ref().and_then(serde_json::Value::as_u64) else {
        return false;
    };
    let sender = pending.lock().remove(&(generation, id));
    sender.is_some_and(|sender| sender.send(response).is_ok())
}

fn finish_stdio_generation(
    pending: &PendingMap,
    generation: u64,
    alive: &AtomicBool,
    active_generation: &AtomicU64,
) {
    if active_generation.load(Ordering::Acquire) == generation {
        alive.store(false, Ordering::Release);
        drain_pending_generation(pending, generation);
    }
}

async fn stdio_read_loop(
    server_name: String,
    generation: u64,
    stdout: tokio::process::ChildStdout,
    pending: PendingMap,
    alive: Arc<AtomicBool>,
    active_generation: Arc<AtomicU64>,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_bounded_line(&mut reader).await {
            Ok(BoundedLine::Line(line)) => {
                let Ok(response) = serde_json::from_slice::<JsonRpcResponse>(&line) else {
                    continue;
                };
                let response_id = response.id.as_ref().and_then(serde_json::Value::as_u64);
                if !deliver_stdio_response(&pending, generation, response) {
                    ::zeroclaw_log::record!(
                        DEBUG,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_attrs(::serde_json::json!({
                                "mcp_server": &server_name,
                                "response_id": response_id,
                                "generation": generation,
                            })),
                        "mcp_transport: dropped unknown or stale stdio response"
                    );
                }
            }
            Ok(BoundedLine::Oversized) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "mcp_server": &server_name,
                            "max_bytes": MAX_LINE_BYTES,
                        })),
                    "mcp_transport: dropped oversized stdio response"
                );
            }
            Ok(BoundedLine::Eof) | Err(_) => break,
        }
    }

    finish_stdio_generation(&pending, generation, &alive, &active_generation);
}

#[async_trait::async_trait]
impl SharedMcpTransportConn for StdioTransport {
    async fn send_and_recv(
        &self,
        request: &JsonRpcRequest,
        lifecycle: &McpRequestLifecycle,
    ) -> Result<JsonRpcResponse> {
        let line = serde_json::to_string(request)?;
        let epoch_guard = lifecycle.begin_write().await;
        #[cfg(all(test, unix))]
        if let Some(hook) = self.write_test_hook.lock().clone() {
            hook.note_attempt();
        }
        let state = self.state.lock().await;
        // Re-check recovery only after acquiring the real stdio writer
        // boundary. If an earlier writer was cancelled while holding `state`,
        // its boundary guard publishes recovery before releasing `state`, so
        // this queued writer cannot race onto the ambiguous child.
        let mut write_boundary = StdioWriterBoundary::new(state, lifecycle)?;
        let state = write_boundary.state_mut()?;
        if state.closed {
            return Err(McpTransportError::TransportClosed.into());
        }
        let conn = state
            .conn
            .as_mut()
            .ok_or(McpTransportError::TransportClosed)?;
        if !self.alive.load(Ordering::Acquire)
            || self.active_generation.load(Ordering::Acquire) != conn.generation
        {
            return Err(McpTransportError::TransportClosed.into());
        }

        let request_id = request.id.as_ref().and_then(serde_json::Value::as_u64);
        let receiver = if let Some(id) = request_id {
            let (sender, receiver) = oneshot::channel();
            register_pending(&self.pending, conn.generation, id, sender)?;
            Some((
                StdioPendingGuard {
                    pending: Arc::clone(&self.pending),
                    key: (conn.generation, id),
                },
                receiver,
            ))
        } else if request.id.is_some() {
            bail!("unsupported non-integer MCP request id");
        } else {
            None
        };

        lifecycle.mark_outcome_unknown(epoch_guard.epoch());
        if let Err(error) = self.send_raw(&mut conn.stdin, &line).await {
            self.alive.store(false, Ordering::Release);
            return Err(error);
        }
        write_boundary.release_state();
        drop(epoch_guard);

        let Some((_pending_guard, receiver)) = receiver else {
            lifecycle.mark_completed();
            drop(write_boundary);
            return Ok(JsonRpcResponse {
                jsonrpc: crate::mcp_protocol::JSONRPC_VERSION.to_string(),
                id: None,
                result: None,
                error: None,
            });
        };
        let response = receiver
            .await
            .map_err(|_| McpTransportError::TransportClosed)?;
        lifecycle.mark_completed();
        drop(write_boundary);
        Ok(response)
    }

    async fn reset(&self) -> Result<()> {
        let mut state = self.state.lock().await;
        if state.closed {
            bail!("MCP stdio transport is closed");
        }

        let old_generation = self.active_generation.fetch_add(1, Ordering::AcqRel);
        self.alive.store(false, Ordering::Release);
        drain_pending_generation(&self.pending, old_generation);
        if let Some(conn) = state.conn.take() {
            Self::reap_conn(conn, &self.config.name).await?;
        }

        let generation = old_generation.wrapping_add(1);
        let conn = Self::spawn(
            &self.config,
            generation,
            Arc::clone(&self.pending),
            Arc::clone(&self.alive),
            Arc::clone(&self.active_generation),
            Arc::clone(&self.child_exited),
        )?;
        state.conn = Some(conn);
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        let mut state = self.state.lock().await;
        if state.closed {
            return Ok(());
        }
        state.closed = true;
        let old_generation = self.active_generation.fetch_add(1, Ordering::AcqRel);
        self.alive.store(false, Ordering::Release);
        drain_pending_generation(&self.pending, old_generation);
        if let Some(conn) = state.conn.take() {
            Self::reap_conn(conn, &self.config.name).await?;
        }
        Ok(())
    }

    fn health_check(&self) -> bool {
        // Healthy only when the reader still owns a live stream *and* the direct
        // child has not exited. The `child_exited` flag is driven by a
        // `try_wait`-based watcher independent of stdout EOF, so a parent that
        // exits while a descendant keeps the inherited stdout pipe open is
        // reported unhealthy instead of falsely alive.
        self.alive.load(Ordering::Acquire) && !self.child_exited.load(Ordering::Acquire)
    }
}

// ── HTTP Transport ───────────────────────────────────────────────────────

/// HTTP-based transport (POST requests).
pub struct HttpTransport {
    url: String,
    /// Per-server tool-call timeout, from `McpServerConfig.tool_timeout_secs`.
    /// Non-tool requests keep the legacy HTTP request timeout and short SSE
    /// read timeout. Tool calls use the configured budget when present; when
    /// absent, the client layer's outer tool-call timeout owns the budget.
    tool_timeout_secs: Option<u64>,
    client: reqwest::Client,
    headers: std::collections::HashMap<String, String>,
    session_id: ParkingMutex<Option<String>>,
}

impl HttpTransport {
    pub fn new(config: &McpServerConfig) -> Result<Self> {
        let url = config
            .url
            .as_ref()
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "mcp_server": &config.name,
                            "transport": "http",
                        })),
                    "mcp_transport: HTTP transport requires URL"
                );
                anyhow::Error::msg("URL required for HTTP transport")
            })?
            .clone();

        let client = reqwest::Client::builder()
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self {
            url,
            tool_timeout_secs: config.tool_timeout_secs,
            client,
            headers: config.headers.clone(),
            session_id: ParkingMutex::new(None),
        })
    }

    fn apply_session_header(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(session_id) = self.session_id.lock().as_deref() {
            req.header(MCP_SESSION_ID_HEADER, session_id)
        } else {
            req
        }
    }

    fn update_session_id_from_headers(&self, headers: &reqwest::header::HeaderMap) {
        if let Some(session_id) = headers
            .get(MCP_SESSION_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            *self.session_id.lock() = Some(session_id.to_string());
        }
    }
}

fn finish_response(
    request: &JsonRpcRequest,
    lifecycle: &McpRequestLifecycle,
    response: JsonRpcResponse,
) -> Result<JsonRpcResponse> {
    if response.id != request.id {
        bail!(
            "MCP response id mismatch: expected {:?}, received {:?}",
            request.id,
            response.id
        );
    }
    lifecycle.mark_completed();
    Ok(response)
}

/// HTTP 400 bodies that carry a recognized modern JSON-RPC error identify a
/// modern MCP server. Surface them as JSON-RPC responses so the era probe
/// can classify the peer instead of treating the status as a transport failure.
fn modern_rpc_error_from_http_body(body: &str) -> Option<JsonRpcResponse> {
    let rpc: JsonRpcResponse = serde_json::from_str(body.trim()).ok()?;
    crate::mcp_era::take_recognized_modern_error(rpc)
}

#[async_trait::async_trait]
impl SharedMcpTransportConn for HttpTransport {
    async fn send_and_recv(
        &self,
        request: &JsonRpcRequest,
        lifecycle: &McpRequestLifecycle,
    ) -> Result<JsonRpcResponse> {
        let body = serde_json::to_string(request)?;

        let has_accept = self
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("Accept"));
        let has_content_type = self
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("Content-Type"));

        let mut req = apply_request_timeout(
            self.client.post(&self.url).body(body),
            http_request_timeout_secs(request, self.tool_timeout_secs),
        );
        if !has_content_type {
            req = req.header("Content-Type", MCP_JSON_CONTENT_TYPE);
        }
        req = apply_configured_headers(req, &self.headers, lifecycle.era());
        if !has_accept {
            req = req.header("Accept", MCP_STREAMABLE_ACCEPT);
        }

        let epoch_guard = lifecycle.begin_write().await;
        if lifecycle.era() == PeerEra::Legacy {
            req = self.apply_session_header(req);
        } else {
            req = apply_modern_post_headers(req, request, lifecycle.protocol_version());
        }
        lifecycle.mark_outcome_unknown(epoch_guard.epoch());
        let resp = req
            .send()
            .await
            .context("HTTP request to MCP server failed")?;
        drop(epoch_guard);

        if !resp.status().is_success() {
            let status = resp.status();
            if self.session_id.lock().is_some()
                && (status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE)
            {
                return Err(McpTransportError::StaleSession {
                    status: status.as_u16(),
                }
                .into());
            }
            let body = resp.text().await.unwrap_or_default();
            if let Some(rpc) = modern_rpc_error_from_http_body(&body) {
                return finish_response(request, lifecycle, rpc);
            }
            lifecycle.mark_completed();
            bail!("MCP server returned HTTP {}", status);
        }

        if lifecycle.era() == PeerEra::Legacy {
            self.update_session_id_from_headers(resp.headers());
        }

        if request.id.is_none() {
            return finish_response(
                request,
                lifecycle,
                JsonRpcResponse {
                    jsonrpc: crate::mcp_protocol::JSONRPC_VERSION.to_string(),
                    id: None,
                    result: None,
                    error: None,
                },
            );
        }

        let is_sse = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("text/event-stream"));
        if is_sse {
            let read_response = read_first_jsonrpc_from_sse_response(resp);
            let maybe_resp = if let Some(sse_timeout) =
                http_sse_read_timeout_secs(request, self.tool_timeout_secs)
            {
                timeout(Duration::from_secs(sse_timeout), read_response)
                    .await
                    .context("timeout waiting for MCP response from streamable HTTP SSE stream")??
            } else {
                read_response.await?
            };
            let response = maybe_resp.ok_or_else(|| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                    "mcp_transport: MCP server returned no response in SSE stream"
                );
                anyhow::Error::msg("MCP server returned no response in SSE stream")
            })?;
            return finish_response(request, lifecycle, response);
        }

        let resp_text = resp.text().await.context("failed to read HTTP response")?;
        let response = parse_jsonrpc_response_text(&resp_text)?;
        finish_response(request, lifecycle, response)
    }

    async fn reset(&self) -> Result<()> {
        // Drop the stale session so the next request re-initializes and the
        // server issues a fresh `Mcp-Session-Id`.
        *self.session_id.lock() = None;
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

// ── SSE Transport ─────────────────────────────────────────────────────────

/// SSE-based transport (HTTP POST for requests, SSE for responses).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum SseStreamState {
    Unknown,
    Connected,
    Unsupported,
}

pub struct SseTransport {
    sse_url: String,
    server_name: String,
    tool_timeout_secs: Option<u64>,
    client: reqwest::Client,
    headers: std::collections::HashMap<String, String>,
    conn: Mutex<SseConnState>,
    shared: std::sync::Arc<Mutex<SseSharedState>>,
    pending: SsePendingMap,
    notify: std::sync::Arc<Notify>,
}

struct SseConnState {
    stream_state: SseStreamState,
    shutdown_tx: Option<oneshot::Sender<()>>,
    reader_task: Option<tokio::task::JoinHandle<()>>,
}

impl SseTransport {
    pub fn new(config: &McpServerConfig) -> Result<Self> {
        let sse_url = config
            .url
            .as_ref()
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "mcp_server": &config.name,
                            "transport": "sse",
                        })),
                    "mcp_transport: SSE transport requires URL"
                );
                anyhow::Error::msg("URL required for SSE transport")
            })?
            .clone();

        let client = reqwest::Client::builder()
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self {
            sse_url,
            server_name: config.name.clone(),
            tool_timeout_secs: config.tool_timeout_secs,
            client,
            headers: config.headers.clone(),
            conn: Mutex::new(SseConnState {
                stream_state: SseStreamState::Unknown,
                shutdown_tx: None,
                reader_task: None,
            }),
            shared: std::sync::Arc::new(Mutex::new(SseSharedState::default())),
            pending: Arc::new(ParkingMutex::new(HashMap::new())),
            notify: std::sync::Arc::new(Notify::new()),
        })
    }

    async fn ensure_connected(&self) -> Result<SseStreamState> {
        let mut conn = self.conn.lock().await;
        if conn.stream_state == SseStreamState::Unsupported {
            return Ok(conn.stream_state);
        }
        if let Some(task) = &conn.reader_task
            && !task.is_finished()
        {
            conn.stream_state = SseStreamState::Connected;
            return Ok(conn.stream_state);
        }

        let has_accept = self
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("Accept"));

        let mut req = self
            .client
            .get(&self.sse_url)
            .header("Cache-Control", "no-cache");
        for (key, value) in &self.headers {
            req = req.header(key, value);
        }
        if !has_accept {
            req = req.header("Accept", MCP_STREAMABLE_ACCEPT);
        }

        let resp = req.send().await.context("SSE GET to MCP server failed")?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND
            || resp.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED
        {
            conn.stream_state = SseStreamState::Unsupported;
            return Ok(conn.stream_state);
        }
        if !resp.status().is_success() {
            let status = resp.status();
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"status": status.as_u16()})),
                "mcp_transport: MCP server returned non-success HTTP"
            );
            return Err(anyhow::Error::msg(format!(
                "MCP server returned HTTP {}",
                status
            )));
        }
        let is_event_stream = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("text/event-stream"));
        if !is_event_stream {
            conn.stream_state = SseStreamState::Unsupported;
            return Ok(conn.stream_state);
        }

        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        conn.shutdown_tx = Some(shutdown_tx);

        let shared = self.shared.clone();
        let pending = Arc::clone(&self.pending);
        let notify = self.notify.clone();
        let sse_url = self.sse_url.clone();
        let server_name = self.server_name.clone();

        conn.reader_task = Some(zeroclaw_spawn::spawn!(async move {
            let stream = resp
                .bytes_stream()
                .map(|item| item.map_err(std::io::Error::other));
            let reader = tokio_util::io::StreamReader::new(stream);
            let mut lines = BufReader::new(reader).lines();

            let mut cur_event: Option<String> = None;
            let mut cur_id: Option<String> = None;
            let mut cur_data: Vec<String> = Vec::new();

            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => {
                        break;
                    }
                    line = lines.next_line() => {
                        let Ok(line_opt) = line else { break; };
                        let Some(mut line) = line_opt else { break; };
                        if line.ends_with('\r') {
                            line.pop();
                        }
                        if line.is_empty() {
                            if cur_event.is_none() && cur_id.is_none() && cur_data.is_empty() {
                                continue;
                            }
                            let event = cur_event.take();
                            let data = cur_data.join("\n");
                            cur_data.clear();
                            let id = cur_id.take();
                            handle_sse_event(&server_name, &sse_url, &shared, &pending, &notify, event.as_deref(), id.as_deref(), data).await;
                            continue;
                        }

                        if line.starts_with(':') {
                            continue;
                        }

                        if let Some(rest) = line.strip_prefix("event:") {
                            cur_event = Some(rest.trim().to_string());
                        }
                        if let Some(rest) = line.strip_prefix("data:") {
                            let rest = rest.strip_prefix(' ').unwrap_or(rest);
                            cur_data.push(rest.to_string());
                        }
                        if let Some(rest) = line.strip_prefix("id:") {
                            cur_id = Some(rest.trim().to_string());
                        }
                    }
                }
            }

            // Stream closed: drop every pending sender so each waiter observes a
            // `RecvError`, which `send_and_recv` maps to
            // `McpTransportError::TransportClosed` to trigger a reconnect.
            pending.lock().clear();
        }));
        conn.stream_state = SseStreamState::Connected;

        Ok(conn.stream_state)
    }

    async fn get_message_url(&self) -> Result<(String, bool)> {
        let guard = self.shared.lock().await;
        if let Some(url) = &guard.message_url {
            return Ok((url.clone(), guard.message_url_from_endpoint));
        }
        drop(guard);

        let derived = derive_message_url(&self.sse_url, "messages")
            .or_else(|| derive_message_url(&self.sse_url, "message"))
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"sse_url": &self.sse_url})),
                    "mcp_transport: invalid SSE URL"
                );
                anyhow::Error::msg("invalid SSE URL")
            })?;
        let mut guard = self.shared.lock().await;
        if guard.message_url.is_none() {
            guard.message_url = Some(derived.clone());
            guard.message_url_from_endpoint = false;
        }
        Ok((derived, false))
    }

    async fn teardown(&self) {
        let mut conn = self.conn.lock().await;
        if let Some(tx) = conn.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = conn.reader_task.take() {
            task.abort();
            let _ = task.await;
        }
        conn.stream_state = SseStreamState::Unknown;
        drop(conn);

        let mut shared = self.shared.lock().await;
        shared.message_url = None;
        shared.message_url_from_endpoint = false;
        drop(shared);
        self.pending.lock().clear();
    }
}

#[derive(Default)]
struct SseSharedState {
    message_url: Option<String>,
    message_url_from_endpoint: bool,
}

type SsePendingMap = Arc<ParkingMutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>;

struct SsePendingGuard {
    pending: SsePendingMap,
    id: u64,
}

impl Drop for SsePendingGuard {
    fn drop(&mut self) {
        self.pending.lock().remove(&self.id);
    }
}

fn derive_message_url(sse_url: &str, message_path: &str) -> Option<String> {
    let url = reqwest::Url::parse(sse_url).ok()?;
    let mut segments: Vec<&str> = url.path_segments()?.collect();
    if segments.is_empty() {
        return None;
    }
    if segments.last().copied() == Some("sse") {
        segments.pop();
        segments.push(message_path);
        let mut new_url = url.clone();
        new_url.set_path(&format!("/{}", segments.join("/")));
        return Some(new_url.to_string());
    }
    let mut new_url = url.clone();
    let mut path = url.path().trim_end_matches('/').to_string();
    path.push('/');
    path.push_str(message_path);
    new_url.set_path(&path);
    Some(new_url.to_string())
}

async fn handle_sse_event(
    server_name: &str,
    sse_url: &str,
    shared: &std::sync::Arc<Mutex<SseSharedState>>,
    pending: &SsePendingMap,
    notify: &std::sync::Arc<Notify>,
    event: Option<&str>,
    _id: Option<&str>,
    data: String,
) {
    let event = event.unwrap_or("message");
    let trimmed = data.trim();
    if trimmed.is_empty() {
        return;
    }

    if event.eq_ignore_ascii_case("endpoint") || event.eq_ignore_ascii_case("mcp-endpoint") {
        if let Some(url) = parse_endpoint_from_data(sse_url, trimmed) {
            let mut guard = shared.lock().await;
            guard.message_url = Some(url);
            guard.message_url_from_endpoint = true;
            drop(guard);
            notify.notify_waiters();
        }
        return;
    }

    if !event.eq_ignore_ascii_case("message") {
        return;
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return;
    };

    let Ok(resp) = serde_json::from_value::<JsonRpcResponse>(value.clone()) else {
        let _ = serde_json::from_value::<JsonRpcRequest>(value);
        return;
    };

    let Some(id_val) = resp.id.clone() else {
        return;
    };
    let id = match id_val.as_u64() {
        Some(v) => v,
        None => return,
    };

    let tx = pending.lock().remove(&id);
    if let Some(tx) = tx {
        let _ = tx.send(resp);
    } else {
        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            &format!(
                "MCP SSE `{}` received response for unknown id {}",
                server_name, id
            )
        );
    }
}

fn parse_endpoint_from_data(sse_url: &str, data: &str) -> Option<String> {
    if data.starts_with('{') {
        let v: serde_json::Value = serde_json::from_str(data).ok()?;
        let endpoint = v.get("endpoint")?.as_str()?;
        return parse_endpoint_from_data(sse_url, endpoint);
    }
    if data.starts_with("http://") || data.starts_with("https://") {
        return Some(data.to_string());
    }
    let base = reqwest::Url::parse(sse_url).ok()?;
    base.join(data).ok().map(|u| u.to_string())
}

fn extract_json_from_sse_text(resp_text: &str) -> Cow<'_, str> {
    let text = resp_text.trim_start_matches('\u{feff}');
    let mut current_data_lines: Vec<&str> = Vec::new();
    let mut last_event_data_lines: Vec<&str> = Vec::new();

    for raw_line in text.lines() {
        let line = raw_line.trim_end_matches('\r').trim_start();
        if line.is_empty() {
            if !current_data_lines.is_empty() {
                last_event_data_lines = std::mem::take(&mut current_data_lines);
            }
            continue;
        }

        if line.starts_with(':') {
            continue;
        }

        if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            current_data_lines.push(rest);
        }
    }

    if !current_data_lines.is_empty() {
        last_event_data_lines = current_data_lines;
    }

    if last_event_data_lines.is_empty() {
        return Cow::Borrowed(text.trim());
    }

    if last_event_data_lines.len() == 1 {
        return Cow::Borrowed(last_event_data_lines[0].trim());
    }

    let joined = last_event_data_lines.join("\n");
    Cow::Owned(joined.trim().to_string())
}

fn parse_jsonrpc_response_text(resp_text: &str) -> Result<JsonRpcResponse> {
    let trimmed = resp_text.trim();
    if trimmed.is_empty() {
        bail!("MCP server returned no response");
    }

    let json_text = if looks_like_sse_text(trimmed) {
        extract_json_from_sse_text(trimmed)
    } else {
        Cow::Borrowed(trimmed)
    };

    let mcp_resp: JsonRpcResponse = serde_json::from_str(json_text.as_ref())
        .with_context(|| format!("invalid JSON-RPC response: {}", resp_text))?;
    Ok(mcp_resp)
}

fn looks_like_sse_text(text: &str) -> bool {
    text.starts_with("data:")
        || text.starts_with("event:")
        || text.contains("\ndata:")
        || text.contains("\nevent:")
}

async fn read_first_jsonrpc_from_sse_response(
    resp: reqwest::Response,
) -> Result<Option<JsonRpcResponse>> {
    let stream = resp
        .bytes_stream()
        .map(|item| item.map_err(std::io::Error::other));
    let reader = tokio_util::io::StreamReader::new(stream);
    let mut lines = BufReader::new(reader).lines();

    let mut cur_event: Option<String> = None;
    let mut cur_data: Vec<String> = Vec::new();

    while let Ok(line_opt) = lines.next_line().await {
        let Some(mut line) = line_opt else { break };
        if line.ends_with('\r') {
            line.pop();
        }
        if line.is_empty() {
            if cur_event.is_none() && cur_data.is_empty() {
                continue;
            }
            let event = cur_event.take();
            let data = cur_data.join("\n");
            cur_data.clear();

            let event = event.unwrap_or_else(|| "message".to_string());
            if event.eq_ignore_ascii_case("endpoint") || event.eq_ignore_ascii_case("mcp-endpoint")
            {
                continue;
            }
            if !event.eq_ignore_ascii_case("message") {
                continue;
            }

            let trimmed = data.trim();
            if trimmed.is_empty() {
                continue;
            }
            let json_str = extract_json_from_sse_text(trimmed);
            if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(json_str.as_ref()) {
                return Ok(Some(resp));
            }
            continue;
        }

        if line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            cur_event = Some(rest.trim().to_string());
        }
        if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            cur_data.push(rest.to_string());
        }
    }

    Ok(None)
}

#[async_trait::async_trait]
impl SharedMcpTransportConn for SseTransport {
    async fn send_and_recv(
        &self,
        request: &JsonRpcRequest,
        lifecycle: &McpRequestLifecycle,
    ) -> Result<JsonRpcResponse> {
        let stream_state = if lifecycle.era() == PeerEra::Modern {
            SseStreamState::Unsupported
        } else {
            self.ensure_connected().await?
        };

        let id = request.id.as_ref().and_then(|v| v.as_u64());
        if request.id.is_some() && id.is_none() {
            bail!("unsupported non-integer MCP request id");
        }
        let body = serde_json::to_string(request)?;

        let (mut message_url, mut from_endpoint) = self.get_message_url().await?;
        if stream_state == SseStreamState::Connected && !from_endpoint {
            for _ in 0..3 {
                {
                    let guard = self.shared.lock().await;
                    if guard.message_url_from_endpoint
                        && let Some(url) = &guard.message_url
                    {
                        message_url = url.clone();
                        from_endpoint = true;
                        break;
                    }
                }
                let _ = timeout(Duration::from_millis(300), self.notify.notified()).await;
            }
        }
        let message_url = if from_endpoint {
            message_url.clone()
        } else {
            self.sse_url.clone()
        };

        // Acquire the epoch permit before registering a response waiter.
        // Cancellation while waiting for the permit is provably pre-write and
        // therefore cannot leak a pending sender.
        let epoch_guard = lifecycle.begin_write().await;
        let mut rx = None;
        if let Some(id) = id
            && stream_state == SseStreamState::Connected
        {
            let (tx, ch) = oneshot::channel();
            match self.pending.lock().entry(id) {
                Entry::Vacant(entry) => {
                    entry.insert(tx);
                }
                Entry::Occupied(_) => {
                    bail!("duplicate in-flight MCP request id {id}");
                }
            }
            rx = Some((
                SsePendingGuard {
                    pending: Arc::clone(&self.pending),
                    id,
                },
                ch,
            ));
        }

        let has_accept = self
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("Accept"));
        let has_content_type = self
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("Content-Type"));
        let mut req = apply_request_timeout(
            self.client.post(&message_url).body(body),
            http_request_timeout_secs(request, self.tool_timeout_secs),
        );
        if !has_content_type {
            req = req.header("Content-Type", MCP_JSON_CONTENT_TYPE);
        }
        req = apply_configured_headers(req, &self.headers, lifecycle.era());
        if !has_accept {
            req = req.header("Accept", MCP_STREAMABLE_ACCEPT);
        }
        if lifecycle.era() == PeerEra::Modern {
            req = apply_modern_post_headers(req, request, lifecycle.protocol_version());
        }

        lifecycle.mark_outcome_unknown(epoch_guard.epoch());
        let resp = req.send().await.context("SSE POST to MCP server failed")?;
        let status = resp.status();
        let mut got_direct = None;

        if status.is_success() {
            if request.id.is_none() {
                got_direct = Some(JsonRpcResponse {
                    jsonrpc: crate::mcp_protocol::JSONRPC_VERSION.to_string(),
                    id: None,
                    result: None,
                    error: None,
                });
            } else {
                let is_sse = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v.to_ascii_lowercase().contains("text/event-stream"));

                if is_sse {
                    got_direct = read_first_jsonrpc_from_sse_response(resp).await?;
                } else {
                    let text = resp.text().await.unwrap_or_default();
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        let json_str =
                            if trimmed.contains("\ndata:") || trimmed.starts_with("data:") {
                                extract_json_from_sse_text(trimmed)
                            } else {
                                Cow::Borrowed(trimmed)
                            };
                        if let Ok(mcp_resp) =
                            serde_json::from_str::<JsonRpcResponse>(json_str.as_ref())
                        {
                            got_direct = Some(mcp_resp);
                        }
                    }
                }
            }
            drop(epoch_guard);
        } else {
            drop(epoch_guard);
            if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
                return Err(McpTransportError::StaleSession {
                    status: status.as_u16(),
                }
                .into());
            }
            let body = resp.text().await.unwrap_or_default();
            if let Some(rpc) = modern_rpc_error_from_http_body(&body) {
                return finish_response(request, lifecycle, rpc);
            }
            lifecycle.mark_completed();
            bail!("MCP server returned HTTP {}", status);
        }

        if let Some(resp) = got_direct {
            return finish_response(request, lifecycle, resp);
        }

        let Some((_pending_guard, rx)) = rx else {
            bail!("MCP server returned no response");
        };

        // A dropped receiver means the SSE reader task tore down the stream
        // before our response arrived — recoverable via reconnect.
        rx.await
            .map_err(|_| McpTransportError::TransportClosed.into())
            .and_then(|response| finish_response(request, lifecycle, response))
    }

    async fn reset(&self) -> Result<()> {
        // Tear down the reader task and clear the cached endpoint/session state
        // so the next send re-handshakes: a fresh GET stream and a new
        // `endpoint` event from the (possibly restarted) server.
        self.teardown().await;
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        self.teardown().await;
        Ok(())
    }
}

macro_rules! impl_legacy_transport {
    ($transport:ty) => {
        #[async_trait::async_trait]
        impl McpTransportConn for $transport {
            async fn send_and_recv(&mut self, request: &JsonRpcRequest) -> Result<JsonRpcResponse> {
                let lifecycle = McpRequestLifecycle::uncoordinated(0);
                SharedMcpTransportConn::send_and_recv(self, request, &lifecycle).await
            }

            async fn reset(&mut self) -> Result<()> {
                SharedMcpTransportConn::reset(self).await
            }

            fn health_check(&mut self) -> bool {
                SharedMcpTransportConn::health_check(self)
            }

            async fn close(&mut self) -> Result<()> {
                SharedMcpTransportConn::close(self).await
            }
        }
    };
}

impl_legacy_transport!(StdioTransport);
impl_legacy_transport!(HttpTransport);
impl_legacy_transport!(SseTransport);

// ── Factory ──────────────────────────────────────────────────────────────

/// Create a transport based on config.
pub fn create_transport(config: &McpServerConfig) -> Result<Box<dyn McpTransportConn>> {
    match config.transport {
        McpTransport::Stdio => Ok(Box::new(StdioTransport::new(config)?)),
        McpTransport::Http => Ok(Box::new(HttpTransport::new(config)?)),
        McpTransport::Sse => Ok(Box::new(SseTransport::new(config)?)),
    }
}

pub(crate) fn create_shared_transport(
    config: &McpServerConfig,
) -> Result<Box<dyn SharedMcpTransportConn>> {
    match config.transport {
        McpTransport::Stdio => Ok(Box::new(StdioTransport::new(config)?)),
        McpTransport::Http => Ok(Box::new(HttpTransport::new(config)?)),
        McpTransport::Sse => Ok(Box::new(SseTransport::new(config)?)),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;

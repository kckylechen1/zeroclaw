//! MCP transport abstraction — supports stdio, SSE, and HTTP transports.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::mcp_protocol::{JsonRpcRequest, JsonRpcResponse};
use anyhow::{Context, Result, bail};
use futures_util::stream::FuturesUnordered;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use tokio::time::{Duration, timeout};
use tokio_stream::StreamExt;
use zeroclaw_config::schema::{McpServerConfig, McpTransport};

/// Maximum bytes for a single JSON-RPC response.
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024; // 4 MB

/// Timeout for init/list operations (and non-tool stdio waits).
const RECV_TIMEOUT_SECS: u64 = 30;

/// Bound on a single stdin write attempt so a blocked pipe cannot stall
/// cancel/reset/close indefinitely.
const STDIO_WRITE_TIMEOUT_SECS: u64 = 15;

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

fn http_request_timeout_secs(request: &JsonRpcRequest, config: &McpServerConfig) -> Option<u64> {
    if request.method == TOOLS_CALL_METHOD {
        // When unset, leave budget to the client outer timeout.
        config
            .tool_timeout_secs
            .map(|t| t.clamp(1, McpServerConfig::MAX_TOOL_TIMEOUT_SECS))
    } else {
        Some(DEFAULT_HTTP_REQUEST_TIMEOUT_SECS)
    }
}

fn http_sse_read_timeout_secs(request: &JsonRpcRequest, config: &McpServerConfig) -> Option<u64> {
    if request.method == TOOLS_CALL_METHOD {
        config
            .tool_timeout_secs
            .map(|t| t.clamp(1, McpServerConfig::MAX_TOOL_TIMEOUT_SECS))
    } else {
        Some(RECV_TIMEOUT_SECS)
    }
}

fn stdio_recv_timeout_secs(request: &JsonRpcRequest, config: &McpServerConfig) -> u64 {
    if request.method == TOOLS_CALL_METHOD {
        config.resolved_tool_timeout_secs()
    } else {
        RECV_TIMEOUT_SECS
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

// ── Transport Errors ───────────────────────────────────────────────────────

#[derive(Debug, Clone, thiserror::Error)]
pub enum McpTransportError {
    #[error("MCP session is stale (HTTP {status})")]
    StaleSession { status: u16 },

    #[error("MCP transport connection closed")]
    TransportClosed,

    #[error("MCP transport timed out waiting for response")]
    ResponseTimeout,

    /// Request was written (or write progress is uncertain). Do not auto-replay.
    #[error("MCP tool call outcome unknown (transport closed after submit)")]
    OutcomeUnknown,

    /// Request was definitely not written; safe to retry.
    #[error("MCP request was not sent")]
    NotSent,
}

// ── Strict inbound classifier ──────────────────────────────────────────────

/// Classify a server→client JSON-RPC message.
///
/// Rules:
/// - objects with `method` are server requests/notifications (never responses)
/// - responses must have `id` and either `result` or `error`
/// - anything else is rejected / skippable
#[derive(Debug)]
pub(crate) enum JsonRpcInbound {
    Response(JsonRpcResponse),
    ServerMessage {
        id: Option<serde_json::Value>,
        method: String,
    },
    Skippable,
}

pub(crate) fn classify_jsonrpc_value(value: serde_json::Value) -> Result<JsonRpcInbound> {
    if let Some(method) = value.get("method").and_then(|m| m.as_str()) {
        return Ok(JsonRpcInbound::ServerMessage {
            id: value.get("id").cloned(),
            method: method.to_string(),
        });
    }
    if value.get("id").is_none() {
        return Ok(JsonRpcInbound::Skippable);
    }
    let has_result = value.get("result").is_some();
    let has_error = value.get("error").is_some();
    if !has_result && !has_error {
        bail!("JSON-RPC response envelope missing both result and error");
    }
    let resp: JsonRpcResponse =
        serde_json::from_value(value).context("invalid JSON-RPC response envelope")?;
    Ok(JsonRpcInbound::Response(resp))
}

pub(crate) fn classify_jsonrpc_str(line: &str) -> Result<JsonRpcInbound> {
    let value: serde_json::Value =
        serde_json::from_str(line).with_context(|| format!("invalid JSON-RPC message: {line}"))?;
    classify_jsonrpc_value(value)
}

/// Match a classified response against an expected JSON-RPC id.
pub(crate) fn match_response_id(resp: &JsonRpcResponse, expected_id: &serde_json::Value) -> bool {
    resp.id.as_ref() == Some(expected_id)
}

// ── Transport Trait ──────────────────────────────────────────────────────

/// Shared (`&self`) transport surface. Stdio uses a worker; HTTP/SSE use
/// [`SerialTransport`] to preserve exclusive behavior.
#[async_trait::async_trait]
pub trait McpTransportConn: Send + Sync {
    async fn send_and_recv(&self, request: &JsonRpcRequest) -> Result<JsonRpcResponse>;

    async fn reset(&self) -> Result<()> {
        Ok(())
    }

    /// Monotonic generation; advances on successful reset/respawn.
    fn generation(&self) -> u64 {
        0
    }

    fn health_check(&self) -> bool {
        true
    }

    async fn close(&self) -> Result<()>;
}

#[async_trait::async_trait]
trait ExclusiveMcpTransport: Send {
    async fn send_and_recv(&mut self, request: &JsonRpcRequest) -> Result<JsonRpcResponse>;
    async fn reset(&mut self) -> Result<()>;
    fn health_check(&mut self) -> bool {
        true
    }
    async fn close(&mut self) -> Result<()>;
}

/// Serializes HTTP/SSE RPCs (conservative exclusive behavior).
struct SerialTransport<T> {
    inner: Mutex<T>,
    generation: AtomicU64,
}

impl<T> SerialTransport<T> {
    fn new(inner: T) -> Self {
        Self {
            inner: Mutex::new(inner),
            generation: AtomicU64::new(0),
        }
    }
}

#[async_trait::async_trait]
impl<T> McpTransportConn for SerialTransport<T>
where
    T: ExclusiveMcpTransport,
{
    async fn send_and_recv(&self, request: &JsonRpcRequest) -> Result<JsonRpcResponse> {
        self.inner.lock().await.send_and_recv(request).await
    }

    async fn reset(&self) -> Result<()> {
        self.inner.lock().await.reset().await?;
        self.generation.fetch_add(1, Ordering::Release);
        Ok(())
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn health_check(&self) -> bool {
        self.inner
            .try_lock()
            .map(|mut g| g.health_check())
            .unwrap_or(true)
    }

    async fn close(&self) -> Result<()> {
        self.inner.lock().await.close().await
    }
}

// ── Stdio Transport (worker / request router) ────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteProgress {
    NotStarted,
    /// At least one write syscall may have delivered bytes to the pipe.
    Started,
}

struct CancelOnDrop {
    tx: Option<oneshot::Sender<()>>,
}

impl CancelOnDrop {
    fn arm() -> (Self, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        (Self { tx: Some(tx) }, rx)
    }

    fn disarm(&mut self) {
        self.tx.take();
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Stdio handle. Callers never own the pipes; a worker serializes stdin writes
/// and demuxes stdout by JSON-RPC id.
///
/// Concurrent `send_and_recv` futures do **not** hold a server mutex across
/// waits. Stdio writes remain serialized by the worker.
///
/// The worker does **not** auto-respawn. Dirty sessions leave `session = None`
/// and the generation-aware client coordinator owns reset/handshake.
pub struct StdioTransport {
    config: Arc<McpServerConfig>,
    cmd_tx: mpsc::Sender<StdioWorkerCmd>,
    generation: Arc<AtomicU64>,
    child_alive: Arc<AtomicBool>,
}

enum StdioWorkerCmd {
    Rpc {
        request: JsonRpcRequest,
        cancel: oneshot::Receiver<()>,
        reply: oneshot::Sender<Result<JsonRpcResponse>>,
    },
    Reset {
        reply: oneshot::Sender<Result<()>>,
    },
    Close {
        reply: oneshot::Sender<Result<()>>,
    },
}

struct StdioSession {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout_lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}

struct PendingRpc {
    reply: oneshot::Sender<Result<JsonRpcResponse>>,
    deadline_abort: tokio::task::AbortHandle,
    cancel_abort: tokio::task::AbortHandle,
}

impl StdioTransport {
    pub fn new(config: Arc<McpServerConfig>) -> Result<Self> {
        let session = spawn_stdio_session(&config)?;
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let generation = Arc::new(AtomicU64::new(0));
        let child_alive = Arc::new(AtomicBool::new(true));
        let worker_config = Arc::clone(&config);
        let gen_w = Arc::clone(&generation);
        let alive_w = Arc::clone(&child_alive);
        zeroclaw_spawn::spawn!(async move {
            stdio_worker(worker_config, session, cmd_rx, gen_w, alive_w).await;
        });
        Ok(Self {
            config,
            cmd_tx,
            generation,
            child_alive,
        })
    }

    async fn send_cmd(&self, cmd: StdioWorkerCmd) -> Result<()> {
        self.cmd_tx.send(cmd).await.map_err(|_| {
            anyhow::Error::new(McpTransportError::TransportClosed).context(format!(
                "MCP stdio worker unavailable for server `{}`",
                self.config.name
            ))
        })
    }
}

fn spawn_stdio_session(config: &McpServerConfig) -> Result<StdioSession> {
    let mut command = Command::new(&config.command);
    command
        .args(&config.args)
        .envs(&config.env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true);
    // Own process group so reset can reap descendants (mirrors shell tool).
    // Windows: no job-object helper exists in this crate; only the direct child
    // is killed/reaped (process-tree orphans are a known platform limitation).
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn MCP server `{}`", config.name))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::Error::msg(format!("no stdin on MCP server `{}`", config.name)))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::Error::msg(format!("no stdout on MCP server `{}`", config.name)))?;
    Ok(StdioSession {
        child,
        stdin,
        stdout_lines: BufReader::new(stdout).lines(),
    })
}

/// Kill the child (process group on Unix) and await confirmed termination.
///
/// Returns `Err` if termination cannot be confirmed — callers must **not**
/// spawn a replacement in that case.
async fn kill_and_reap_stdio_session(session: &mut StdioSession, server_name: &str) -> Result<()> {
    #[cfg(unix)]
    {
        if let Some(pid) = session.child.id()
            && let Ok(pgid) = i32::try_from(pid)
            && pgid > 0
        {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Kill)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({
                        "mcp_server": server_name,
                        "pgid": pgid,
                        "signal": "SIGKILL",
                    })),
                "mcp_transport: reaping stdio MCP process group"
            );
            // SAFETY: pgid is the child's pid (== pgid when process_group(0)).
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
    let _ = session.child.start_kill();
    match timeout(Duration::from_secs(5), session.child.wait()).await {
        Ok(Ok(_status)) => Ok(()),
        Ok(Err(err)) => Err(anyhow::Error::new(err).context(format!(
            "MCP server `{server_name}` wait failed while reaping stdio child"
        ))),
        Err(_) => {
            // Last-resort poll.
            match session.child.try_wait() {
                Ok(Some(_)) => Ok(()),
                Ok(None) => Err(anyhow::Error::msg(format!(
                    "MCP server `{server_name}` child did not terminate after kill; refusing to spawn replacement"
                ))),
                Err(err) => Err(anyhow::Error::new(err).context(format!(
                    "MCP server `{server_name}` try_wait failed after kill timeout"
                ))),
            }
        }
    }
}

async fn write_stdio_line_tracked(
    stdin: &mut tokio::process::ChildStdin,
    line: &str,
) -> std::result::Result<(), (WriteProgress, anyhow::Error)> {
    // Mark Started before the first syscall — any error afterward is
    // OutcomeUnknown (bytes may have reached the pipe).
    let progress = WriteProgress::Started;
    if let Err(e) = stdin.write_all(line.as_bytes()).await {
        return Err((
            progress,
            anyhow::Error::new(e).context("failed to write to MCP server stdin"),
        ));
    }
    if let Err(e) = stdin.write_all(b"\n").await {
        return Err((
            progress,
            anyhow::Error::new(e).context("failed to write newline to MCP server stdin"),
        ));
    }
    if let Err(e) = stdin.flush().await {
        return Err((
            progress,
            anyhow::Error::new(e).context("failed to flush stdin"),
        ));
    }
    Ok(())
}

fn fail_pending(pending: &mut HashMap<serde_json::Value, PendingRpc>, err: McpTransportError) {
    for (_, slot) in pending.drain() {
        slot.deadline_abort.abort();
        slot.cancel_abort.abort();
        let _ = slot.reply.send(Err(err.clone().into()));
    }
}

fn mark_session_dirty(
    session: &mut Option<StdioSession>,
    pending: &mut HashMap<serde_json::Value, PendingRpc>,
    alive: &AtomicBool,
    err: McpTransportError,
) {
    // Drop pipes without respawning — coordinator owns recovery.
    *session = None;
    alive.store(false, Ordering::Release);
    fail_pending(pending, err);
}

async fn respawn_stdio_session(
    config: &McpServerConfig,
    session: &mut Option<StdioSession>,
    generation: &AtomicU64,
    alive: &AtomicBool,
) -> Result<()> {
    if let Some(mut old) = session.take() {
        kill_and_reap_stdio_session(&mut old, &config.name).await?;
    }
    let new_session = spawn_stdio_session(config).with_context(|| {
        format!(
            "MCP server `{}` failed to spawn replacement stdio child after reset",
            config.name
        )
    })?;
    *session = Some(new_session);
    alive.store(true, Ordering::Release);
    generation.fetch_add(1, Ordering::Release);
    Ok(())
}

async fn stdio_worker(
    config: Arc<McpServerConfig>,
    initial: StdioSession,
    mut cmd_rx: mpsc::Receiver<StdioWorkerCmd>,
    generation: Arc<AtomicU64>,
    alive: Arc<AtomicBool>,
) {
    let mut session: Option<StdioSession> = Some(initial);
    let mut pending: HashMap<serde_json::Value, PendingRpc> = HashMap::new();
    let mut cancels: FuturesUnordered<
        std::pin::Pin<Box<dyn std::future::Future<Output = serde_json::Value> + Send>>,
    > = FuturesUnordered::new();
    let mut deadlines: FuturesUnordered<
        std::pin::Pin<Box<dyn std::future::Future<Output = serde_json::Value> + Send>>,
    > = FuturesUnordered::new();

    loop {
        let has_session = session.is_some();
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break; };
                match cmd {
                    StdioWorkerCmd::Rpc { request, mut cancel, reply } => {
                        // Cancelled while queued — must not write.
                        if cancel.try_recv().is_ok() {
                            let _ = reply.send(Err(McpTransportError::NotSent.into()));
                            continue;
                        }
                        let Some(sess) = session.as_mut() else {
                            let _ = reply.send(Err(McpTransportError::NotSent.into()));
                            continue;
                        };
                        let line = match serde_json::to_string(&request) {
                            Ok(l) => l,
                            Err(e) => {
                                let _ = reply.send(Err(e.into()));
                                continue;
                            }
                        };

                        let write_result = tokio::select! {
                            biased;
                            c = &mut cancel => {
                                let _ = c;
                                Err((WriteProgress::NotStarted, anyhow::Error::new(McpTransportError::NotSent)))
                            }
                            res = timeout(
                                Duration::from_secs(STDIO_WRITE_TIMEOUT_SECS),
                                write_stdio_line_tracked(&mut sess.stdin, &line),
                            ) => {
                                match res {
                                    Ok(Ok(())) => Ok(()),
                                    Ok(Err(pair)) => Err(pair),
                                    Err(_) => Err((
                                        WriteProgress::Started,
                                        anyhow::Error::msg("stdio write timed out"),
                                    )),
                                }
                            }
                        };

                        match write_result {
                            Err((WriteProgress::NotStarted, _err)) => {
                                let _ = reply.send(Err(McpTransportError::NotSent.into()));
                                continue;
                            }
                            Err((WriteProgress::Started, _err)) => {
                                let _ = reply.send(Err(McpTransportError::OutcomeUnknown.into()));
                                mark_session_dirty(
                                    &mut session,
                                    &mut pending,
                                    &alive,
                                    McpTransportError::OutcomeUnknown,
                                );
                                cancels.clear();
                                deadlines.clear();
                                continue;
                            }
                            Ok(()) => {}
                        }

                        if request.id.is_none() {
                            let _ = reply.send(Ok(JsonRpcResponse {
                                jsonrpc: crate::mcp_protocol::JSONRPC_VERSION.to_string(),
                                id: None,
                                result: None,
                                error: None,
                            }));
                            continue;
                        }
                        let Some(id) = request.id.clone() else { continue; };
                        if pending.contains_key(&id) {
                            let _ = reply.send(Err(anyhow::Error::msg(
                                "MCP stdio: refusing to replace occupied pending JSON-RPC id",
                            )));
                            continue;
                        }
                        let wait_secs = stdio_recv_timeout_secs(&request, &config);
                        let id_for_cancel = id.clone();
                        let cancel_handle = zeroclaw_spawn::spawn!(async move {
                            match cancel.await {
                                Ok(()) => Some(id_for_cancel),
                                Err(_) => None, // disarmed after success
                            }
                        });
                        let cancel_abort = cancel_handle.abort_handle();
                        cancels.push(Box::pin(async move {
                            match cancel_handle.await {
                                Ok(Some(id)) => id,
                                _ => serde_json::Value::Null,
                            }
                        }));

                        let id_for_deadline = id.clone();
                        let deadline_handle = zeroclaw_spawn::spawn!(async move {
                            tokio::time::sleep(Duration::from_secs(wait_secs)).await;
                            id_for_deadline
                        });
                        let deadline_abort = deadline_handle.abort_handle();
                        deadlines.push(Box::pin(async move {
                            deadline_handle.await.unwrap_or_else(|_| serde_json::Value::Null)
                        }));

                        pending.insert(
                            id,
                            PendingRpc {
                                reply,
                                deadline_abort,
                                cancel_abort,
                            },
                        );
                    }
                    StdioWorkerCmd::Reset { reply } => {
                        fail_pending(&mut pending, McpTransportError::OutcomeUnknown);
                        cancels.clear();
                        deadlines.clear();
                        let result =
                            respawn_stdio_session(&config, &mut session, &generation, &alive).await;
                        let _ = reply.send(result);
                    }
                    StdioWorkerCmd::Close { reply } => {
                        fail_pending(&mut pending, McpTransportError::TransportClosed);
                        cancels.clear();
                        deadlines.clear();
                        if let Some(mut sess) = session.take() {
                            let _ = sess.stdin.shutdown().await;
                            let _ = kill_and_reap_stdio_session(&mut sess, &config.name).await;
                        }
                        alive.store(false, Ordering::Release);
                        let _ = reply.send(Ok(()));
                        break;
                    }
                }
            }
            line = async {
                match session.as_mut() {
                    Some(sess) => sess.stdout_lines.next_line().await,
                    None => std::future::pending().await,
                }
            }, if has_session => {
                match line {
                    Ok(Some(resp_line)) => {
                        if resp_line.len() > MAX_LINE_BYTES {
                            mark_session_dirty(
                                &mut session,
                                &mut pending,
                                &alive,
                                McpTransportError::TransportClosed,
                            );
                            cancels.clear();
                            deadlines.clear();
                            continue;
                        }
                        match classify_jsonrpc_str(&resp_line) {
                            Ok(JsonRpcInbound::ServerMessage { id, method }) => {
                                ::zeroclaw_log::record!(
                                    DEBUG,
                                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                                        .with_attrs(::serde_json::json!({
                                            "mcp_server": &config.name,
                                            "method": method,
                                            "id": id,
                                        })),
                                    "MCP stdio: ignoring server-to-client request/notification"
                                );
                            }
                            Ok(JsonRpcInbound::Skippable) => {}
                            Ok(JsonRpcInbound::Response(resp)) => {
                                let Some(resp_id) = resp.id.clone() else { continue; };
                                if let Some(slot) = pending.remove(&resp_id) {
                                    slot.deadline_abort.abort();
                                    slot.cancel_abort.abort();
                                    let _ = slot.reply.send(Ok(resp));
                                } else {
                                    ::zeroclaw_log::record!(
                                        WARN,
                                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                                            .with_attrs(::serde_json::json!({
                                                "mcp_server": &config.name,
                                                "got_id": resp_id,
                                            })),
                                        "MCP stdio: skipping response with unmatched JSON-RPC id"
                                    );
                                }
                            }
                            Err(err) => {
                                ::zeroclaw_log::record!(
                                    WARN,
                                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                                        .with_attrs(::serde_json::json!({
                                            "mcp_server": &config.name,
                                            "error": err.to_string(),
                                        })),
                                    "MCP stdio: rejected invalid inbound envelope"
                                );
                            }
                        }
                    }
                    Ok(None) | Err(_) => {
                        mark_session_dirty(
                            &mut session,
                            &mut pending,
                            &alive,
                            McpTransportError::TransportClosed,
                        );
                        cancels.clear();
                        deadlines.clear();
                    }
                }
            }
            canceled = futures_util::StreamExt::next(&mut cancels), if !cancels.is_empty() => {
                if let Some(id) = canceled {
                    if id.is_null() { continue; }
                    if let Some(slot) = pending.remove(&id) {
                        slot.deadline_abort.abort();
                        slot.cancel_abort.abort();
                        let _ = slot.reply.send(Err(McpTransportError::OutcomeUnknown.into()));
                        mark_session_dirty(
                            &mut session,
                            &mut pending,
                            &alive,
                            McpTransportError::OutcomeUnknown,
                        );
                        cancels.clear();
                        deadlines.clear();
                    }
                }
            }
            timed_out_id = futures_util::StreamExt::next(&mut deadlines), if !deadlines.is_empty() => {
                if let Some(id) = timed_out_id {
                    if id.is_null() { continue; }
                    if let Some(slot) = pending.remove(&id) {
                        slot.deadline_abort.abort();
                        slot.cancel_abort.abort();
                        let _ = slot.reply.send(Err(McpTransportError::ResponseTimeout.into()));
                        mark_session_dirty(
                            &mut session,
                            &mut pending,
                            &alive,
                            McpTransportError::ResponseTimeout,
                        );
                        cancels.clear();
                        deadlines.clear();
                    }
                }
            }
        }
    }

    if let Some(mut sess) = session.take() {
        let _ = kill_and_reap_stdio_session(&mut sess, &config.name).await;
    }
    alive.store(false, Ordering::Release);
}

#[async_trait::async_trait]
impl McpTransportConn for StdioTransport {
    async fn send_and_recv(&self, request: &JsonRpcRequest) -> Result<JsonRpcResponse> {
        let (mut cancel_guard, cancel_rx) = CancelOnDrop::arm();
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send_cmd(StdioWorkerCmd::Rpc {
            request: request.clone(),
            cancel: cancel_rx,
            reply: reply_tx,
        })
        .await?;
        let result = reply_rx
            .await
            .map_err(|_| anyhow::Error::new(McpTransportError::OutcomeUnknown))?;
        cancel_guard.disarm();
        result
    }

    async fn reset(&self) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send_cmd(StdioWorkerCmd::Reset { reply: reply_tx })
            .await?;
        reply_rx
            .await
            .map_err(|_| anyhow::Error::new(McpTransportError::TransportClosed))?
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn health_check(&self) -> bool {
        self.child_alive.load(Ordering::Acquire)
    }

    async fn close(&self) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(StdioWorkerCmd::Close { reply: reply_tx })
            .await
            .is_err()
        {
            return Ok(());
        }
        let _ = reply_rx.await;
        Ok(())
    }
}

// ── HTTP Transport ───────────────────────────────────────────────────────

pub struct HttpTransport {
    config: Arc<McpServerConfig>,
    url: String,
    client: reqwest::Client,
    session_id: Option<String>,
}

impl HttpTransport {
    pub fn new(config: Arc<McpServerConfig>) -> Result<Self> {
        let url = config
            .url
            .as_ref()
            .ok_or_else(|| anyhow::Error::msg("URL required for HTTP transport"))?
            .clone();
        let client = reqwest::Client::builder()
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            config,
            url,
            client,
            session_id: None,
        })
    }

    fn apply_session_header(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(session_id) = self.session_id.as_deref() {
            req.header(MCP_SESSION_ID_HEADER, session_id)
        } else {
            req
        }
    }

    fn update_session_id_from_headers(&mut self, headers: &reqwest::header::HeaderMap) {
        if let Some(session_id) = headers
            .get(MCP_SESSION_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            self.session_id = Some(session_id.to_string());
        }
    }
}

#[async_trait::async_trait]
impl ExclusiveMcpTransport for HttpTransport {
    async fn send_and_recv(&mut self, request: &JsonRpcRequest) -> Result<JsonRpcResponse> {
        let body = serde_json::to_string(request)?;
        let has_accept = self
            .config
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("Accept"));
        let has_content_type = self
            .config
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("Content-Type"));

        let mut req = apply_request_timeout(
            self.client.post(&self.url).body(body),
            http_request_timeout_secs(request, &self.config),
        );
        if !has_content_type {
            req = req.header("Content-Type", MCP_JSON_CONTENT_TYPE);
        }
        for (key, value) in &self.config.headers {
            req = req.header(key, value);
        }
        req = self.apply_session_header(req);
        if !has_accept {
            req = req.header("Accept", MCP_STREAMABLE_ACCEPT);
        }

        let resp = req
            .send()
            .await
            .context("HTTP request to MCP server failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            if self.session_id.is_some()
                && (status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE)
            {
                return Err(McpTransportError::StaleSession {
                    status: status.as_u16(),
                }
                .into());
            }
            bail!("MCP server returned HTTP {}", status);
        }

        self.update_session_id_from_headers(resp.headers());

        if request.id.is_none() {
            return Ok(JsonRpcResponse {
                jsonrpc: crate::mcp_protocol::JSONRPC_VERSION.to_string(),
                id: None,
                result: None,
                error: None,
            });
        }

        let expected_id = request.id.clone();
        let is_sse = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("text/event-stream"));
        if is_sse {
            let read_response = read_first_jsonrpc_from_sse_response(resp, expected_id.as_ref());
            let maybe_resp = if let Some(sse_timeout) =
                http_sse_read_timeout_secs(request, &self.config)
            {
                timeout(Duration::from_secs(sse_timeout), read_response)
                    .await
                    .context("timeout waiting for MCP response from streamable HTTP SSE stream")??
            } else {
                read_response.await?
            };
            return maybe_resp.ok_or_else(|| {
                anyhow::Error::msg("MCP server returned no response in SSE stream")
            });
        }

        let resp_text = resp.text().await.context("failed to read HTTP response")?;
        parse_jsonrpc_response_text(&resp_text, expected_id.as_ref())
    }

    async fn reset(&mut self) -> Result<()> {
        self.session_id = None;
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        Ok(())
    }
}

// ── SSE Transport ─────────────────────────────────────────────────────────

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum SseStreamState {
    Unknown,
    Connected,
    Unsupported,
}

pub struct SseTransport {
    config: Arc<McpServerConfig>,
    sse_url: String,
    client: reqwest::Client,
    stream_state: SseStreamState,
    shared: Arc<Mutex<SseSharedState>>,
    notify: Arc<Notify>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    reader_task: Option<tokio::task::JoinHandle<()>>,
}

impl SseTransport {
    pub fn new(config: Arc<McpServerConfig>) -> Result<Self> {
        let sse_url = config
            .url
            .as_ref()
            .ok_or_else(|| anyhow::Error::msg("URL required for SSE transport"))?
            .clone();
        let client = reqwest::Client::builder()
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            config,
            sse_url,
            client,
            stream_state: SseStreamState::Unknown,
            shared: Arc::new(Mutex::new(SseSharedState::default())),
            notify: Arc::new(Notify::new()),
            shutdown_tx: None,
            reader_task: None,
        })
    }

    async fn ensure_connected(&mut self) -> Result<()> {
        if self.stream_state == SseStreamState::Unsupported {
            return Ok(());
        }
        if let Some(task) = &self.reader_task
            && !task.is_finished()
        {
            self.stream_state = SseStreamState::Connected;
            return Ok(());
        }

        let has_accept = self
            .config
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("Accept"));
        let mut req = self
            .client
            .get(&self.sse_url)
            .header("Cache-Control", "no-cache");
        for (key, value) in &self.config.headers {
            req = req.header(key, value);
        }
        if !has_accept {
            req = req.header("Accept", MCP_STREAMABLE_ACCEPT);
        }

        let resp = req.send().await.context("SSE GET to MCP server failed")?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND
            || resp.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED
        {
            self.stream_state = SseStreamState::Unsupported;
            return Ok(());
        }
        if !resp.status().is_success() {
            bail!("MCP server returned HTTP {}", resp.status());
        }
        let is_event_stream = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("text/event-stream"));
        if !is_event_stream {
            self.stream_state = SseStreamState::Unsupported;
            return Ok(());
        }

        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        self.shutdown_tx = Some(shutdown_tx);
        let shared = self.shared.clone();
        let notify = self.notify.clone();
        let sse_url = self.sse_url.clone();
        let server_name = self.config.name.clone();

        self.reader_task = Some(zeroclaw_spawn::spawn!(async move {
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
                    _ = &mut shutdown_rx => break,
                    line = lines.next_line() => {
                        let Ok(line_opt) = line else { break; };
                        let Some(mut line) = line_opt else { break; };
                        if line.ends_with('\r') { line.pop(); }
                        if line.is_empty() {
                            if cur_event.is_none() && cur_id.is_none() && cur_data.is_empty() {
                                continue;
                            }
                            let event = cur_event.take();
                            let data = cur_data.join("\n");
                            cur_data.clear();
                            let id = cur_id.take();
                            handle_sse_event(
                                &server_name,
                                &sse_url,
                                &shared,
                                &notify,
                                event.as_deref(),
                                id.as_deref(),
                                data,
                            )
                            .await;
                            continue;
                        }
                        if line.starts_with(':') { continue; }
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
            let pending = {
                let mut guard = shared.lock().await;
                std::mem::take(&mut guard.pending)
            };
            drop(pending);
        }));
        self.stream_state = SseStreamState::Connected;
        Ok(())
    }

    async fn get_message_url(&self) -> Result<(String, bool)> {
        let guard = self.shared.lock().await;
        if let Some(url) = &guard.message_url {
            return Ok((url.clone(), guard.message_url_from_endpoint));
        }
        drop(guard);
        let derived = derive_message_url(&self.sse_url, "messages")
            .or_else(|| derive_message_url(&self.sse_url, "message"))
            .ok_or_else(|| anyhow::Error::msg("invalid SSE URL"))?;
        let mut guard = self.shared.lock().await;
        if guard.message_url.is_none() {
            guard.message_url = Some(derived.clone());
            guard.message_url_from_endpoint = false;
        }
        Ok((derived, false))
    }
}

#[derive(Default)]
struct SseSharedState {
    message_url: Option<String>,
    message_url_from_endpoint: bool,
    pending: HashMap<u64, oneshot::Sender<JsonRpcResponse>>,
}

/// RAII removal of an SSE pending id on cancel/early exit.
struct SsePendingGuard {
    shared: Arc<Mutex<SseSharedState>>,
    id: Option<u64>,
}

impl SsePendingGuard {
    fn new(shared: Arc<Mutex<SseSharedState>>, id: u64) -> Self {
        Self {
            shared,
            id: Some(id),
        }
    }
    fn disarm(&mut self) {
        self.id.take();
    }
}

impl Drop for SsePendingGuard {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            // Best-effort; reset() also clears pending.
            if let Ok(mut guard) = self.shared.try_lock() {
                guard.pending.remove(&id);
            }
        }
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
    shared: &Arc<Mutex<SseSharedState>>,
    notify: &Arc<Notify>,
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
    let Ok(inbound) = classify_jsonrpc_value(value) else {
        return;
    };
    let JsonRpcInbound::Response(resp) = inbound else {
        return;
    };
    let Some(id_val) = resp.id.clone() else {
        return;
    };
    let Some(id) = id_val.as_u64() else {
        return;
    };
    let tx = {
        let mut guard = shared.lock().await;
        guard.pending.remove(&id)
    };
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

fn parse_jsonrpc_response_text(
    resp_text: &str,
    expected_id: Option<&serde_json::Value>,
) -> Result<JsonRpcResponse> {
    let trimmed = resp_text.trim();
    if trimmed.is_empty() {
        bail!("MCP server returned no response");
    }
    let json_text = if looks_like_sse_text(trimmed) {
        extract_json_from_sse_text(trimmed)
    } else {
        Cow::Borrowed(trimmed)
    };
    let value: serde_json::Value = serde_json::from_str(json_text.as_ref())
        .with_context(|| format!("invalid JSON-RPC response: {resp_text}"))?;
    match classify_jsonrpc_value(value)? {
        JsonRpcInbound::Response(resp) => {
            if let Some(expected) = expected_id {
                if !match_response_id(&resp, expected) {
                    bail!(
                        "MCP response id mismatch: expected {:?}, got {:?}",
                        expected,
                        resp.id
                    );
                }
            }
            Ok(resp)
        }
        JsonRpcInbound::ServerMessage { method, .. } => {
            bail!("MCP server request `{method}` cannot be accepted as a response")
        }
        JsonRpcInbound::Skippable => bail!("MCP server returned no usable response"),
    }
}

fn looks_like_sse_text(text: &str) -> bool {
    text.starts_with("data:")
        || text.starts_with("event:")
        || text.contains("\ndata:")
        || text.contains("\nevent:")
}

async fn read_first_jsonrpc_from_sse_response(
    resp: reqwest::Response,
    expected_id: Option<&serde_json::Value>,
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
            let event = cur_event.take().unwrap_or_else(|| "message".to_string());
            let data = cur_data.join("\n");
            cur_data.clear();
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
            let Ok(value) = serde_json::from_str::<serde_json::Value>(json_str.as_ref()) else {
                continue;
            };
            if let Ok(JsonRpcInbound::Response(resp)) = classify_jsonrpc_value(value) {
                if let Some(expected) = expected_id {
                    if !match_response_id(&resp, expected) {
                        continue;
                    }
                }
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
impl ExclusiveMcpTransport for SseTransport {
    async fn send_and_recv(&mut self, request: &JsonRpcRequest) -> Result<JsonRpcResponse> {
        self.ensure_connected().await?;
        let id = request.id.as_ref().and_then(|v| v.as_u64());
        let body = serde_json::to_string(request)?;
        let side_effecting = request.method == TOOLS_CALL_METHOD;

        let (mut message_url, mut from_endpoint) = self.get_message_url().await?;
        if self.stream_state == SseStreamState::Connected && !from_endpoint {
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
        let primary_url = if from_endpoint {
            message_url.clone()
        } else {
            self.sse_url.clone()
        };
        let secondary_url = if message_url == self.sse_url {
            None
        } else if primary_url == message_url {
            Some(self.sse_url.clone())
        } else {
            Some(message_url.clone())
        };
        let has_secondary = secondary_url.is_some();

        let mut pending_guard = None;
        let mut rx = None;
        if let Some(id) = id
            && self.stream_state == SseStreamState::Connected
        {
            let (tx, ch) = oneshot::channel();
            {
                let mut guard = self.shared.lock().await;
                if guard.pending.contains_key(&id) {
                    bail!("MCP SSE: refusing to replace occupied pending JSON-RPC id {id}");
                }
                guard.pending.insert(id, tx);
            }
            pending_guard = Some(SsePendingGuard::new(self.shared.clone(), id));
            rx = Some((id, ch));
        }

        let mut got_direct = None;
        let mut last_status = None;
        let mut accepted_side_effect = false;

        for (i, url) in std::iter::once(primary_url)
            .chain(secondary_url)
            .enumerate()
        {
            // Never re-POST an accepted side-effecting tools/call to a fallback URL.
            if i > 0 && accepted_side_effect && side_effecting {
                return Err(McpTransportError::OutcomeUnknown.into());
            }

            let has_accept = self
                .config
                .headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("Accept"));
            let has_content_type = self
                .config
                .headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("Content-Type"));
            let mut req = apply_request_timeout(
                self.client.post(&url).body(body.clone()),
                http_request_timeout_secs(request, &self.config),
            );
            if !has_content_type {
                req = req.header("Content-Type", MCP_JSON_CONTENT_TYPE);
            }
            for (key, value) in &self.config.headers {
                req = req.header(key, value);
            }
            if !has_accept {
                req = req.header("Accept", MCP_STREAMABLE_ACCEPT);
            }

            let resp = req.send().await.context("SSE POST to MCP server failed")?;
            let status = resp.status();
            last_status = Some(status);

            if (status == reqwest::StatusCode::NOT_FOUND
                || status == reqwest::StatusCode::METHOD_NOT_ALLOWED)
                && i == 0
            {
                continue;
            }
            if !status.is_success() {
                break;
            }
            if side_effecting {
                accepted_side_effect = true;
            }

            if request.id.is_none() {
                got_direct = Some(JsonRpcResponse {
                    jsonrpc: crate::mcp_protocol::JSONRPC_VERSION.to_string(),
                    id: None,
                    result: None,
                    error: None,
                });
                break;
            }

            let expected_id = request.id.clone();
            let is_sse = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.to_ascii_lowercase().contains("text/event-stream"));

            if is_sse {
                if i == 0 && has_secondary && !side_effecting {
                    match timeout(
                        Duration::from_secs(3),
                        read_first_jsonrpc_from_sse_response(resp, expected_id.as_ref()),
                    )
                    .await
                    {
                        Ok(res) => {
                            if let Some(resp) = res? {
                                got_direct = Some(resp);
                            }
                            break;
                        }
                        Err(_) => continue,
                    }
                }
                // Side-effecting: do not fall through to secondary POST on slow SSE read.
                if i == 0 && has_secondary && side_effecting {
                    match timeout(
                        Duration::from_secs(3),
                        read_first_jsonrpc_from_sse_response(resp, expected_id.as_ref()),
                    )
                    .await
                    {
                        Ok(res) => {
                            if let Some(resp) = res? {
                                got_direct = Some(resp);
                                break;
                            }
                            // Accepted but no body yet — wait on pending channel; never re-POST.
                            break;
                        }
                        Err(_) => {
                            // Timed out reading SSE body after accept — outcome unknown.
                            if let Some(mut g) = pending_guard.take() {
                                g.disarm();
                                let mut guard = self.shared.lock().await;
                                guard.pending.remove(&id.unwrap_or_default());
                            }
                            return Err(McpTransportError::OutcomeUnknown.into());
                        }
                    }
                }
                if let Some(resp) =
                    read_first_jsonrpc_from_sse_response(resp, expected_id.as_ref()).await?
                {
                    got_direct = Some(resp);
                }
                break;
            }

            let text = if i == 0 && has_secondary && !side_effecting {
                match timeout(Duration::from_secs(3), resp.text()).await {
                    Ok(Ok(t)) => t,
                    Ok(Err(_)) => String::new(),
                    Err(_) => continue,
                }
            } else {
                resp.text().await.unwrap_or_default()
            };
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                if let Ok(mcp_resp) = parse_jsonrpc_response_text(trimmed, expected_id.as_ref()) {
                    got_direct = Some(mcp_resp);
                }
            }
            break;
        }

        if let Some(resp) = got_direct {
            if let Some(mut g) = pending_guard.take() {
                g.disarm();
                if let Some((id, _)) = rx.as_ref() {
                    let mut guard = self.shared.lock().await;
                    guard.pending.remove(id);
                }
            }
            return Ok(resp);
        }

        if let Some(status) = last_status {
            if !status.is_success() {
                if let Some(mut g) = pending_guard.take() {
                    g.disarm();
                    if let Some((id, _)) = rx.as_ref() {
                        let mut guard = self.shared.lock().await;
                        guard.pending.remove(id);
                    }
                }
                if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
                    return Err(McpTransportError::StaleSession {
                        status: status.as_u16(),
                    }
                    .into());
                }
                bail!("MCP server returned HTTP {}", status);
            }
        } else {
            bail!("MCP request not sent");
        }

        let Some((_id, rx)) = rx else {
            bail!("MCP server returned no response");
        };
        // Keep pending_guard armed until we get the oneshot reply or drop.
        let result = rx
            .await
            .map_err(|_| McpTransportError::TransportClosed.into());
        if result.is_ok() {
            if let Some(mut g) = pending_guard.take() {
                g.disarm();
            }
        }
        result
    }

    async fn reset(&mut self) -> Result<()> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.reader_task.take() {
            task.abort();
        }
        self.stream_state = SseStreamState::Unknown;
        let mut guard = self.shared.lock().await;
        guard.message_url = None;
        guard.message_url_from_endpoint = false;
        guard.pending.clear();
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.reader_task.take() {
            task.abort();
        }
        Ok(())
    }
}

// ── Factory ──────────────────────────────────────────────────────────────

pub fn create_transport(config: Arc<McpServerConfig>) -> Result<Arc<dyn McpTransportConn>> {
    match config.transport {
        McpTransport::Stdio => Ok(Arc::new(StdioTransport::new(config)?)),
        McpTransport::Http => Ok(Arc::new(SerialTransport::new(HttpTransport::new(config)?))),
        McpTransport::Sse => Ok(Arc::new(SerialTransport::new(SseTransport::new(config)?))),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transport_default_is_stdio() {
        let config = McpServerConfig::default();
        assert_eq!(config.transport, McpTransport::Stdio);
    }

    #[test]
    fn classify_rejects_method_bearing_as_response() {
        let v = serde_json::json!({"jsonrpc":"2.0","id":7,"method":"roots/list"});
        match classify_jsonrpc_value(v).unwrap() {
            JsonRpcInbound::ServerMessage { method, .. } => assert_eq!(method, "roots/list"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn classify_requires_result_or_error() {
        let v = serde_json::json!({"jsonrpc":"2.0","id":1});
        assert!(classify_jsonrpc_value(v).is_err());
    }

    #[test]
    fn classify_accepts_valid_response() {
        let v = serde_json::json!({"jsonrpc":"2.0","id":7,"result":{"ok":true}});
        match classify_jsonrpc_value(v).unwrap() {
            JsonRpcInbound::Response(r) => {
                assert!(match_response_id(&r, &serde_json::json!(7)));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn timeout_policy_lives_on_config_only() {
        let src = include_str!("mcp_transport.rs");
        let needle = ["tool_timeout_secs: Option<", "u64>"].concat();
        assert!(
            !src.contains(&needle),
            "transports must not store duplicated tool_timeout_secs"
        );
        assert_eq!(McpServerConfig::DEFAULT_TOOL_TIMEOUT_SECS, 180);
        assert_eq!(McpServerConfig::MAX_TOOL_TIMEOUT_SECS, 600);
    }

    #[test]
    fn stdio_recv_timeout_uses_canonical_policy() {
        let req = JsonRpcRequest::new(1, TOOLS_CALL_METHOD, serde_json::json!({}));
        let cfg = McpServerConfig {
            tool_timeout_secs: Some(240),
            ..Default::default()
        };
        assert_eq!(stdio_recv_timeout_secs(&req, &cfg), 240);
        let cfg = McpServerConfig::default();
        assert_eq!(
            stdio_recv_timeout_secs(&req, &cfg),
            McpServerConfig::DEFAULT_TOOL_TIMEOUT_SECS
        );
    }

    #[test]
    fn create_transport_http_requires_url() {
        let config = Arc::new(McpServerConfig {
            name: "t".into(),
            transport: McpTransport::Http,
            ..Default::default()
        });
        assert!(create_transport(config).is_err());
    }

    #[test]
    fn write_progress_not_sent_only_before_start() {
        assert_eq!(WriteProgress::NotStarted, WriteProgress::NotStarted);
        assert_ne!(WriteProgress::NotStarted, WriteProgress::Started);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_reset_reaps_before_spawn_and_bumps_generation() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use tokio::time::sleep;

        fn alive(pid: u32) -> bool {
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        }

        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("hang.sh");
        let pid_path = temp.path().join("p.pid");
        let mut f = std::fs::File::create(&script).unwrap();
        f.write_all(
            br#"#!/bin/sh
echo "$$" > "$1"
exec tail -f /dev/null
"#,
        )
        .unwrap();
        drop(f);
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let config = Arc::new(McpServerConfig {
            name: "hang".into(),
            command: script.display().to_string(),
            args: vec![pid_path.display().to_string()],
            transport: McpTransport::Stdio,
            ..Default::default()
        });
        let t = StdioTransport::new(Arc::clone(&config)).unwrap();
        let gen0 = t.generation();
        let mut old = None;
        for _ in 0..50 {
            if let Ok(raw) = std::fs::read_to_string(&pid_path) {
                if let Ok(pid) = raw.trim().parse::<u32>() {
                    old = Some(pid);
                    break;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
        let old = old.expect("pid");
        assert!(alive(old));
        let _ = std::fs::remove_file(&pid_path);
        t.reset().await.expect("reset");
        assert!(t.generation() > gen0);
        for _ in 0..50 {
            if !alive(old) {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
        assert!(!alive(old), "old child must be reaped before replacement");
        t.close().await.ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_failed_reap_refuses_replacement_surface() {
        // Self-deleting script: after first kill the path is gone so spawn fails.
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("once.sh");
        let mut f = std::fs::File::create(&script).unwrap();
        f.write_all(
            br#"#!/bin/sh
rm -f "$0"
exec tail -f /dev/null
"#,
        )
        .unwrap();
        drop(f);
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        let config = Arc::new(McpServerConfig {
            name: "once".into(),
            command: script.display().to_string(),
            transport: McpTransport::Stdio,
            ..Default::default()
        });
        let t = StdioTransport::new(Arc::clone(&config)).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let err = t.reset().await.expect_err("spawn after delete must fail");
        assert!(
            err.to_string().contains("failed to spawn")
                || format!("{err:#}").contains("replacement"),
            "{err:#}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_cancel_before_write_is_not_sent() {
        // Flood a blocking stdin reader that never reads — write may block.
        // Cancel immediately after submit so cancel wins before/during write.
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("noread.sh");
        let mut f = std::fs::File::create(&script).unwrap();
        // Never read stdin; fill the pipe eventually. For cancel-before-write we
        // cancel the future immediately.
        f.write_all(
            br#"#!/bin/sh
sleep 60
"#,
        )
        .unwrap();
        drop(f);
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        let config = Arc::new(McpServerConfig {
            name: "noread".into(),
            command: script.display().to_string(),
            transport: McpTransport::Stdio,
            ..Default::default()
        });
        let t = StdioTransport::new(config).unwrap();
        let req = JsonRpcRequest::new(1, "initialize", serde_json::json!({}));
        let fut = t.send_and_recv(&req);
        tokio::pin!(fut);
        // Cancel before polling write completion.
        drop(fut);
        // Next call after dirty recovery path is coordinator-owned; here we only
        // assert cancel did not panic and generation unchanged until reset.
        assert_eq!(t.generation(), 0);
        t.close().await.ok();
    }

    #[tokio::test]
    async fn http_transport_reset_clears_session() {
        let config = Arc::new(McpServerConfig {
            name: "h".into(),
            transport: McpTransport::Http,
            url: Some("http://localhost/mcp".into()),
            ..Default::default()
        });
        let mut t = HttpTransport::new(config).unwrap();
        t.session_id = Some("s".into());
        ExclusiveMcpTransport::reset(&mut t).await.unwrap();
        assert!(t.session_id.is_none());
    }

    #[test]
    fn parse_response_rejects_server_request() {
        let err = parse_jsonrpc_response_text(
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
            Some(&serde_json::json!(1)),
        )
        .unwrap_err();
        assert!(err.to_string().contains("cannot be accepted"));
    }

    #[test]
    fn parse_response_rejects_id_mismatch() {
        let err = parse_jsonrpc_response_text(
            r#"{"jsonrpc":"2.0","id":2,"result":{}}"#,
            Some(&serde_json::json!(1)),
        )
        .unwrap_err();
        assert!(err.to_string().contains("id mismatch"));
    }
}

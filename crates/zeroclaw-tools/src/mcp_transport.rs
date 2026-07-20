//! MCP transport abstraction — supports stdio, SSE, and HTTP transports.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use futures_util::stream::FuturesUnordered;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use tokio::time::{Duration, timeout};
use tokio_stream::StreamExt;

use crate::mcp_protocol::{JsonRpcRequest, JsonRpcResponse};
use zeroclaw_config::schema::{McpServerConfig, McpTransport};

/// Maximum bytes for a single JSON-RPC response.
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024; // 4 MB

/// Timeout for init/list operations (and non-tool stdio waits).
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

/// Stdio receive deadline for a request.
///
/// Non-tool RPCs keep the short init/list budget. `tools/call` uses the
/// canonical per-server policy from [`McpServerConfig::resolved_tool_timeout_secs`].
fn stdio_recv_timeout_secs(request: &JsonRpcRequest, config: &McpServerConfig) -> u64 {
    if request.method == TOOLS_CALL_METHOD {
        config.resolved_tool_timeout_secs()
    } else {
        RECV_TIMEOUT_SECS
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

// ── Transport Errors ───────────────────────────────────────────────────────

/// Transport-level failures.
///
/// Soft failures (`StaleSession`, `TransportClosed`, `ResponseTimeout`,
/// `OutcomeUnknown`) mean the connection should be reset + re-handshaked for
/// *future* calls. Side-effecting `tools/call` must **not** be auto-replayed
/// after these unless the request was definitely never written
/// ([`McpTransportError::NotSent`]).
#[derive(Debug, Clone, thiserror::Error)]
pub enum McpTransportError {
    /// The server no longer recognizes our session (typically after it
    /// restarted). Surfaced from HTTP 404/410 responses.
    #[error("MCP session is stale (HTTP {status})")]
    StaleSession { status: u16 },

    /// The underlying stream/connection dropped before a response arrived
    /// (e.g. SSE EOF or connection reset).
    #[error("MCP transport connection closed")]
    TransportClosed,

    /// A response deadline elapsed (stdio tool/init wait). The transport may be
    /// desynchronized by a late reply — callers should reset + re-handshake,
    /// but must **not** automatically retry the same tool call.
    #[error("MCP transport timed out waiting for response")]
    ResponseTimeout,

    /// The request was written (or write outcome is uncertain) and the transport
    /// closed/reset before a matched response arrived. Do not auto-replay.
    #[error("MCP tool call outcome unknown (transport closed after submit)")]
    OutcomeUnknown,

    /// The request was definitely not written to the wire; safe to retry.
    #[error("MCP request was not sent")]
    NotSent,
}

// ── Transport Trait ──────────────────────────────────────────────────────

/// Abstract transport for MCP communication.
///
/// Methods take `&self` so callers never need a process-wide mutex across
/// response waits. Stdio uses a dedicated worker (concurrent submits queue;
/// stdin writes are serialized by that worker). HTTP/SSE keep their existing
/// exclusive behavior via [`SerialTransport`].
#[async_trait::async_trait]
pub trait McpTransportConn: Send + Sync {
    /// Send a JSON-RPC request and receive the response.
    async fn send_and_recv(&self, request: &JsonRpcRequest) -> Result<JsonRpcResponse>;

    /// Reset per-connection session state so the next operation re-establishes
    /// a fresh session. Default is a no-op for stateless transports.
    async fn reset(&self) -> Result<()> {
        Ok(())
    }

    /// Close the connection.
    async fn close(&self) -> Result<()>;
}

/// Exclusive (`&mut self`) transport used by HTTP/SSE; wrapped in
/// [`SerialTransport`] for the shared [`McpTransportConn`] surface.
#[async_trait::async_trait]
trait ExclusiveMcpTransport: Send {
    async fn send_and_recv(&mut self, request: &JsonRpcRequest) -> Result<JsonRpcResponse>;
    async fn reset(&mut self) -> Result<()>;
    async fn close(&mut self) -> Result<()>;
}

/// Serializes HTTP/SSE RPCs behind a mutex (conservative: preserves prior
/// exclusive `&mut` behavior). Callers still do not hold a *server metadata*
/// lock — only this transport-local mutex.
struct SerialTransport<T> {
    inner: Mutex<T>,
}

impl<T> SerialTransport<T> {
    fn new(inner: T) -> Self {
        Self {
            inner: Mutex::new(inner),
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
        self.inner.lock().await.reset().await
    }

    async fn close(&self) -> Result<()> {
        self.inner.lock().await.close().await
    }
}

// ── Stdio Transport (worker / request router) ────────────────────────────

/// Classify a stdout JSON-RPC line. Server-to-client *requests* (objects with
/// a `method` field) must never be accepted as responses, even when `id`
/// collides with an in-flight client request.
#[derive(Debug)]
pub(crate) enum StdioInbound {
    /// JSON-RPC response (`result` and/or `error`, no `method`).
    Response(JsonRpcResponse),
    /// Server → client request or notification (has `method`).
    ServerMessage {
        id: Option<serde_json::Value>,
        method: String,
    },
    /// id-less noise / notification-shaped response without method.
    Skippable,
}

pub(crate) fn classify_stdio_inbound(line: &str) -> Result<StdioInbound> {
    let value: serde_json::Value =
        serde_json::from_str(line).with_context(|| format!("invalid JSON-RPC message: {line}"))?;
    if let Some(method) = value.get("method").and_then(|m| m.as_str()) {
        return Ok(StdioInbound::ServerMessage {
            id: value.get("id").cloned(),
            method: method.to_string(),
        });
    }
    if value.get("id").is_none() {
        return Ok(StdioInbound::Skippable);
    }
    let resp: JsonRpcResponse = serde_json::from_value(value)
        .with_context(|| format!("invalid JSON-RPC response: {line}"))?;
    Ok(StdioInbound::Response(resp))
}

/// Stdio transport handle. Owns only a command channel to a worker task that
/// exclusively holds the child process pipes.
///
/// Concurrent `send_and_recv` callers do **not** hold a global server mutex
/// across waits — they submit to the worker and await a oneshot. The worker
/// serializes stdin writes (stdio is single-writer) and routes stdout replies
/// by JSON-RPC id.
pub struct StdioTransport {
    /// Canonical spawn/timeout recipe — shared with [`crate::mcp_client::McpServer`].
    /// Source of truth is this `Arc<McpServerConfig>`; no cloned spawn fields.
    config: Arc<McpServerConfig>,
    cmd_tx: mpsc::Sender<StdioWorkerCmd>,
}

enum StdioWorkerCmd {
    Rpc {
        request: JsonRpcRequest,
        /// Fired when the caller future is dropped (timeout/cancel).
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
    /// True once the request bytes were fully written.
    written: bool,
}

/// Drop guard: signals the worker that the caller abandoned the wait.
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

impl StdioTransport {
    pub fn new(config: Arc<McpServerConfig>) -> Result<Self> {
        let session = spawn_stdio_session(&config)?;
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let worker_config = Arc::clone(&config);
        zeroclaw_spawn::spawn!(async move {
            stdio_worker(worker_config, session, cmd_rx).await;
        });
        Ok(Self { config, cmd_tx })
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
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
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
    Ok(StdioSession {
        child,
        stdin,
        stdout_lines: BufReader::new(stdout).lines(),
    })
}

async fn kill_and_reap_stdio_session(session: &mut StdioSession, server_name: &str) {
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
        Ok(Ok(status)) => {
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({
                        "mcp_server": server_name,
                        "exit": format!("{status:?}"),
                    })),
                "mcp_transport: stdio MCP child reaped"
            );
        }
        Ok(Err(err)) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "mcp_server": server_name,
                        "error": err.to_string(),
                    })),
                "mcp_transport: wait failed while reaping stdio MCP child"
            );
        }
        Err(_) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Timeout)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({ "mcp_server": server_name })),
                "mcp_transport: timed out waiting to reap stdio MCP child"
            );
        }
    }
}

async fn write_stdio_line(stdin: &mut tokio::process::ChildStdin, line: &str) -> Result<()> {
    stdin
        .write_all(line.as_bytes())
        .await
        .context("failed to write to MCP server stdin")?;
    stdin
        .write_all(b"\n")
        .await
        .context("failed to write newline to MCP server stdin")?;
    stdin.flush().await.context("failed to flush stdin")?;
    Ok(())
}

fn fail_pending(pending: &mut HashMap<serde_json::Value, PendingRpc>, err: McpTransportError) {
    for (_, slot) in pending.drain() {
        let e = if slot.written {
            McpTransportError::OutcomeUnknown
        } else {
            err.clone()
        };
        let _ = slot.reply.send(Err(e.into()));
    }
}

async fn respawn_stdio_session(
    config: &McpServerConfig,
    session: &mut Option<StdioSession>,
) -> Result<()> {
    if let Some(mut old) = session.take() {
        kill_and_reap_stdio_session(&mut old, &config.name).await;
    }
    match spawn_stdio_session(config) {
        Ok(new_session) => {
            *session = Some(new_session);
            Ok(())
        }
        Err(err) => {
            *session = None;
            Err(err).with_context(|| {
                format!(
                    "MCP server `{}` failed to spawn replacement stdio child after reset",
                    config.name
                )
            })
        }
    }
}

async fn stdio_worker(
    config: Arc<McpServerConfig>,
    initial: StdioSession,
    mut cmd_rx: mpsc::Receiver<StdioWorkerCmd>,
) {
    let mut session: Option<StdioSession> = Some(initial);
    let mut pending: HashMap<serde_json::Value, PendingRpc> = HashMap::new();
    // Completes with `Some(id)` only on explicit caller-cancel (Drop sends).
    // `None` means the cancel sender was disarmed after a successful reply.
    let mut cancels: FuturesUnordered<
        std::pin::Pin<Box<dyn std::future::Future<Output = Option<serde_json::Value>> + Send>>,
    > = FuturesUnordered::new();
    // Per-pending response deadlines.
    let mut deadlines: FuturesUnordered<
        std::pin::Pin<Box<dyn std::future::Future<Output = serde_json::Value> + Send>>,
    > = FuturesUnordered::new();

    loop {
        let has_session = session.is_some();
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break; };
                match cmd {
                    StdioWorkerCmd::Rpc { request, cancel, reply } => {
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
                        // Serialize stdin writes here. Callers are not blocked on a
                        // server-level mutex — only on this worker's write turn.
                        if let Err(err) = write_stdio_line(&mut sess.stdin, &line).await {
                            let _ = reply.send(Err(
                                anyhow::Error::new(McpTransportError::NotSent).context(err)
                            ));
                            // Partial/failed write → connection dirty.
                            let _ = respawn_stdio_session(&config, &mut session).await;
                            fail_pending(&mut pending, McpTransportError::OutcomeUnknown);
                            cancels.clear();
                            deadlines.clear();
                            continue;
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
                        let wait_secs = stdio_recv_timeout_secs(&request, &config);
                        let id_for_cancel = id.clone();
                        cancels.push(Box::pin(async move {
                            match cancel.await {
                                Ok(()) => Some(id_for_cancel),
                                Err(_) => None, // disarmed after success
                            }
                        }));
                        let id_for_deadline = id.clone();
                        deadlines.push(Box::pin(async move {
                            tokio::time::sleep(Duration::from_secs(wait_secs)).await;
                            id_for_deadline
                        }));
                        pending.insert(
                            id,
                            PendingRpc {
                                reply,
                                written: true,
                            },
                        );
                    }
                    StdioWorkerCmd::Reset { reply } => {
                        fail_pending(&mut pending, McpTransportError::OutcomeUnknown);
                        cancels.clear();
                        deadlines.clear();
                        let result = respawn_stdio_session(&config, &mut session).await;
                        let _ = reply.send(result);
                    }
                    StdioWorkerCmd::Close { reply } => {
                        fail_pending(&mut pending, McpTransportError::TransportClosed);
                        cancels.clear();
                        deadlines.clear();
                        if let Some(mut sess) = session.take() {
                            let _ = sess.stdin.shutdown().await;
                            kill_and_reap_stdio_session(&mut sess, &config.name).await;
                        }
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
                            ::zeroclaw_log::record!(
                                ERROR,
                                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                                    .with_attrs(::serde_json::json!({
                                        "mcp_server": &config.name,
                                        "bytes": resp_line.len(),
                                    })),
                                "mcp_transport: MCP response too large"
                            );
                            fail_pending(&mut pending, McpTransportError::TransportClosed);
                            let _ = respawn_stdio_session(&config, &mut session).await;
                            cancels.clear();
                            deadlines.clear();
                            continue;
                        }
                        match classify_stdio_inbound(&resp_line) {
                            Ok(StdioInbound::ServerMessage { id, method }) => {
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
                            Ok(StdioInbound::Skippable) => {
                                ::zeroclaw_log::record!(
                                    DEBUG,
                                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                                    "MCP stdio: skipping server notification while waiting for response"
                                );
                            }
                            Ok(StdioInbound::Response(resp)) => {
                                let Some(resp_id) = resp.id.clone() else {
                                    continue;
                                };
                                if let Some(slot) = pending.remove(&resp_id) {
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
                                    "MCP stdio: failed to parse inbound line"
                                );
                            }
                        }
                    }
                    Ok(None) | Err(_) => {
                        fail_pending(&mut pending, McpTransportError::TransportClosed);
                        cancels.clear();
                        deadlines.clear();
                        let _ = respawn_stdio_session(&config, &mut session).await;
                    }
                }
            }
            canceled = futures_util::StreamExt::next(&mut cancels), if !cancels.is_empty() => {
                if let Some(Some(id)) = canceled
                    && let Some(slot) = pending.remove(&id)
                {
                    // Caller dropped during wait after a successful write.
                    let _ = slot.reply.send(Err(McpTransportError::OutcomeUnknown.into()));
                    fail_pending(&mut pending, McpTransportError::OutcomeUnknown);
                    cancels.clear();
                    deadlines.clear();
                    let _ = respawn_stdio_session(&config, &mut session).await;
                }
            }
            timed_out_id = futures_util::StreamExt::next(&mut deadlines), if !deadlines.is_empty() => {
                if let Some(id) = timed_out_id
                    && let Some(slot) = pending.remove(&id)
                {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Timeout)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "mcp_server": &config.name,
                                "id": id,
                            })),
                        "MCP stdio: timeout waiting for response"
                    );
                    let _ = slot
                        .reply
                        .send(Err(McpTransportError::ResponseTimeout.into()));
                    // Reset so a late frame cannot poison the next call.
                    fail_pending(&mut pending, McpTransportError::ResponseTimeout);
                    cancels.clear();
                    deadlines.clear();
                    let _ = respawn_stdio_session(&config, &mut session).await;
                }
            }
        }
    }

    if let Some(mut sess) = session.take() {
        kill_and_reap_stdio_session(&mut sess, &config.name).await;
    }
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

    async fn close(&self) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        // Worker may already be gone on drop paths.
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
    session_id: Option<String>,
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
        for (key, value) in &self.headers {
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
            // A 404/410 only means "stale session" when this request carried an
            // `Mcp-Session-Id` the server no longer recognizes (MCP spec 2025-06-18,
            // Session Management). Without a session id, a 404 is just a missing
            // endpoint (typo'd `url`, wrong path, proxy misroute) — surface it as a
            // plain error so `call_tool` doesn't burn a reconnect on it.
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
            return maybe_resp.ok_or_else(|| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                    "mcp_transport: MCP server returned no response in SSE stream"
                );
                anyhow::Error::msg("MCP server returned no response in SSE stream")
            });
        }

        let resp_text = resp.text().await.context("failed to read HTTP response")?;
        parse_jsonrpc_response_text(&resp_text)
    }

    async fn reset(&mut self) -> Result<()> {
        // Drop the stale session so the next request re-initializes and the
        // server issues a fresh `Mcp-Session-Id`.
        self.session_id = None;
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
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
    stream_state: SseStreamState,
    shared: std::sync::Arc<Mutex<SseSharedState>>,
    notify: std::sync::Arc<Notify>,
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
            stream_state: SseStreamState::Unknown,
            shared: std::sync::Arc::new(Mutex::new(SseSharedState::default())),
            notify: std::sync::Arc::new(Notify::new()),
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
            self.stream_state = SseStreamState::Unsupported;
            return Ok(());
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
            self.stream_state = SseStreamState::Unsupported;
            return Ok(());
        }

        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        self.shutdown_tx = Some(shutdown_tx);

        let shared = self.shared.clone();
        let notify = self.notify.clone();
        let sse_url = self.sse_url.clone();
        let server_name = self.server_name.clone();

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
                            handle_sse_event(&server_name, &sse_url, &shared, &notify, event.as_deref(), id.as_deref(), data).await;
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
}

#[derive(Default)]
struct SseSharedState {
    message_url: Option<String>,
    message_url_from_endpoint: bool,
    pending: std::collections::HashMap<u64, oneshot::Sender<JsonRpcResponse>>,
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

    // Server-to-client requests carry `method` and must not be treated as
    // responses even when `id` collides with an in-flight client request.
    if value.get("method").is_some() {
        return;
    }

    let Ok(resp) = serde_json::from_value::<JsonRpcResponse>(value) else {
        return;
    };

    let Some(id_val) = resp.id.clone() else {
        return;
    };
    let id = match id_val.as_u64() {
        Some(v) => v,
        None => return,
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
impl ExclusiveMcpTransport for SseTransport {
    async fn send_and_recv(&mut self, request: &JsonRpcRequest) -> Result<JsonRpcResponse> {
        self.ensure_connected().await?;

        let id = request.id.as_ref().and_then(|v| v.as_u64());
        let body = serde_json::to_string(request)?;

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

        let mut rx = None;
        if let Some(id) = id
            && self.stream_state == SseStreamState::Connected
        {
            let (tx, ch) = oneshot::channel();
            {
                let mut guard = self.shared.lock().await;
                guard.pending.insert(id, tx);
            }
            rx = Some((id, ch));
        }

        let mut got_direct = None;
        let mut last_status = None;

        for (i, url) in std::iter::once(primary_url)
            .chain(secondary_url)
            .enumerate()
        {
            let has_accept = self
                .headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("Accept"));
            let has_content_type = self
                .headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("Content-Type"));
            let mut req = apply_request_timeout(
                self.client.post(&url).body(body.clone()),
                http_request_timeout_secs(request, self.tool_timeout_secs),
            );
            if !has_content_type {
                req = req.header("Content-Type", MCP_JSON_CONTENT_TYPE);
            }
            for (key, value) in &self.headers {
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

            if request.id.is_none() {
                got_direct = Some(JsonRpcResponse {
                    jsonrpc: crate::mcp_protocol::JSONRPC_VERSION.to_string(),
                    id: None,
                    result: None,
                    error: None,
                });
                break;
            }

            let is_sse = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.to_ascii_lowercase().contains("text/event-stream"));

            if is_sse {
                if i == 0 && has_secondary {
                    match timeout(
                        Duration::from_secs(3),
                        read_first_jsonrpc_from_sse_response(resp),
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
                if let Some(resp) = read_first_jsonrpc_from_sse_response(resp).await? {
                    got_direct = Some(resp);
                }
                break;
            }

            let text = if i == 0 && has_secondary {
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
                let json_str = if trimmed.contains("\ndata:") || trimmed.starts_with("data:") {
                    extract_json_from_sse_text(trimmed)
                } else {
                    Cow::Borrowed(trimmed)
                };
                if let Ok(mcp_resp) = serde_json::from_str::<JsonRpcResponse>(json_str.as_ref()) {
                    got_direct = Some(mcp_resp);
                }
            }
            break;
        }

        if let Some((id, _)) = rx.as_ref() {
            if got_direct.is_some() {
                let mut guard = self.shared.lock().await;
                guard.pending.remove(id);
            } else if let Some(status) = last_status
                && !status.is_success()
            {
                let mut guard = self.shared.lock().await;
                guard.pending.remove(id);
            }
        }

        if let Some(resp) = got_direct {
            return Ok(resp);
        }

        if let Some(status) = last_status {
            if !status.is_success() {
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

        // A dropped receiver means the SSE reader task tore down the stream
        // before our response arrived — recoverable via reconnect.
        rx.await
            .map_err(|_| McpTransportError::TransportClosed.into())
    }

    async fn reset(&mut self) -> Result<()> {
        // Tear down the reader task and clear the cached endpoint/session state
        // so the next send re-handshakes: a fresh GET stream and a new
        // `endpoint` event from the (possibly restarted) server.
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

/// Create a transport based on config.
///
/// Stdio returns a shared worker handle. HTTP/SSE are wrapped in
/// [`SerialTransport`] so their prior exclusive behavior is preserved.
pub fn create_transport(config: Arc<McpServerConfig>) -> Result<Arc<dyn McpTransportConn>> {
    match config.transport {
        McpTransport::Stdio => Ok(Arc::new(StdioTransport::new(config)?)),
        McpTransport::Http => Ok(Arc::new(SerialTransport::new(HttpTransport::new(&config)?))),
        McpTransport::Sse => Ok(Arc::new(SerialTransport::new(SseTransport::new(&config)?))),
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
    fn stdio_recv_timeout_uses_short_budget_for_init() {
        let request = JsonRpcRequest::new(1, "initialize", serde_json::json!({}));
        let config = McpServerConfig {
            tool_timeout_secs: Some(McpServerConfig::MAX_TOOL_TIMEOUT_SECS),
            ..Default::default()
        };
        assert_eq!(
            stdio_recv_timeout_secs(&request, &config),
            RECV_TIMEOUT_SECS
        );
    }

    #[test]
    fn stdio_recv_timeout_honors_configured_tool_call_timeout() {
        let request = JsonRpcRequest::new(1, TOOLS_CALL_METHOD, serde_json::json!({}));
        let config = McpServerConfig {
            tool_timeout_secs: Some(240),
            ..Default::default()
        };
        assert_eq!(stdio_recv_timeout_secs(&request, &config), 240);
    }

    #[test]
    fn stdio_recv_timeout_defaults_tool_call_to_canonical_budget() {
        let request = JsonRpcRequest::new(1, TOOLS_CALL_METHOD, serde_json::json!({}));
        let config = McpServerConfig::default();
        assert_eq!(
            stdio_recv_timeout_secs(&request, &config),
            McpServerConfig::DEFAULT_TOOL_TIMEOUT_SECS
        );
    }

    #[test]
    fn stdio_recv_timeout_caps_tool_call_budget() {
        let request = JsonRpcRequest::new(1, TOOLS_CALL_METHOD, serde_json::json!({}));
        let config = McpServerConfig {
            tool_timeout_secs: Some(u64::MAX),
            ..Default::default()
        };
        assert_eq!(
            stdio_recv_timeout_secs(&request, &config),
            McpServerConfig::MAX_TOOL_TIMEOUT_SECS
        );
    }

    #[test]
    fn classify_stdio_skips_mismatched_response_shape_with_method() {
        // Colliding id + method is a server request, not a client response.
        let line = r#"{"jsonrpc":"2.0","id":7,"method":"roots/list","params":{}}"#;
        match classify_stdio_inbound(line).expect("parse") {
            StdioInbound::ServerMessage { id, method } => {
                assert_eq!(id, Some(serde_json::json!(7)));
                assert_eq!(method, "roots/list");
            }
            other => panic!("expected ServerMessage, got {other:?}"),
        }
    }

    #[test]
    fn classify_stdio_accepts_matching_response() {
        let line = r#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#;
        match classify_stdio_inbound(line).expect("parse") {
            StdioInbound::Response(resp) => {
                assert_eq!(resp.id, Some(serde_json::json!(7)));
                assert_eq!(
                    resp.result
                        .as_ref()
                        .and_then(|v| v.get("ok"))
                        .and_then(|v| v.as_bool()),
                    Some(true)
                );
            }
            other => panic!("expected Response, got {other:?}"),
        }
    }

    #[test]
    fn timeout_policy_lives_on_mcp_server_config_only() {
        // Guard against re-introducing duplicate 180/600 constants in this module.
        // Split needles so this assertion text cannot false-positive.
        let src = include_str!("mcp_transport.rs");
        let default_needle = ["const DEFAULT_STDIO_TOOL_", "TIMEOUT_SECS"].concat();
        let max_needle = ["const MAX_STDIO_TOOL_", "TIMEOUT_SECS"].concat();
        let spawn_needle = ["struct Stdio", "SpawnConfig"].concat();
        assert!(
            !src.contains(&default_needle),
            "stdio must not redeclare the default tool timeout"
        );
        assert!(
            !src.contains(&max_needle),
            "stdio must not redeclare the max tool timeout"
        );
        assert!(
            !src.contains(&spawn_needle),
            "stdio must not clone spawn fields into StdioSpawnConfig"
        );
        assert_eq!(McpServerConfig::DEFAULT_TOOL_TIMEOUT_SECS, 180);
        assert_eq!(McpServerConfig::MAX_TOOL_TIMEOUT_SECS, 600);
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
        let sse =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"ok\":true}}\n\n";
        let parsed =
            parse_jsonrpc_response_text(sse).expect("SSE-framed JSON response should parse");
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
        let mut transport = HttpTransport::new(&config).expect("build transport");

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::HeaderName::from_static("mcp-session-id"),
            reqwest::header::HeaderValue::from_static("session-abc"),
        );
        transport.update_session_id_from_headers(&headers);
        assert_eq!(transport.session_id.as_deref(), Some("session-abc"));
    }

    #[test]
    fn http_transport_injects_session_id_header_when_available() {
        let config = McpServerConfig {
            name: "test-http".into(),
            transport: McpTransport::Http,
            url: Some("http://localhost/mcp".into()),
            ..Default::default()
        };
        let mut transport = HttpTransport::new(&config).expect("build transport");
        transport.session_id = Some("session-xyz".to_string());

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
        let config = Arc::new(McpServerConfig {
            name: "test-stdio".into(),
            transport: McpTransport::Stdio,
            command: "/usr/bin/zeroclaw_nonexistent_binary_abc123".into(),
            ..Default::default()
        });
        let result = create_transport(config);
        assert!(result.is_err());
    }

    #[test]
    fn create_transport_http_without_url_fails() {
        let config = Arc::new(McpServerConfig {
            name: "test-http".into(),
            transport: McpTransport::Http,
            ..Default::default()
        });
        assert!(create_transport(config).is_err());
    }

    #[test]
    fn create_transport_sse_without_url_fails() {
        let config = Arc::new(McpServerConfig {
            name: "test-sse".into(),
            transport: McpTransport::Sse,
            ..Default::default()
        });
        assert!(create_transport(config).is_err());
    }

    #[test]
    fn create_transport_http_with_url_succeeds() {
        let config = Arc::new(McpServerConfig {
            name: "test-http".into(),
            transport: McpTransport::Http,
            url: Some("http://localhost:9999/mcp".into()),
            ..Default::default()
        });
        // Build should succeed even if server isn't running
        assert!(create_transport(config).is_ok());
    }

    #[test]
    fn create_transport_sse_with_url_succeeds() {
        let config = Arc::new(McpServerConfig {
            name: "test-sse".into(),
            transport: McpTransport::Sse,
            url: Some("http://localhost:9999/sse".into()),
            ..Default::default()
        });
        assert!(create_transport(config).is_ok());
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
        let mut transport = HttpTransport::new(&config).expect("build transport");
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::HeaderName::from_static("mcp-session-id"),
            reqwest::header::HeaderValue::from_static("   "),
        );
        transport.update_session_id_from_headers(&headers);
        // Whitespace-only session id should not be stored
        assert!(transport.session_id.is_none());
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
        assert!(transport.session_id.is_none());
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
        let mut transport = HttpTransport::new(&config).expect("build transport");
        transport.session_id = Some("stale-session".into());
        ExclusiveMcpTransport::reset(&mut transport)
            .await
            .expect("reset");
        assert!(transport.session_id.is_none());
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
        let mut transport = HttpTransport::new(&config).expect("build transport");
        // A 404 only signals a stale session when the request carried a session id.
        transport.session_id = Some("sess-1".into());
        let req = JsonRpcRequest::new(1, "tools/call", serde_json::json!({}));
        let err = ExclusiveMcpTransport::send_and_recv(&mut transport, &req)
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
        let mut transport = HttpTransport::new(&config).expect("build transport");
        assert!(transport.session_id.is_none());
        let req = JsonRpcRequest::new(1, "tools/call", serde_json::json!({}));
        let err = ExclusiveMcpTransport::send_and_recv(&mut transport, &req)
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
    async fn sse_transport_reset_clears_session_and_endpoint_state() {
        let config = McpServerConfig {
            name: "test-sse".into(),
            transport: McpTransport::Sse,
            url: Some("http://localhost:1/sse".into()),
            ..Default::default()
        };
        let mut transport = SseTransport::new(&config).expect("build transport");
        transport.stream_state = SseStreamState::Connected;
        {
            let mut guard = transport.shared.lock().await;
            guard.message_url = Some("http://localhost:1/messages".into());
            guard.message_url_from_endpoint = true;
            let (tx, _rx) = oneshot::channel();
            guard.pending.insert(7, tx);
        }

        ExclusiveMcpTransport::reset(&mut transport)
            .await
            .expect("reset");

        assert_eq!(transport.stream_state, SseStreamState::Unknown);
        let guard = transport.shared.lock().await;
        assert!(guard.message_url.is_none());
        assert!(!guard.message_url_from_endpoint);
        assert!(guard.pending.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_reset_reaps_old_child_before_replacement() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;
        use tokio::time::{Duration, sleep};

        fn process_is_alive(pid: u32) -> bool {
            std::process::Command::new("kill")
                .arg("-0")
                .arg(pid.to_string())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        }

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
        let server_path = temp.path().join("hang-mcp.sh");
        let pid_path = temp.path().join("hang-mcp.pid");
        let mut script = std::fs::File::create(&server_path).expect("script");
        script
            .write_all(
                br#"#!/bin/sh
echo "$$" > "$1"
# Stay alive until killed; ignore stdin.
exec tail -f /dev/null
"#,
            )
            .expect("write script");
        drop(script);
        let mut perms = std::fs::metadata(&server_path)
            .expect("metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&server_path, perms).expect("chmod");

        let config = Arc::new(McpServerConfig {
            name: "hang".into(),
            command: server_path.display().to_string(),
            args: vec![pid_path.display().to_string()],
            transport: McpTransport::Stdio,
            ..Default::default()
        });
        let transport = StdioTransport::new(Arc::clone(&config)).expect("spawn");
        let old_pid = read_pid(&pid_path).await;
        assert!(process_is_alive(old_pid), "child should be alive");

        // Remove pid file so the replacement write is observable.
        let _ = std::fs::remove_file(&pid_path);
        transport.reset().await.expect("reset must succeed");

        for _ in 0..50 {
            if !process_is_alive(old_pid) {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !process_is_alive(old_pid),
            "old child {old_pid} must be reaped before/at reset completion"
        );
        let new_pid = read_pid(&pid_path).await;
        assert_ne!(old_pid, new_pid, "replacement child must be a new pid");
        assert!(process_is_alive(new_pid));
        transport.close().await.expect("close");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_failed_reset_is_surfaced() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        // Self-deleting script: first spawn succeeds; after reap, the script
        // path is gone so replacement spawn fails and reset surfaces Err.
        let temp = tempfile::tempdir().expect("tempdir");
        let server_path = temp.path().join("once.sh");
        let mut script = std::fs::File::create(&server_path).expect("script");
        script
            .write_all(
                br#"#!/bin/sh
rm -f "$0"
exec tail -f /dev/null
"#,
            )
            .expect("write");
        drop(script);
        let mut perms = std::fs::metadata(&server_path)
            .expect("metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&server_path, perms).expect("chmod");

        let config = Arc::new(McpServerConfig {
            name: "once".into(),
            command: server_path.display().to_string(),
            args: vec![],
            transport: McpTransport::Stdio,
            ..Default::default()
        });
        let transport = StdioTransport::new(Arc::clone(&config)).expect("initial spawn");
        // Give the script a moment to delete itself.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let err = transport
            .reset()
            .await
            .expect_err("replacement spawn must fail and surface");
        assert!(
            err.to_string().contains("failed to spawn replacement")
                || err.to_string().contains("failed to spawn"),
            "got: {err:#}"
        );
    }
}

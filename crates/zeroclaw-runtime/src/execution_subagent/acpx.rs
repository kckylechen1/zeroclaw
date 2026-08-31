//! The production ACPX carrier — a real [`SessionController`] over the
//! Agent Client Protocol (ACP) JSON-RPC wire to an agent-side adapter
//! process (Codex first: the `codex-acp` adapter shipped with the Codex
//! CLI). This is the transport the scripted fixture stood in for (#261's
//! honest gap); every lifecycle operation here is a real process/protocol
//! operation observed by this controller.
//!
//! ```text
//! ExecutionSubagentTool → GatedSessionController → AcpxController (THIS FILE)
//!   stdio JSON-RPC 2.0 (ACP): initialize → session/new → session/prompt
//!   notifications: session/update (bounded progress projection)
//!   harness requests answered: session/request_permission → denied
//! ```
//!
//! Authority boundaries encoded here:
//!
//! - **Fixed transport binding, below the tool surface.** The executable,
//!   argv extras, operator env, workspace root, and session mode come from
//!   the host-constructed [`AcpxControllerConfig`]. No port request can
//!   widen them: [`SessionStartSpec`] carries only the bounded prompt.
//! - **The transport mints the remote session identity.** `session/new`
//!   returns the harness's `sessionId`; it is OBSERVED into the
//!   [`SessionHandle`], never chosen by the caller.
//! - **The subagent holds no approval authority.** The ACP client
//!   capability surface is declared with filesystem reads/writes DISABLED;
//!   `session/request_permission` requests from the harness are answered
//!   `cancelled` (deny) and surfaced as a progress fact. This client never
//!   reads or writes files for the harness.
//! - **Bounded payloads.** Child stdout lines, event summaries, and the
//!   collect projection are capped (fact summaries at the spine's 2000-char
//!   ceiling); oversized lines are truncated, never buffered unbounded.
//! - **Credentials stay out.** The operator env (e.g. the harness
//!   credential home) is passed to the child verbatim, never logged, never
//!   echoed into errors (the config's `Debug` impl redacts it), and never
//!   part of any fact or report surface. Workspace paths are scrubbed from
//!   every fact summary.
//! - **Fail closed, real reconnect.** A dead transport surfaces
//!   [`ControllerError::Unavailable`]; `reattach` — and the watch-retry
//!   path the tool runs after a reconnect receipt — brings the SAME
//!   harness session back through ACP `session/load`. The session id is
//!   preserved; no new harness session is minted.
//! - **The run's turn contract (EPHEMERAL_EXEC).** An ACP session only
//!   acts when prompted: the objective turn's end surfaces as
//!   `InputRequired` (the session is genuinely awaiting input); a
//!   correction leg delivered through the port's `prompt` that the
//!   harness answers closes the host-owned run lifecycle (`completed`).
//!   The harness ending a turn is NOT semantic acceptance — the parent
//!   still adjudicates the collected report.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;
use zeroclaw_api::session_exec::{
    AuthorityConfirmationRef, RemoteSessionRef, SessionEventIdRef, SessionEventKindV1,
    SessionTerminalOutcomeV1,
};

use super::controller::{
    ControllerError, ControllerEvent, PromptReceipt, SessionCapabilities, SessionCollectView,
    SessionController, SessionEventPage, SessionHandle, SessionStartSpec, SessionStopReceipt,
};

/// The fact-summary ceiling (the spine's bounded-summary law).
const SUMMARY_CEILING: usize = 2000;
/// Grace period between `session/cancel` and the child kill.
const CANCEL_GRACE: Duration = Duration::from_secs(10);
/// One watch long-poll window.
const WATCH_POLL_WINDOW: Duration = Duration::from_secs(5);
/// Consecutive dead-transport resume attempts before `watch` fails
/// closed (a flapping adapter must not trap the run).
const MAX_WATCH_RESUMES: u64 = 3;
/// How long a recovery waiter waits for an in-flight recovery to settle
/// before failing typed.
const RECOVERY_WAIT: Duration = Duration::from_secs(60);
const ALIVE_YES: u8 = 1;
/// How long a terminal-reached session stays retrievable: the owning
/// run's collect happens immediately after the loop; reattaching to a
/// terminal session is not meaningful for the ephemeral vertical.
const TERMINAL_RETENTION: Duration = Duration::from_secs(300);
const UNSET: u8 = 0;
const SET: u8 = 1;

// ─────────────────────────────────────────────────────────────────────────
// Configuration (host-constructed; never a port parameter)
// ─────────────────────────────────────────────────────────────────────────

/// Host-constructed binding for the ACPX transport. Constructed once by
/// the embedder from the operator's own configuration; the port surface
/// cannot widen any field.
#[derive(Clone)]
pub struct AcpxControllerConfig {
    /// Absolute path of the agent-side ACP adapter executable. Verified
    /// at construction — never read from a port request.
    pub command: PathBuf,
    /// Fixed extra argv (e.g. protocol flags). Never model-supplied.
    pub args: Vec<String>,
    /// Operator-managed child env (e.g. the harness credential home).
    /// Values are secrets: redacted from `Debug`, never logged, never
    /// part of any error or fact surface.
    pub env: HashMap<String, String>,
    /// The host's own workspace root — the `cwd` every session is bound
    /// to. Fixed here; the port cannot retarget it.
    pub workspace_root: PathBuf,
    /// Optional harness approval-preset id applied via `session/set_mode`
    /// after `session/new` (e.g. the harness's workspace-write preset).
    /// `None` keeps the harness's default.
    pub session_mode: Option<String>,
    /// Upper bound for the initialize + session/new handshake.
    pub startup_timeout: Duration,
    /// Upper bound for one prompt turn.
    pub turn_timeout: Duration,
    /// Upper bound for one child stdout line (bounded buffering).
    pub max_line_bytes: usize,
    /// Declared capability set for sessions this transport starts (must
    /// be covered by what this adapter actually implements).
    pub declared_capabilities: Vec<&'static str>,
}

impl std::fmt::Debug for AcpxControllerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redact the env map wholesale: values are operator secrets.
        f.debug_struct("AcpxControllerConfig")
            .field("command", &self.command)
            .field("args", &self.args)
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .field("workspace_root", &self.workspace_root)
            .field("session_mode", &self.session_mode)
            .field("startup_timeout", &self.startup_timeout)
            .field("turn_timeout", &self.turn_timeout)
            .field("max_line_bytes", &self.max_line_bytes)
            .field("declared_capabilities", &self.declared_capabilities)
            .finish()
    }
}

impl AcpxControllerConfig {
    /// The capability set this adapter actually implements. A start spec
    /// requesting anything outside this set is refused (no fake support).
    #[must_use]
    pub fn supported_capabilities() -> SessionCapabilities {
        // observe/wait (event log + turn waits), prompt, cancel, resume,
        // events. No `load`/`artifacts`: collect is read-only and bounded.
        SessionCapabilities {
            observe: true,
            wait: true,
            prompt: true,
            cancel: true,
            resume: true,
            load: false,
            events: true,
            artifacts: false,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Session state (the host-owned bounded transcript projection)
// ─────────────────────────────────────────────────────────────────────────

struct AcpxProcess {
    /// The direct child handle. Ownership lives HERE (not in a local) so
    /// the harness process is alive exactly as long as the session is,
    /// and is killed when the process slot is terminated — no orphaned
    /// harness survives a transport drop or a stop.
    child: Mutex<Option<tokio::process::Child>>,
    stdin_tx: mpsc::Sender<String>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next_id: AtomicU64,
}

struct SessionState {
    /// The harness-minted remote identity (set by `start` once the
    /// transport returns it).
    session_id: Mutex<String>,
    /// Long-poll wakeup for `watch` (bounded so the deadline law in the
    /// tool loop still advances).
    event_signal: Arc<tokio::sync::Notify>,
    process: Mutex<Option<Arc<AcpxProcess>>>,
    events: Mutex<Vec<ControllerEvent>>,
    next_seq: AtomicU64,
    /// Monotone ordinal of harness facts observed by this state. It makes
    /// every event id distinct (two real tool calls are two facts) while
    /// staying stable across a transport drop (the state, and therefore
    /// the ordinal space, survives the drop), so the spine's replay dedup
    /// stays exact. The salt is per state instance: a controller-restart
    /// resume opens a fresh ordinal namespace, so facts observed by the
    /// new instance can never collide with (and be silently dropped
    /// against) the pre-restart stream — they are distinct ids at the
    /// spine, and the rank/revision guard owns ordering.
    ordinal_salt: u64,
    /// Monotone counter over this state's observed facts.
    update_ordinal: AtomicU64,
    /// Accumulating text of the turn in flight.
    last_turn_text: Mutex<String>,
    /// Final text of the last COMPLETED turn (the collect source).
    completed_turn_text: Mutex<String>,
    /// Armed stop: the next observed `cancelled` stop reason binds this
    /// confirmation reference to the terminal fact.
    stop_armed: Mutex<Option<AuthorityConfirmationRef>>,
    /// The objective turn's stop reason was observed.
    objective_done: AtomicU8,
    /// Correction legs delivered through the port's `prompt`.
    correction_legs: AtomicU64,
    /// A `session/load` resume is in flight: history replay is observed
    /// but not appended (already-observed facts).
    load_in_flight: AtomicU8,
    alive: AtomicU8,
    /// The open sequence has settled (success or typed failure). A
    /// cancelled `start` future never sets this; the open watchdog reads
    /// it to decide whether a stranded child must be killed.
    settled: AtomicU8,
    /// A terminal fact was observed: the run is over, the state becomes
    /// evictable after a retention window long enough for the owning run
    /// to finish collect (the spine owns canonical truth from here).
    terminal_reached: AtomicU8,
    /// When this state becomes evictable (`None` = window not started).
    evict_after: Mutex<Option<std::time::Instant>>,
    /// Consecutive resume attempts without intervening events (the
    /// flapping-transport bound for `watch`).
    resume_streak: AtomicU64,
    /// Serializes resume attempts on this state: two concurrent
    /// recoveries can no longer interleave spawn/publish/teardown.
    resume_lock: tokio::sync::Mutex<()>,
    /// Bounded, secret-free fact projection: occurrences of these strings
    /// (workspace path, operator env values) are replaced before any
    /// summary leaves the transport.
    scrub: std::sync::Arc<Vec<String>>,
}

impl SessionState {
    fn session_id(&self) -> String {
        self.session_id.lock().clone()
    }

    fn push_event(
        &self,
        kind: SessionEventKindV1,
        outcome: Option<SessionTerminalOutcomeV1>,
        summary: Option<String>,
    ) {
        let summary = summary.map(|text| bounded_text(&text, SUMMARY_CEILING, &self.scrub));
        // The ordinal is part of the identity: two distinct harness facts
        // with the same projected summary are STILL two facts, and the
        // ordinal space survives a transport drop (same state), so the
        // spine's replay dedup sees identical ids exactly once. History
        // replayed by `session/load` never reaches here (the load gate).
        if matches!(kind, SessionEventKindV1::Terminal) {
            self.terminal_reached.store(SET, Ordering::SeqCst);
            *self.evict_after.lock() = Some(std::time::Instant::now() + TERMINAL_RETENTION);
        }
        let ordinal = self.update_ordinal.fetch_add(1, Ordering::SeqCst);
        let event_id = event_identity(
            kind.as_str(),
            summary.as_deref(),
            (self.ordinal_salt, ordinal),
        );
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.events.lock().push(ControllerEvent {
            seq,
            event_id,
            kind,
            outcome,
            summary,
        });
        self.event_signal.notify_waiters();
    }
}

/// Event identity: ordinal-scoped (distinct real facts never collide)
/// and stable across a transport drop (the ordinal space lives in the
/// session state, which outlives the child).
fn event_identity(
    kind: &str,
    summary: Option<&str>,
    (salt, ordinal): (u64, u64),
) -> SessionEventIdRef {
    let mut hasher = Sha256::new();
    hasher.update(kind.as_bytes());
    hasher.update([0x1f]);
    hasher.update(summary.unwrap_or_default().as_bytes());
    hasher.update([0x1f]);
    hasher.update(salt.to_le_bytes());
    hasher.update(ordinal.to_le_bytes());
    let digest = hasher.finalize();
    SessionEventIdRef::from_opaque(format!(
        "acpx-{:016x}",
        u64::from_be_bytes(digest[..8].try_into().unwrap_or([0; 8]))
    ))
}

/// Bound text at a char boundary and scrub the workspace path (receipt
/// surfaces carry no repository path).
fn bounded_text(text: &str, ceiling: usize, scrub: &[String]) -> String {
    const SUFFIX: &str = "…[truncated]";
    let mut scrubbed = text.to_string();
    for needle in scrub {
        if !needle.is_empty() {
            scrubbed = scrubbed.replace(needle.as_str(), "<workspace>");
        }
    }
    if scrubbed.len() <= ceiling {
        return scrubbed;
    }
    // The suffix counts against the ceiling: the bound is the bound.
    let hard = ceiling.saturating_sub(SUFFIX.len());
    let mut boundary = hard.min(scrubbed.len());
    while boundary > 0 && !scrubbed.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}{SUFFIX}", &scrubbed[..boundary])
}

// ─────────────────────────────────────────────────────────────────────────
// The controller
// ─────────────────────────────────────────────────────────────────────────

impl AcpxProcess {
    /// Synchronously kill THIS process's own child (group-signalled).
    /// Idempotent and self-scoped: whatever else occupies a state slot is
    /// never touched, so superseded recovery attempts cannot kill a
    /// newer attempt's child.
    fn kill_now(&self) {
        self.pending.lock().clear();
        let child = self.child.lock().take();
        if let Some(child) = child {
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                let pgid = pid as libc::pid_t;
                unsafe {
                    libc::killpg(pgid, libc::SIGKILL);
                }
            }
            #[cfg(not(unix))]
            {
                let mut child = child;
                let _ = child.start_kill();
            }
        }
    }
}

/// The real ACPX [`SessionController`]. Deliberately NOT `Clone`: one
/// controller owns its sessions' lifecycles, and its `Drop` is the
/// last-resort kill. Share it through an `Arc` at the wiring layer.
pub struct AcpxController {
    config: Arc<AcpxControllerConfig>,
    sessions: Arc<Mutex<HashMap<String, Arc<SessionState>>>>,
    /// Redaction needles: the workspace root plus every operator env
    /// value (the values are secrets; the list lives behind Arc).
    scrub: std::sync::Arc<Vec<String>>,
}

impl AcpxController {
    /// Construct the transport. Fails closed when the configured
    /// executable or workspace root does not exist, or when the declared
    /// capability set exceeds what this adapter implements — never
    /// lazily at `start`.
    pub fn new(config: AcpxControllerConfig) -> Result<Self, ControllerError> {
        if !config.command.is_absolute() || !config.command.is_file() {
            return Err(ControllerError::Refused(format!(
                "acpx adapter command is not an absolute existing file: {}",
                config.command.display()
            )));
        }
        if !config.workspace_root.is_absolute() || !config.workspace_root.is_dir() {
            return Err(ControllerError::Refused(
                "acpx workspace root is not an absolute existing directory (fail closed)"
                    .to_string(),
            ));
        }
        let declared = SessionCapabilities::from_names(
            &config
                .declared_capabilities
                .iter()
                .map(|name| (*name).to_string())
                .collect::<Vec<String>>(),
        )?;
        let supported = AcpxControllerConfig::supported_capabilities();
        for (name, requested, supported) in [
            ("observe", declared.observe, supported.observe),
            ("wait", declared.wait, supported.wait),
            ("prompt", declared.prompt, supported.prompt),
            ("cancel", declared.cancel, supported.cancel),
            ("resume", declared.resume, supported.resume),
            ("load", declared.load, supported.load),
            ("events", declared.events, supported.events),
            ("artifacts", declared.artifacts, supported.artifacts),
        ] {
            if requested && !supported {
                return Err(ControllerError::Refused(format!(
                    "declared capability {name:?} exceeds what the acpx adapter implements"
                )));
            }
        }
        let mut scrub = vec![config.workspace_root.display().to_string()];
        scrub.extend(config.env.values().cloned());
        Ok(Self {
            config: Arc::new(config),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            scrub: std::sync::Arc::new(scrub),
        })
    }

    fn state(&self, handle: &SessionHandle) -> Result<Arc<SessionState>, ControllerError> {
        let id = handle.remote_session.as_str().to_string();
        self.sessions
            .lock()
            .get(&id)
            .cloned()
            .ok_or(ControllerError::Unavailable)
    }

    fn process(state: &SessionState) -> Result<Arc<AcpxProcess>, ControllerError> {
        let process = state.process.lock().clone();
        process
            .filter(|_| state.alive.load(Ordering::SeqCst) == ALIVE_YES)
            .ok_or(ControllerError::Unavailable)
    }

    /// Send a JSON-RPC notification (no id; no response expected).
    async fn notify(
        process: &AcpxProcess,
        method: &str,
        params: Value,
    ) -> Result<(), ControllerError> {
        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
        process
            .stdin_tx
            .send(message.to_string())
            .await
            .map_err(|_| ControllerError::Unavailable)
    }

    /// Send a JSON-RPC request and await its response (bounded).
    async fn request(
        process: &AcpxProcess,
        method: &str,
        params: Value,
        timeout: Duration,
        scrub: &[String],
    ) -> Result<Value, ControllerError> {
        let id = process.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        process.pending.lock().insert(id, tx);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if process.stdin_tx.send(message.to_string()).await.is_err() {
            process.pending.lock().remove(&id);
            return Err(ControllerError::Unavailable);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => {
                if let Some(error) = response.get("error") {
                    let detail = bounded_text(
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("acpx request failed"),
                        SUMMARY_CEILING,
                        scrub,
                    );
                    return Err(ControllerError::Refused(format!(
                        "{method} refused by harness: {detail}"
                    )));
                }
                Ok(response.get("result").cloned().unwrap_or(Value::Null))
            }
            Ok(Err(_)) | Err(_) => {
                process.pending.lock().remove(&id);
                Err(ControllerError::Unavailable)
            }
        }
    }

    /// Spawn the adapter child, run the `initialize` handshake, and start
    /// the reader/writer tasks wired to `state`.
    async fn spawn_and_initialize(
        &self,
        state: &Arc<SessionState>,
    ) -> Result<Arc<AcpxProcess>, ControllerError> {
        // Direct argv exec — no shell anywhere on this path. The child is
        // killed when its handle drops (no orphaned harness sessions).
        #[cfg(unix)]
        let mut command = {
            let mut command = tokio::process::Command::new(&self.config.command);
            // Own process group: the terminate path kills the whole tree
            // (adapter wrappers often exec platform binaries that would
            // otherwise survive as orphans).
            command.process_group(0);
            command
        };
        #[cfg(not(unix))]
        let mut command = tokio::process::Command::new(&self.config.command);
        command
            .args(&self.config.args)
            .current_dir(&self.config.workspace_root)
            // Minimal env allowlist (the adapter child needs a resolver
            // and a home to start) plus the operator-configured map.
            // Nothing else from this process leaks into the harness.
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .env("TMPDIR", std::env::var("TMPDIR").unwrap_or_default())
            .envs(&self.config.env)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // stderr is drained to a bounded sink: never inherited (it
            // could interleave with our logs) and never stored (it may
            // carry machine-local detail). Nothing from it is surfaced.
            .stderr(std::process::Stdio::piped());
        let mut child = command.spawn().map_err(|_| ControllerError::Unavailable)?;
        let stdin = child.stdin.take().ok_or(ControllerError::Unavailable)?;
        let stdout = child.stdout.take().ok_or(ControllerError::Unavailable)?;
        let stderr = child.stderr.take().ok_or(ControllerError::Unavailable)?;

        let (stdin_tx, stdin_rx) = mpsc::channel::<String>(64);
        zeroclaw_spawn::spawn!(writer_task(stdin, stdin_rx));

        // Bounded stderr drain: consumes, keeps nothing, logs nothing.
        zeroclaw_spawn::spawn!(drain_task(stderr));

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let process = Arc::new(AcpxProcess {
            child: Mutex::new(Some(child)),
            stdin_tx,
            pending: pending.clone(),
            next_id: AtomicU64::new(1),
        });

        // The child is owned by the state from this moment: every
        // failure path below (and the tool's own fail-closed arms) kills
        // it through the state slot.
        state.alive.store(ALIVE_YES, Ordering::SeqCst);
        *state.process.lock() = Some(process.clone());

        // Reader: owns stdout until EOF.
        let reader_process = process.clone();
        let reader_state = state.clone();
        let line_ceiling = self.config.max_line_bytes;
        zeroclaw_spawn::spawn!(async move {
            reader_task(stdout, reader_process, reader_state, pending, line_ceiling).await;
        });

        // Handshake: the client declares NO filesystem authority.
        let result = tokio::time::timeout(
            self.config.startup_timeout,
            Self::request(
                &process,
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {
                        "fs": {"readTextFile": false, "writeTextFile": false},
                    },
                }),
                self.config.startup_timeout,
                &self.scrub,
            ),
        )
        .await
        .map_err(|_| ControllerError::Unavailable)??;
        if result.get("protocolVersion").and_then(Value::as_i64) != Some(1) {
            return Err(ControllerError::Refused(
                "acpx adapter negotiated an unexpected protocol version".to_string(),
            ));
        }
        Ok(process)
    }

    /// Create the harness session and run the objective turn. The remote
    /// identity is MINTED by the transport here and observed into `state`.
    async fn open_session(
        &self,
        state: &Arc<SessionState>,
        prompt: &str,
    ) -> Result<(), ControllerError> {
        let process = self.spawn_and_initialize(state).await?;
        let result = tokio::time::timeout(
            self.config.startup_timeout,
            Self::request(
                &process,
                "session/new",
                json!({"cwd": self.config.workspace_root, "mcpServers": []}),
                self.config.startup_timeout,
                &self.scrub,
            ),
        )
        .await
        .map_err(|_| ControllerError::Unavailable)??;
        let Some(minted) = result
            .get("sessionId")
            .or_else(|| result.get("session_id"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
        else {
            return Err(ControllerError::Refused(
                "acpx adapter returned no session identity".to_string(),
            ));
        };
        *state.session_id.lock() = minted;
        if let Some(mode) = self.config.session_mode.as_deref() {
            let session_id = state.session_id();
            let _ = tokio::time::timeout(
                self.config.startup_timeout,
                Self::request(
                    &process,
                    "session/set_mode",
                    json!({"sessionId": session_id, "modeId": mode}),
                    self.config.startup_timeout,
                    &self.scrub,
                ),
            )
            .await
            .map_err(|_| ControllerError::Unavailable)??;
        }
        // The objective prompt opens the session's first turn.
        Self::start_turn(
            &process,
            state,
            prompt,
            self.config.turn_timeout,
            &self.scrub,
        )
        .await
    }

    /// Send `session/prompt` for one turn and wait (bounded) for its
    /// stop reason. The reader owns the response and performs the
    /// turn-contract event mapping.
    async fn start_turn(
        process: &AcpxProcess,
        state: &Arc<SessionState>,
        text: &str,
        timeout: Duration,
        scrub: &[String],
    ) -> Result<(), ControllerError> {
        let id = process.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        process.pending.lock().insert(id, tx);
        let session_id = state.session_id();
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": text}],
            },
        });
        if process.stdin_tx.send(message.to_string()).await.is_err() {
            process.pending.lock().remove(&id);
            return Err(ControllerError::Unavailable);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => {
                if response.get("error").is_some() {
                    let detail = bounded_text(
                        response
                            .pointer("/error/message")
                            .and_then(Value::as_str)
                            .unwrap_or("session/prompt failed"),
                        SUMMARY_CEILING,
                        scrub,
                    );
                    return Err(ControllerError::Refused(format!(
                        "session/prompt refused by harness: {detail}"
                    )));
                }
                Ok(())
            }
            Ok(Err(_)) | Err(_) => {
                process.pending.lock().remove(&id);
                Err(ControllerError::Unavailable)
            }
        }
    }

    /// Bring a dead transport back to the SAME harness session through
    /// ACP `session/load` (no new session is minted).
    /// Recovery is serialized per state and CANCELLATION-SAFE: the
    /// in-flight marker lives in a guard whose `Drop` clears it, so even
    /// a cancelled caller future can never leave the marker stuck (which
    /// would suppress all later updates) nor fool a coalescing waiter
    /// into treating a half-open transport as recovered. The flapping
    /// budget is consumed only by attempts this waiter actually makes.
    async fn resume_session(&self, state: &Arc<SessionState>) -> Result<(), ControllerError> {
        // The recovery is owned by ONE guard: the per-state lock (serializing
        // attempts), the in-flight marker, and the child kill on every
        // non-success exit — including future CANCELLATION. There is no
        // cleanup path that can be skipped by a cancelled caller. The lock
        // wait is bounded: an in-flight recovery that never settles fails
        // the attempt typed instead of hanging the run.
        // Held to the end of the attempt: the recovery is serialized.
        let _resume_guard = tokio::time::timeout(RECOVERY_WAIT, state.resume_lock.lock())
            .await
            .map_err(|_| ControllerError::Unavailable)?;
        state.load_in_flight.store(SET, Ordering::SeqCst);
        // Coalesce: a COMPLETED preceding recovery that left a live
        // transport makes this attempt a no-op. A cancelled predecessor
        // cleared both the marker (guard Drop) and liveness (kill_now),
        // so this check can only pass for a genuinely settled transport.
        if state.alive.load(Ordering::SeqCst) == ALIVE_YES {
            return Ok(());
        }
        let mut recovery = RecoveryGuard::new(state);
        recovery.spawn_and_initialize(self).await?;
        let session_id = state.session_id();
        let load = tokio::time::timeout(
            self.config.startup_timeout,
            recovery.request(
                "session/load",
                json!({
                    "sessionId": session_id,
                    "cwd": self.config.workspace_root,
                    "mcpServers": [],
                }),
            ),
        )
        .await;
        match load {
            Ok(Ok(_)) => {
                recovery.done = true;
                Ok(())
            }
            Ok(Err(_)) | Err(_) => Err(ControllerError::Unavailable),
        }
    }

    /// Kill the transport child and mark the state dead. Idempotent. The
    /// kill is real (the child handle is owned here) and the child is
    /// reaped, so no harness process outlives the session state.
    async fn terminate(state: &SessionState) {
        state.alive.store(UNSET, Ordering::SeqCst);
        // Take the slot OUT of the lock before awaiting: no guard may be
        // held across the kill.
        let process = state.process.lock().take();
        if let Some(process) = process {
            process.pending.lock().clear();
            let child = process.child.lock().take();
            if let Some(mut child) = child {
                #[cfg(unix)]
                if let Some(pid) = child.id() {
                    let pgid = pid as libc::pid_t;
                    unsafe {
                        libc::killpg(pgid, libc::SIGKILL);
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = child.start_kill();
                }
                let _ = child.wait().await;
            }
        }
    }
}

/// The single owner of one recovery attempt: holds the per-state resume
/// lock, marks the in-flight marker, and — unless the recovery completed
/// successfully — kills the attempt's own child on EVERY exit path,
/// including future CANCELLATION. There is no cleanup that a cancelled
/// caller future can skip.
struct RecoveryGuard<'a> {
    state: &'a Arc<SessionState>,
    process: Option<Arc<AcpxProcess>>,
    done: bool,
}

impl<'a> RecoveryGuard<'a> {
    fn new(state: &'a Arc<SessionState>) -> Self {
        Self {
            state,
            process: None,
            done: false,
        }
    }

    async fn spawn_and_initialize(
        &mut self,
        controller: &AcpxController,
    ) -> Result<(), ControllerError> {
        self.state.load_in_flight.store(SET, Ordering::SeqCst);
        self.process = Some(controller.spawn_and_initialize(self.state).await?);
        Ok(())
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, ControllerError> {
        let process = self.process.as_ref().ok_or(ControllerError::Unavailable)?;
        AcpxController::request(
            process,
            method,
            params,
            Duration::from_secs(30),
            &self.state.scrub,
        )
        .await
    }
}

impl Drop for RecoveryGuard<'_> {
    fn drop(&mut self) {
        self.state.load_in_flight.store(UNSET, Ordering::SeqCst);
        if !self.done {
            // The recovery did not complete: kill THIS attempt's own child
            // (self-scoped), retire the slot only if it still holds this
            // process, and mark the state dead.
            if let Some(process) = &self.process {
                process.kill_now();
            }
            let mut slot = self.state.process.lock();
            let owned = slot.as_ref().is_some_and(|current| {
                self.process
                    .as_ref()
                    .is_some_and(|p| Arc::ptr_eq(current, p))
            });
            if owned {
                self.state.alive.store(UNSET, Ordering::SeqCst);
                *slot = None;
            }
        }
    }
}

#[async_trait]
impl SessionController for AcpxController {
    async fn start(&self, spec: &SessionStartSpec) -> Result<SessionHandle, ControllerError> {
        if spec.prompt.is_empty() {
            return Err(ControllerError::Refused("empty prompt".to_string()));
        }
        let state = Arc::new(SessionState {
            session_id: Mutex::new(String::new()),
            event_signal: Arc::new(tokio::sync::Notify::new()),
            process: Mutex::new(None),
            events: Mutex::new(Vec::new()),
            next_seq: AtomicU64::new(0),
            update_ordinal: AtomicU64::new(0),
            ordinal_salt: u64::from_le_bytes(
                Uuid::new_v4().as_bytes()[..8].try_into().unwrap_or([0; 8]),
            ),
            last_turn_text: Mutex::new(String::new()),
            completed_turn_text: Mutex::new(String::new()),
            stop_armed: Mutex::new(None),
            objective_done: AtomicU8::new(UNSET),
            correction_legs: AtomicU64::new(0),
            load_in_flight: AtomicU8::new(UNSET),
            alive: AtomicU8::new(UNSET),
            settled: AtomicU8::new(UNSET),
            terminal_reached: AtomicU8::new(UNSET),
            evict_after: Mutex::new(None),
            resume_streak: AtomicU64::new(0),
            resume_lock: tokio::sync::Mutex::new(()),
            scrub: self.scrub.clone(),
        });
        // Evict terminal-reached sessions past their retention window:
        // the spine owns canonical truth, and a shared controller must not
        // retain every completed run's projection forever. The window
        // keeps the owning run's collect (and any concurrent reader) safe;
        // transport-dead sessions (no terminal) stay retained for
        // reattach.
        let now = std::time::Instant::now();
        self.sessions.lock().retain(|_, state| {
            if state.terminal_reached.load(Ordering::SeqCst) == UNSET {
                return true;
            }
            match *state.evict_after.lock() {
                Some(at) => now < at,
                None => false,
            }
        });
        let key = format!("pending-{}", Uuid::new_v4().simple());
        self.sessions.lock().insert(key.clone(), state.clone());

        // The open is guarded by a cancellation-safe watchdog: if the
        // caller's own ceiling cancels the start future mid-open, the
        // future's cleanup arms never run — the watchdog still does. The
        // settled flag is claimed with a compare-exchange so the watchdog
        // and the completing open can never BOTH own the teardown, it
        // kills the child through the state slot, and it removes the
        // exact pending entry — a cancelled start leaks neither a process
        // nor state.
        let open_budget = self.config.startup_timeout * 3 + self.config.turn_timeout;
        let watchdog_state = Arc::downgrade(&state);
        let watchdog_sessions = Arc::clone(&self.sessions);
        let watchdog_key = key.clone();
        zeroclaw_spawn::spawn!(async move {
            tokio::time::sleep(open_budget).await;
            if let Some(state) = watchdog_state.upgrade()
                && state
                    .settled
                    .compare_exchange(UNSET, SET, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                Self::terminate(&state).await;
                // Remove BOTH possible keys: the pending key this start
                // inserted and (if the open got far enough to publish)
                // the minted remote identity.
                watchdog_sessions.lock().remove(&watchdog_key);
                let minted = state.session_id();
                if !minted.is_empty()
                    && watchdog_sessions
                        .lock()
                        .get(&minted)
                        .is_some_and(|entry| Arc::ptr_eq(entry, &state))
                {
                    // Ownership-checked: a repeated/foreign session id can
                    // never be evicted by this teardown.
                    watchdog_sessions.lock().remove(&minted);
                }
            }
        });
        // NOTE: no adapter-side Accepted fact — the TOOL authors the
        // accepted fact (its own verbatim attach fact), and a second
        // adapter-emitted Accepted could never be deduped by id.
        let opened = self.open_session(&state, &spec.prompt).await;
        let settled_first = state
            .settled
            .compare_exchange(UNSET, SET, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if let Err(error) = opened {
            Self::terminate(&state).await;
            if !settled_first {
                // The watchdog already tore this open down (over ceiling):
                // surface the typed failure instead of a possibly-published
                // handle.
                return Err(ControllerError::Unavailable);
            }
            self.sessions.lock().remove(&key);
            return Err(error);
        }
        if !settled_first {
            self.sessions.lock().remove(&key);
            return Err(ControllerError::Unavailable);
        }
        self.sessions.lock().remove(&key);
        let session_id = state.session_id();
        // Take the lock ONLY to inspect: the decision (and the possible
        // kill) happens outside the guard, so no lock guard is ever held
        // across an await.
        // Check-then-insert under ONE lock acquisition (no await inside):
        // concurrent starts receiving the same identity cannot overwrite
        // each other.
        let collision = {
            let mut sessions = self.sessions.lock();
            if sessions
                .get(&session_id)
                .is_some_and(|existing| !Arc::ptr_eq(existing, &state))
            {
                Some(true)
            } else {
                sessions.insert(session_id.clone(), state.clone());
                None
            }
        };
        if collision.is_some() {
            // The harness repeated a live session identity: refuse rather
            // than silently re-point an existing binding. The kill is
            // synchronous (start_kill) — no lock guard is held here.
            if let Some(process) = state.process.lock().take() {
                process.pending.lock().clear();
                if let Some(mut child) = process.child.lock().take() {
                    let _ = child.start_kill();
                }
            }
            return Err(ControllerError::Refused(
                "acpx adapter repeated a live session identity (fail closed)".to_string(),
            ));
        }

        // The handle advertises the INTERSECTION of what the host asked
        // for and what this transport was configured to declare: both
        // sources must admit a capability before the gated client can
        // ever exercise it (single enforcement boundary, no unchecked
        // second source).
        let effective = spec
            .capabilities
            .intersection(AcpxControllerConfig::supported_capabilities())
            .intersection(self.config_declared()?);
        Ok(SessionHandle {
            remote_session: RemoteSessionRef::from_opaque(session_id),
            capabilities: effective,
        })
    }

    async fn watch(
        &self,
        handle: &SessionHandle,
        after_seq: u64,
        limit: usize,
    ) -> Result<SessionEventPage, ControllerError> {
        let state = self.state(handle)?;
        // Bounded long-poll: wait up to one window for events after the
        // cursor so a real harness turn (tens of seconds) does not burn
        // the run's bounded action budget on busy polling. The wakeup is
        // registered before the check (no lost-wakeup).
        let notified = state.event_signal.notified();
        let page = {
            let events = state.events.lock();
            let pending: Vec<ControllerEvent> = events
                .iter()
                .filter(|event| event.seq > after_seq)
                .take(limit.max(1))
                .cloned()
                .collect();
            let next_seq = pending
                .last()
                .map(|event| event.seq)
                .unwrap_or(after_seq)
                .max(after_seq);
            SessionEventPage {
                events: pending,
                next_seq,
            }
        };
        if page.events.is_empty() && state.alive.load(Ordering::SeqCst) == ALIVE_YES {
            let _ = tokio::time::timeout(WATCH_POLL_WINDOW, notified).await;
            // Re-read once after the window (or wakeup).
            let events = state.events.lock();
            let pending: Vec<ControllerEvent> = events
                .iter()
                .filter(|event| event.seq > after_seq)
                .take(limit.max(1))
                .cloned()
                .collect();
            drop(events);
            let next_seq = pending
                .last()
                .map(|event| event.seq)
                .unwrap_or(after_seq)
                .max(after_seq);
            return Ok(SessionEventPage {
                events: pending,
                next_seq,
            });
        }
        if state.alive.load(Ordering::SeqCst) == ALIVE_YES || !page.events.is_empty() {
            return Ok(page);
        }
        // Dead transport: bounded resume attempts so the tool's
        // reconnect-retry path (post `sink.reconnect`) brings the SAME
        // session back instead of spinning unavailable — while a
        // flapping adapter (load succeeds, then closes) can never trap
        // the watch in an endless success/fail cycle. The streak is
        // consumed here (one per attempt) and decays only on observed
        // progress below.
        if state.resume_streak.fetch_add(1, Ordering::SeqCst) >= MAX_WATCH_RESUMES {
            return Err(ControllerError::Unavailable);
        }
        self.resume_session(&state).await?;
        let page = self.watch(handle, after_seq, limit).await?;
        // The streak decays only on OBSERVED progress: a resume that
        // succeeds and then dies again before any event keeps the streak
        // climbing until the bound fails the run typed.
        if !page.events.is_empty() {
            state.resume_streak.store(0, Ordering::SeqCst);
        }
        Ok(page)
    }

    async fn prompt(
        &self,
        handle: &SessionHandle,
        text: &str,
    ) -> Result<PromptReceipt, ControllerError> {
        let state = self.state(handle)?;
        let process = Self::process(&state)?;
        state.correction_legs.fetch_add(1, Ordering::SeqCst);
        Self::start_turn(
            &process,
            &state,
            text,
            self.config.turn_timeout,
            &self.scrub,
        )
        .await?;
        Ok(PromptReceipt {
            accepted: true,
            detail: None,
        })
    }

    async fn interrupt(&self, handle: &SessionHandle) -> Result<(), ControllerError> {
        let state = self.state(handle)?;
        let process = Self::process(&state)?;
        let session_id = state.session_id();
        Self::notify(&process, "session/cancel", json!({"sessionId": session_id})).await
    }

    async fn stop(
        &self,
        handle: &SessionHandle,
        graceful: bool,
    ) -> Result<SessionStopReceipt, ControllerError> {
        let state = self.state(handle)?;
        let confirmation =
            AuthorityConfirmationRef::from_opaque(format!("acpx-stop-{}", Uuid::new_v4().simple()));
        if !graceful {
            // An immediate host kill is run-scoped bookkeeping: the child
            // dies, but NO terminal fact is minted into the event stream
            // (the spine keeps its honest state; a fact that nobody
            // authored through the lifecycle vocabulary would be noise).
            Self::terminate(&state).await;
            return Ok(SessionStopReceipt {
                confirmed: true,
                authority_confirmation_ref: Some(confirmation),
                detail: Some("session terminated by host (immediate)".to_string()),
            });
        }
        // Graceful: arm the confirmation, cancel the current turn (if
        // any), give the harness the grace window to observe it, then
        // terminate. The terminal fact binds the SAME confirmation the
        // receipt returns.
        *state.stop_armed.lock() = Some(confirmation.clone());
        if let Ok(process) = Self::process(&state) {
            let session_id = state.session_id();
            let _ =
                Self::notify(&process, "session/cancel", json!({"sessionId": session_id})).await;
        }
        let deadline = tokio::time::Instant::now() + CANCEL_GRACE;
        while !Self::terminal_observed(&state, &confirmation)
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Self::terminate(&state).await;
        if !Self::terminal_observed(&state, &confirmation) {
            // The harness never surfaced the cancel; the host still owns
            // the terminal: it killed the transport and mints the fact
            // bound to the same confirmation reference.
            state.push_event(
                SessionEventKindV1::Terminal,
                Some(SessionTerminalOutcomeV1::Cancelled {
                    confirmation: confirmation.clone(),
                }),
                Some("session terminated by host after cancel window".to_string()),
            );
        }
        Ok(SessionStopReceipt {
            confirmed: true,
            authority_confirmation_ref: Some(confirmation),
            detail: None,
        })
    }

    async fn collect(&self, handle: &SessionHandle) -> Result<SessionCollectView, ControllerError> {
        let state = self.state(handle)?;
        let mut hasher = Sha256::new();
        let summary = {
            let text = state.completed_turn_text.lock();
            hasher.update(text.as_bytes());
            if text.is_empty() {
                None
            } else {
                Some(bounded_text(&text, SUMMARY_CEILING, &state.scrub))
            }
        };
        let digest = format!("{:x}", hasher.finalize());
        let session_id = state.session_id();
        Ok(SessionCollectView {
            summary,
            digest,
            evidence_refs: vec![format!("acpx-session/{session_id}")],
        })
    }

    async fn reattach(
        &self,
        _adapter_connection: &zeroclaw_api::session_exec::AdapterConnectionRef,
        remote_session: &RemoteSessionRef,
        _resume_from_revision: u64,
    ) -> Result<SessionHandle, ControllerError> {
        // The session state may live in THIS controller (in-process
        // recovery) or nowhere (the controller was restarted with the
        // host): both resume to the SAME harness session id — the remote
        // identity the host already holds is the only input, and no new
        // session is minted.
        let state = {
            let sessions = self.sessions.lock();
            sessions.get(remote_session.as_str()).cloned()
        };
        if let Some(existing) = {
            let sessions = self.sessions.lock();
            sessions.get(remote_session.as_str()).cloned()
        } {
            // Already live in this controller: an idempotent reattach —
            // never spawn a second transport over a live session.
            if existing.alive.load(Ordering::SeqCst) == ALIVE_YES {
                return Ok(SessionHandle {
                    remote_session: remote_session.clone(),
                    capabilities: AcpxControllerConfig::supported_capabilities()
                        .intersection(self.config_declared()?),
                });
            }
        }
        let state = match state {
            Some(state) => state,
            None => Arc::new(SessionState {
                session_id: Mutex::new(remote_session.as_str().to_string()),
                event_signal: Arc::new(tokio::sync::Notify::new()),
                process: Mutex::new(None),
                events: Mutex::new(Vec::new()),
                next_seq: AtomicU64::new(0),
                update_ordinal: AtomicU64::new(0),
                ordinal_salt: u64::from_le_bytes(
                    Uuid::new_v4().as_bytes()[..8].try_into().unwrap_or([0; 8]),
                ),
                last_turn_text: Mutex::new(String::new()),
                completed_turn_text: Mutex::new(String::new()),
                stop_armed: Mutex::new(None),
                objective_done: AtomicU8::new(SET),
                correction_legs: AtomicU64::new(0),
                load_in_flight: AtomicU8::new(UNSET),
                alive: AtomicU8::new(UNSET),
                settled: AtomicU8::new(SET),
                terminal_reached: AtomicU8::new(UNSET),
                evict_after: Mutex::new(None),
                resume_streak: AtomicU64::new(0),
                resume_lock: tokio::sync::Mutex::new(()),
                scrub: self.scrub.clone(),
            }),
        };
        // resume_session self-cleans its own child on every failure leg.
        // The map entry is deliberately NOT removed: for an existing state
        // it is shared with any concurrent recovery (Arc-identity cannot
        // prove exclusive ownership), and a dead retained state is simply
        // retryable.
        self.resume_session(&state).await?;
        self.sessions
            .lock()
            .insert(remote_session.as_str().to_string(), state.clone());
        Ok(SessionHandle {
            remote_session: remote_session.clone(),
            // Same single enforcement boundary as start: the transport's
            // supported set intersected with its configured declaration.
            capabilities: AcpxControllerConfig::supported_capabilities()
                .intersection(self.config_declared()?),
        })
    }
}

impl Drop for AcpxController {
    fn drop(&mut self) {
        // Synchronous last-resort kill: the controller is not `Clone`, so
        // this drop IS the final ownership release — no clone can be
        // holding sessions alive, and the sweep cannot murder another
        // owner's sessions.
        for state in self.sessions.lock().values() {
            state.alive.store(UNSET, Ordering::SeqCst);
            let process = state.process.lock().take();
            if let Some(process) = process {
                process.pending.lock().clear();
                let child = process.child.lock().take();
                if let Some(child) = child {
                    #[cfg(unix)]
                    if let Some(pid) = child.id() {
                        let pgid = pid as libc::pid_t;
                        unsafe {
                            libc::killpg(pgid, libc::SIGKILL);
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        let mut child = child;
                        let _ = child.start_kill();
                    }
                }
            }
        }
    }
}

impl AcpxController {
    /// The transport's configured declared set (one side of the
    /// capability intersection enforced at start/reattach).
    fn config_declared(&self) -> Result<SessionCapabilities, ControllerError> {
        SessionCapabilities::from_names(
            &self
                .config
                .declared_capabilities
                .iter()
                .map(|name| (*name).to_string())
                .collect::<Vec<String>>(),
        )
    }

    /// Test/observability accessor: whether the harness child behind this
    /// handle is still alive. Doc-hidden: not part of the port contract.
    #[doc(hidden)]
    pub fn session_alive_for_test(&self, handle: &SessionHandle) -> bool {
        match self.state(handle) {
            Ok(state) => state.alive.load(Ordering::SeqCst) == ALIVE_YES,
            Err(_) => false,
        }
    }

    fn terminal_observed(state: &SessionState, confirmation: &AuthorityConfirmationRef) -> bool {
        state.events.lock().iter().any(|event| {
            event.outcome.as_ref().is_some_and(|outcome| {
                matches!(outcome, SessionTerminalOutcomeV1::Cancelled { confirmation: observed }
                    if observed.as_str() == confirmation.as_str())
            })
        })
    }
}

impl Drop for AcpxProcess {
    fn drop(&mut self) {
        // Drop-path kill: the process group is signalled even when the
        // session dies without an explicit terminate (controller dropped,
        // run abandoned), so no adapter wrapper or its platform-binary
        // child survives the session.
        if let Some(child) = self.child.lock().take() {
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                let pgid = pid as libc::pid_t;
                unsafe {
                    libc::killpg(pgid, libc::SIGKILL);
                }
            }
            #[cfg(not(unix))]
            {
                let mut child = child;
                let _ = child.start_kill();
            }
            // The (already signalled) direct child is dropped here; tokio
            // hands the zombie to its orphan reaper.
        }
    }
}

/// The reader task: frames bounded lines, resolves pending responses,
/// answers harness requests, maps notifications. Owns the turn-contract
/// event mapping. At EOF the state goes dead (fail closed).
async fn reader_task(
    stdout: tokio::process::ChildStdout,
    process: Arc<AcpxProcess>,
    state: Arc<SessionState>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    line_ceiling: usize,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        match read_bounded_line(&mut reader, &mut line, line_ceiling).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let Ok(message) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string);
        let id = message.get("id").and_then(Value::as_u64);
        match (method, id) {
            // Response to one of our requests.
            (None, Some(id)) => {
                // Turn-contract mapping: only `session/prompt` responses
                // carry a stop reason. Extract before the waiter takes
                // the message.
                let stop_reason = message
                    .pointer("/result/stopReason")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Some(waiter) = pending.lock().remove(&id) {
                    let _ = waiter.send(message);
                }
                if let Some(stop_reason) = stop_reason {
                    handle_turn_end(&state, &process, &stop_reason).await;
                }
            }
            // Server→client request: the harness may ask the CLIENT for
            // permission or file access. This client holds no authority:
            // permission requests are denied (cancelled); anything else
            // is answered with a typed method-not-available error. Never
            // a hang, never an approval.
            (Some(method), Some(request_id)) => {
                let response = if method == "session/request_permission" {
                    state.push_event(
                        SessionEventKindV1::Progress,
                        None,
                        Some(
                            "harness permission request denied (client holds no authority)"
                                .to_string(),
                        ),
                    );
                    json!({
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "result": {"outcome": {"outcome": "cancelled"}},
                    })
                } else {
                    json!({
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "error": {
                            "code": -32601,
                            "message": "method not available to this acpx carrier client",
                        },
                    })
                };
                let _ = process.stdin_tx.send(response.to_string()).await;
            }
            // Notification.
            (Some(method), None) => {
                if method == "session/update"
                    && state.load_in_flight.load(Ordering::SeqCst) == UNSET
                {
                    handle_session_update(&state, &message);
                }
            }
            (None, None) => {}
        }
    }
    // EOF: the transport is gone. Retirement is ownership-scoped: the
    // alive flag drops only when the slot still holds THIS reader's
    // process (a newer resume's child owns its own liveness).
    let mut slot = state.process.lock();
    if slot
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, &process))
    {
        // Same guard scope: the slot retirement and the alive transition
        // are one atomic step (no TOCTOU against a newer resume).
        state.alive.store(UNSET, Ordering::SeqCst);
        *slot = None;
    }
    // Resolve every pending waiter with nothing: they surface as
    // Unavailable (fail closed).
    pending.lock().clear();
}

/// Kill the child and clear the process slot (called from the reader
/// when the run's terminal is observed: the host-owned session is over).
async fn shutdown_transport(process: &AcpxProcess, state: &SessionState) {
    state.alive.store(UNSET, Ordering::SeqCst);
    process.pending.lock().clear();
    let child = process.child.lock().take();
    if let Some(mut child) = child {
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            let pgid = pid as libc::pid_t;
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
        #[cfg(not(unix))]
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    *state.process.lock() = None;
}

/// The turn contract: the objective turn's end is `InputRequired` (the
/// session is genuinely awaiting the next instruction); an answered
/// correction leg closes the host-owned run (`completed`); a host-armed
/// cancel binds its confirmation; anything else fails.
async fn handle_turn_end(state: &Arc<SessionState>, process: &Arc<AcpxProcess>, stop_reason: &str) {
    let armed = state.stop_armed.lock().clone();
    let final_text = std::mem::take(&mut *state.last_turn_text.lock());
    *state.completed_turn_text.lock() = final_text.clone();
    let summary = (!final_text.is_empty()).then_some(final_text);
    if let Some(confirmation) = armed
        && stop_reason == "cancelled"
    {
        state.push_event(
            SessionEventKindV1::Terminal,
            Some(SessionTerminalOutcomeV1::Cancelled { confirmation }),
            summary,
        );
        shutdown_transport(process, state).await;
        return;
    }
    match stop_reason {
        "end_turn" => {
            if state.objective_done.load(Ordering::SeqCst) == UNSET {
                state.objective_done.store(SET, Ordering::SeqCst);
                state.push_event(SessionEventKindV1::InputRequired, None, summary);
            } else if state.correction_legs.load(Ordering::SeqCst) > 0 {
                // An answered correction leg closes the host-owned run.
                state.push_event(
                    SessionEventKindV1::Terminal,
                    Some(SessionTerminalOutcomeV1::Completed),
                    summary,
                );
                shutdown_transport(process, state).await;
            } else {
                state.push_event(SessionEventKindV1::InputRequired, None, summary);
            }
        }
        "cancelled" => {
            // A cancel this controller did not arm (harness-side); the
            // host did not confirm it — surface progress, fabricate no
            // terminal.
            state.push_event(
                SessionEventKindV1::Progress,
                None,
                Some("turn cancelled".to_string()),
            );
        }
        other => {
            // refusal / max_tokens / anything else: the run fails.
            state.push_event(
                SessionEventKindV1::Terminal,
                Some(SessionTerminalOutcomeV1::Failed),
                Some(format!("turn ended: {other}")),
            );
            shutdown_transport(process, state).await;
        }
    }
}

/// Map one `session/update` notification into the bounded event
/// projection. Update KINDS are facts; tool inputs and message deltas are
/// content: deltas accumulate into the turn buffer (bounded), tool
/// activity surfaces as a kind-only progress fact (no commands, no paths,
/// no titles).
fn handle_session_update(state: &Arc<SessionState>, message: &Value) {
    let Some(update) = message
        .pointer("/params/update")
        .or_else(|| message.get("params"))
    else {
        return;
    };
    let Some(kind) = update
        .get("sessionUpdate")
        .or_else(|| update.get("type"))
        .or_else(|| update.get("kind"))
        .and_then(Value::as_str)
    else {
        return;
    };
    match kind {
        "agent_message_chunk" => {
            if let Some(text) = update
                .pointer("/content/text")
                .or_else(|| update.get("text"))
                .and_then(Value::as_str)
            {
                const TURN_TEXT_CEILING: usize = SUMMARY_CEILING * 4;
                let mut buffer = state.last_turn_text.lock();
                if buffer.len() < TURN_TEXT_CEILING {
                    buffer.push_str(text);
                    if buffer.len() > TURN_TEXT_CEILING {
                        *buffer = bounded_text(&buffer.clone(), TURN_TEXT_CEILING, &state.scrub);
                    }
                }
            }
        }
        "tool_call" | "tool_call_update" => {
            state.push_event(
                SessionEventKindV1::Progress,
                None,
                Some("harness tool activity".to_string()),
            );
        }
        _ => {
            // available_commands_update / config_option_update / others:
            // protocol churn, not session facts.
        }
    }
}

/// Read one newline-terminated line, bounded: returns Ok(0) at EOF.
/// Oversized lines are truncated in place (the excess is drained and
/// discarded, never buffered unbounded).
async fn read_bounded_line(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
    out: &mut Vec<u8>,
    ceiling: usize,
) -> std::io::Result<usize> {
    let mut total = 0usize;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(total);
        }
        match available.iter().position(|byte| *byte == b'\n') {
            Some(newline) => {
                let take = newline;
                if total < ceiling {
                    let room = (ceiling - total).min(take);
                    out.extend_from_slice(&available[..room]);
                }
                total += take + 1;
                let consumed = newline + 1;
                reader.consume(consumed);
                return Ok(total);
            }
            None => {
                let take = available.len();
                if total < ceiling {
                    let room = (ceiling - total).min(take);
                    out.extend_from_slice(&available[..room]);
                }
                total += take;
                reader.consume(take);
            }
        }
    }
}

async fn writer_task(mut stdin: tokio::process::ChildStdin, mut rx: mpsc::Receiver<String>) {
    while let Some(line) = rx.recv().await {
        if stdin.write_all(line.as_bytes()).await.is_err() {
            break;
        }
        if stdin.write_all(b"\n").await.is_err() {
            break;
        }
        if stdin.flush().await.is_err() {
            break;
        }
    }
}

/// Bounded stderr drain: fixed-buffer reads, keeps nothing, logs
/// nothing, and never accumulates regardless of newline framing.
async fn drain_task(mut stderr: tokio::process::ChildStderr) {
    let mut sink = [0u8; 8192];
    loop {
        match stderr.read(&mut sink).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

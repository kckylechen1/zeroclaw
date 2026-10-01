#![forbid(unsafe_code)]

//! Codex app-server carrier. The host supplies all process authority.
//!
//! The first completed turn requests parent input; a completed correction
//! finishes the ephemeral run, matching the ACPX controller's contract.
//! Reattachment preserves retained event identities and reconciles a lost
//! turn from `thread/resume`; it never replays a prompt. After host restart,
//! unknown sessions are refused because their authority and event journal
//! cannot be reconstructed from a thread id alone.
//!
//! Launch the actual app-server executable, not a detached shell wrapper.
//! Child teardown covers the owned process; process-tree containment belongs
//! to the host's sandbox/job container. No process-exit receipt is presented
//! as proof that arbitrary descendant processes have stopped.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, watch, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::{timeout, timeout_at, Instant};
use uuid::Uuid;
use zeroclaw_api::session_exec::{
    AdapterConnectionRef, AuthorityConfirmationRef, RemoteSessionRef, SessionEventIdRef,
    SessionEventKindV1, SessionTerminalOutcomeV1,
};

use super::controller::{
    ControllerError, ControllerEvent, PromptReceipt, SessionCapabilities, SessionCollectView,
    SessionController, SessionEventPage, SessionHandle, SessionStartSpec, SessionStopReceipt,
};

const SUMMARY_CHARS: usize = 2000;
const TEXT_BYTES: usize = 32 * 1024;
const PROMPT_BYTES: usize = 64 * 1024;
const MAX_FRAME: usize = 1024 * 1024;
const EVENT_LIMIT: usize = 512;
const SESSION_LIMIT: usize = 64;
const QUEUE_LIMIT: usize = 8;
const RETENTION: Duration = Duration::from_secs(300);
const STOP_GRACE: Duration = Duration::from_secs(10);
const WATCH_WINDOW: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct CodexControllerConfig {
    pub command: PathBuf,
    pub args: Vec<String>,
    /// Complete child environment allowlist; nothing is inherited.
    pub env: HashMap<String, String>,
    pub workspace_root: PathBuf,
    pub model: Option<String>,
    pub sandbox: Option<String>,
    pub approval_policy: Option<String>,
    pub startup_timeout: Duration,
    pub turn_timeout: Duration,
    pub max_line_bytes: usize,
    pub declared_capabilities: Vec<&'static str>,
}

impl CodexControllerConfig {
    #[must_use]
    pub fn new(command: PathBuf, workspace_root: PathBuf) -> Self {
        Self {
            command,
            args: vec!["app-server".into(), "--stdio".into()],
            env: HashMap::new(),
            workspace_root,
            model: None,
            sandbox: Some("read-only".into()),
            approval_policy: Some("on-request".into()),
            startup_timeout: Duration::from_secs(30),
            turn_timeout: Duration::from_secs(600),
            max_line_bytes: MAX_FRAME,
            declared_capabilities: Self::supported_capabilities().as_names(),
        }
    }

    #[must_use]
    pub fn supported_capabilities() -> SessionCapabilities {
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

    fn declared(&self) -> Result<SessionCapabilities, ControllerError> {
        SessionCapabilities::from_names(
            &self
                .declared_capabilities
                .iter()
                .map(|s| (*s).to_owned())
                .collect::<Vec<_>>(),
        )
    }

    fn thread_params(&self, thread: Option<&str>) -> Value {
        let mut p = json!({"cwd": self.workspace_root});
        if let Some(id) = thread {
            p["threadId"] = json!(id);
        }
        if let Some(v) = &self.model {
            p["model"] = json!(v);
        }
        if let Some(v) = &self.sandbox {
            p["sandbox"] = json!(v);
        }
        if let Some(v) = &self.approval_policy {
            p["approvalPolicy"] = json!(v);
        }
        p
    }
}

impl fmt::Debug for CodexControllerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodexControllerConfig")
            .field("command", &"<host-configured>")
            .field("args", &"<redacted>")
            .field("env", &"<redacted>")
            .field("workspace_root", &"<host-configured>")
            .field("model", &"<host-configured>")
            .field("sandbox", &self.sandbox)
            .field("approval_policy", &self.approval_policy)
            .field("startup_timeout", &self.startup_timeout)
            .field("turn_timeout", &self.turn_timeout)
            .field("max_line_bytes", &self.max_line_bytes)
            .field("declared_capabilities", &self.declared_capabilities)
            .finish()
    }
}

fn refused(reason: &'static str) -> ControllerError {
    ControllerError::Refused(reason.into())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

fn get_id(value: &Value, pointer: &str) -> Result<String, ControllerError> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|id| valid_id(id))
        .map(str::to_owned)
        .ok_or(ControllerError::Unavailable)
}

#[derive(Default)]
struct TextBuffer {
    text: String,
    truncated: bool,
}

impl TextBuffer {
    fn append(&mut self, text: &str) {
        if self.truncated {
            return;
        }
        let room = TEXT_BYTES.saturating_sub(self.text.len());
        let mut end = room.min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&text[..end]);
        self.truncated = end < text.len();
    }

    fn summary(&self, scrub: &[String]) -> Option<String> {
        let mut text = self.text.clone();
        // Redact before truncating, including secrets split across deltas.
        // Remove a possible secret prefix at the retained buffer boundary.
        for needle in scrub.iter().filter(|s| !s.is_empty()) {
            if self.truncated {
                let partial = needle
                    .char_indices()
                    .map(|(i, _)| i)
                    .filter(|i| *i > 0 && text.ends_with(&needle[..*i]))
                    .max();
                if let Some(n) = partial {
                    text.truncate(text.len() - n);
                }
            }
            text = text.replace(needle, "*");
        }
        let mut result: String = text.chars().take(SUMMARY_CHARS).collect();
        if self.truncated || text.chars().count() > SUMMARY_CHARS {
            if result.chars().count() == SUMMARY_CHARS {
                result.pop();
            }
            result.push('…');
        }
        (!result.is_empty()).then_some(result)
    }
}

struct Projection {
    events: VecDeque<ControllerEvent>,
    seq: u64,
    salt: Uuid,
    summary: Option<String>,
    terminal: bool,
    available: bool,
    retired_at: Option<Instant>,
}

impl Projection {
    fn new() -> Self {
        Self {
            events: VecDeque::new(),
            seq: 0,
            salt: Uuid::new_v4(),
            summary: None,
            terminal: false,
            available: false,
            retired_at: None,
        }
    }

    fn push(
        &mut self,
        kind: SessionEventKindV1,
        outcome: Option<SessionTerminalOutcomeV1>,
        summary: Option<String>,
    ) {
        if self.terminal {
            return;
        }
        let Some(seq) = self.seq.checked_add(1) else {
            self.available = false;
            self.retired_at = Some(Instant::now());
            return;
        };
        self.seq = seq;
        if kind == SessionEventKindV1::Terminal {
            self.terminal = true;
            self.retired_at = Some(Instant::now());
        }
        if self.events.len() == EVENT_LIMIT {
            self.events.pop_front();
        }
        self.events.push_back(ControllerEvent {
            seq,
            event_id: SessionEventIdRef::from_opaque(format!("codex-{}-{seq}", self.salt)),
            kind,
            outcome,
            summary: summary.map(|s| s.chars().take(SUMMARY_CHARS).collect()),
        });
    }

    fn page(&self, after: u64, limit: usize) -> Result<SessionEventPage, ControllerError> {
        if self
            .events
            .front()
            .is_some_and(|e| after < e.seq.saturating_sub(1))
        {
            return Err(refused(
                "codex event cursor expired; replay would omit facts",
            ));
        }
        if after > self.seq {
            return Err(refused("codex event cursor is ahead of this session"));
        }
        let events: Vec<_> = self
            .events
            .iter()
            .filter(|e| e.seq > after)
            .take(limit.min(EVENT_LIMIT))
            .cloned()
            .collect();
        let next_seq = events.last().map_or(after, |e| e.seq);
        if events.is_empty() && !self.available && !self.terminal {
            return Err(ControllerError::Unavailable);
        }
        Ok(SessionEventPage { events, next_seq })
    }
}

type Reply<T> = oneshot::Sender<Result<T, ControllerError>>;

enum Operation {
    Prompt(String, Reply<()>),
    Interrupt(Reply<()>),
    Stop(bool, Reply<SessionStopReceipt>),
    Resume(Reply<()>),
}

struct Entry {
    adapter: AdapterConnectionRef,
    capabilities: SessionCapabilities,
    max_prompt_bytes: usize,
    projection: Arc<Mutex<Projection>>,
    changed: watch::Sender<u64>,
    commands: mpsc::Sender<Operation>,
    shutdown: Option<oneshot::Sender<()>>,
    _permit: OwnedSemaphorePermit,
}

impl Drop for Entry {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

pub struct CodexController {
    config: Arc<CodexControllerConfig>,
    sessions: Mutex<HashMap<String, Arc<Entry>>>,
    slots: Arc<Semaphore>,
}

impl CodexController {
    pub fn new(mut config: CodexControllerConfig) -> Result<Self, ControllerError> {
        if !config.command.is_absolute() || !config.command.is_file() {
            return Err(refused("codex command must be an absolute existing file"));
        }
        if !config.workspace_root.is_absolute()
            || !config.workspace_root.is_dir()
            || config.workspace_root.to_str().is_none()
        {
            return Err(refused(
                "codex workspace must be an absolute UTF-8 directory",
            ));
        }
        config.workspace_root = config
            .workspace_root
            .canonicalize()
            .map_err(|_| ControllerError::Unavailable)?;
        if !(1024..=MAX_FRAME).contains(&config.max_line_bytes)
            || config.startup_timeout.is_zero()
            || config.turn_timeout.is_zero()
            || config.startup_timeout > Duration::from_secs(3600)
            || config.turn_timeout > Duration::from_secs(86400)
        {
            return Err(refused("invalid codex transport bounds"));
        }
        let declared = config.declared()?;
        if declared.intersection(CodexControllerConfig::supported_capabilities()) != declared {
            return Err(refused("unsupported codex capability declaration"));
        }
        if config.args.len() > 128
            || config.env.len() > 128
            || config
                .args
                .iter()
                .any(|s| s.len() > TEXT_BYTES || s.contains('\0'))
            || config.env.iter().any(|(k, v)| {
                k.is_empty()
                    || k.len() > 256
                    || k.contains(['=', '\0'])
                    || v.len() > TEXT_BYTES
                    || v.contains('\0')
            })
            || [&config.model, &config.sandbox, &config.approval_policy]
                .into_iter()
                .flatten()
                .any(|s| s.is_empty() || s.len() > 256 || s.contains('\0'))
        {
            return Err(refused("invalid codex process configuration"));
        }
        Ok(Self {
            config: Arc::new(config),
            sessions: Mutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(SESSION_LIMIT)),
        })
    }

    fn entry(
        &self,
        handle: &SessionHandle,
        operation: &str,
    ) -> Result<Arc<Entry>, ControllerError> {
        let entry = self
            .sessions
            .lock()
            .get(handle.remote_session.as_str())
            .cloned()
            .ok_or(ControllerError::Unavailable)?;
        // Never use caller-controlled handle capabilities to widen authority.
        if handle.capabilities != entry.capabilities {
            return Err(refused(
                "codex handle capabilities do not match the admitted session",
            ));
        }
        if let Some(operation) = entry.capabilities.unsupported_operation(operation) {
            return Err(ControllerError::UnsupportedByLifecycleOwner { operation });
        }
        Ok(entry)
    }

    fn prune(&self) {
        self.sessions.lock().retain(|_, e| {
            e.projection
                .lock()
                .retired_at
                .is_none_or(|t| t.elapsed() < RETENTION)
        });
    }

    async fn send<T>(
        &self,
        e: &Entry,
        op: Operation,
        rx: oneshot::Receiver<Result<T, ControllerError>>,
    ) -> Result<T, ControllerError> {
        // An enqueued operation remains authorized if its caller is cancelled.
        // Every operation has its own actor-side deadline; no unbounded waiters.
        e.commands
            .try_send(op)
            .map_err(|_| ControllerError::Unavailable)?;
        rx.await.map_err(|_| ControllerError::Unavailable)?
    }
}

#[async_trait]
impl SessionController for CodexController {
    async fn start(&self, spec: &SessionStartSpec) -> Result<SessionHandle, ControllerError> {
        if spec.prompt.len() > spec.max_prompt_bytes.min(PROMPT_BYTES) {
            return Err(refused("codex prompt exceeds the admitted byte ceiling"));
        }
        let admitted = self.config.declared()?;
        if spec.capabilities.intersection(admitted) != spec.capabilities {
            return Err(refused(
                "requested capabilities exceed the codex host declaration",
            ));
        }
        self.prune();
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| refused("codex session capacity exhausted"))?;
        let projection = Arc::new(Mutex::new(Projection::new()));
        let (changed, _) = watch::channel(0);
        let (commands, rx) = mpsc::channel(QUEUE_LIMIT);
        let (shutdown, shutdown_rx) = oneshot::channel();
        let (ready, result) = oneshot::channel();
        let entry = Arc::new(Entry {
            adapter: spec.adapter_connection.clone(),
            capabilities: spec.capabilities,
            max_prompt_bytes: spec.max_prompt_bytes.min(PROMPT_BYTES),
            projection: projection.clone(),
            changed: changed.clone(),
            commands,
            shutdown: Some(shutdown),
            _permit: permit,
        });
        let driver = Driver::new(self.config.clone(), projection, changed);
        let initial_prompt = spec.prompt.clone();
        zeroclaw_spawn::spawn!(run_actor(driver, initial_prompt, rx, shutdown_rx, ready,));
        let id = result.await.map_err(|_| ControllerError::Unavailable)??;
        let handle = SessionHandle {
            remote_session: RemoteSessionRef::from_opaque(&id),
            capabilities: entry.capabilities,
        };
        let mut sessions = self.sessions.lock();
        if sessions.contains_key(&id) {
            return Err(refused("codex returned a duplicate thread identity"));
        }
        sessions.insert(id, entry);
        Ok(handle)
    }

    async fn watch(
        &self,
        handle: &SessionHandle,
        after_seq: u64,
        limit: usize,
    ) -> Result<SessionEventPage, ControllerError> {
        let e = self.entry(handle, "watch")?;
        let mut changes = e.changed.subscribe();
        let deadline = Instant::now() + WATCH_WINDOW;
        loop {
            let page = e.projection.lock().page(after_seq, limit)?;
            if !page.events.is_empty() || limit == 0 || e.projection.lock().terminal {
                return Ok(page);
            }
            if timeout_at(deadline, changes.changed()).await.is_err() {
                return e.projection.lock().page(after_seq, limit);
            }
        }
    }

    async fn prompt(
        &self,
        handle: &SessionHandle,
        text: &str,
    ) -> Result<PromptReceipt, ControllerError> {
        let e = self.entry(handle, "prompt")?;
        if text.len() > e.max_prompt_bytes {
            return Err(refused("codex prompt exceeds the admitted byte ceiling"));
        }
        let (tx, rx) = oneshot::channel();
        self.send(&e, Operation::Prompt(text.into(), tx), rx)
            .await?;
        Ok(PromptReceipt {
            accepted: true,
            detail: None,
        })
    }

    async fn interrupt(&self, handle: &SessionHandle) -> Result<(), ControllerError> {
        let e = self.entry(handle, "interrupt")?;
        let (tx, rx) = oneshot::channel();
        self.send(&e, Operation::Interrupt(tx), rx).await
    }

    async fn stop(
        &self,
        handle: &SessionHandle,
        graceful: bool,
    ) -> Result<SessionStopReceipt, ControllerError> {
        let e = self.entry(handle, "stop")?;
        let (tx, rx) = oneshot::channel();
        self.send(&e, Operation::Stop(graceful, tx), rx).await
    }

    async fn collect(&self, handle: &SessionHandle) -> Result<SessionCollectView, ControllerError> {
        let e = self.entry(handle, "collect")?;
        let p = e.projection.lock();
        if !p.terminal && !p.available {
            return Err(ControllerError::Unavailable);
        }
        let summary = p.summary.clone();
        let digest = format!(
            "{:x}",
            Sha256::digest(summary.as_deref().unwrap_or_default().as_bytes())
        );
        Ok(SessionCollectView {
            summary,
            digest,
            evidence_refs: vec![],
        })
    }

    async fn reattach(
        &self,
        adapter_connection: &AdapterConnectionRef,
        remote_session: &RemoteSessionRef,
        _resume_from_revision: u64,
    ) -> Result<SessionHandle, ControllerError> {
        // The spine revision is not a controller event sequence. Retained
        // facts keep their identities; the spine deduplicates replay.
        let e = self
            .sessions
            .lock()
            .get(remote_session.as_str())
            .cloned()
            .ok_or(ControllerError::Unavailable)?;
        if &e.adapter != adapter_connection {
            return Err(refused("codex adapter binding mismatch"));
        }
        if !e.capabilities.resume {
            return Err(ControllerError::UnsupportedByLifecycleOwner {
                operation: "reattach".into(),
            });
        }
        let (tx, rx) = oneshot::channel();
        self.send(&e, Operation::Resume(tx), rx).await?;
        Ok(SessionHandle {
            remote_session: remote_session.clone(),
            capabilities: e.capabilities,
        })
    }
}

struct Task(JoinHandle<()>);
impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Transport {
    child: Child,
    incoming: mpsc::Receiver<Result<Value, ControllerError>>,
    outgoing: mpsc::Sender<Vec<u8>>,
    _reader: Task,
    _writer: Task,
}

impl Transport {
    fn spawn(config: &CodexControllerConfig) -> Result<Self, ControllerError> {
        let mut child = Command::new(&config.command)
            .args(&config.args)
            .current_dir(&config.workspace_root)
            .env_clear()
            .envs(&config.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| ControllerError::Unavailable)?;
        let stdout = child.stdout.take().ok_or(ControllerError::Unavailable)?;
        let mut stdin = child.stdin.take().ok_or(ControllerError::Unavailable)?;
        let (in_tx, incoming) = mpsc::channel(QUEUE_LIMIT);
        let (outgoing, mut out_rx) = mpsc::channel::<Vec<u8>>(QUEUE_LIMIT);
        let errors = in_tx.clone();
        let limit = config.max_line_bytes;
        let reader = zeroclaw_spawn::spawn!(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                let value = read_frame(&mut reader, limit).await;
                let failed = value.is_err();
                if in_tx.send(value).await.is_err() || failed {
                    break;
                }
            }
        });
        let writer = zeroclaw_spawn::spawn!(async move {
            while let Some(frame) = out_rx.recv().await {
                if stdin.write_all(&frame).await.is_err() || stdin.flush().await.is_err() {
                    let _ = errors.send(Err(ControllerError::Unavailable)).await;
                    break;
                }
            }
        });
        Ok(Self {
            child,
            incoming,
            outgoing,
            _reader: Task(reader),
            _writer: Task(writer),
        })
    }

    fn send(&self, value: &Value, limit: usize) -> Result<(), ControllerError> {
        self.outgoing
            .try_send(encode_frame(value, limit)?)
            .map_err(|_| ControllerError::Unavailable)
    }

    async fn close(mut self) -> Result<(), ControllerError> {
        self._reader.0.abort();
        self._writer.0.abort();
        self.child
            .start_kill()
            .map_err(|_| ControllerError::Unavailable)?;
        timeout(STOP_GRACE, self.child.wait())
            .await
            .map_err(|_| ControllerError::Unavailable)?
            .map_err(|_| ControllerError::Unavailable)?;
        Ok(())
    }
}

// Reject oversized and unterminated frames; never parse a truncated prefix.
async fn read_frame(
    reader: &mut (impl AsyncBufRead + Unpin),
    limit: usize,
) -> Result<Value, ControllerError> {
    let mut line = Vec::new();
    loop {
        let bytes = reader
            .fill_buf()
            .await
            .map_err(|_| ControllerError::Unavailable)?;
        if bytes.is_empty() {
            return Err(ControllerError::Unavailable);
        }
        let newline = bytes.iter().position(|b| *b == b'\n');
        let n = newline.unwrap_or(bytes.len());
        if n > limit.saturating_sub(line.len()) {
            return Err(ControllerError::Unavailable);
        }
        line.extend_from_slice(&bytes[..n]);
        reader.consume(n + usize::from(newline.is_some()));
        if newline.is_some() {
            break;
        }
    }
    let value: Value = serde_json::from_slice(&line).map_err(|_| ControllerError::Unavailable)?;
    if !value.is_object() || value.get("jsonrpc").is_some_and(|v| v != "2.0") {
        return Err(ControllerError::Unavailable);
    }
    Ok(value)
}

fn encode_frame(value: &Value, limit: usize) -> Result<Vec<u8>, ControllerError> {
    struct Bounded {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl std::io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other("frame too large"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut buffer = Bounded {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut buffer, value)
        .map_err(|_| refused("codex request exceeds the wire ceiling"))?;
    buffer.bytes.push(b'\n');
    Ok(buffer.bytes)
}

struct ActiveTurn {
    id: Option<String>,
    correction: bool,
    deadline: Instant,
    text: TextBuffer,
}

struct Driver {
    config: Arc<CodexControllerConfig>,
    projection: Arc<Mutex<Projection>>,
    changed: watch::Sender<u64>,
    transport: Option<Transport>,
    thread: String,
    next_request: u64,
    active: Option<ActiveTurn>,
    last_turn: Option<String>,
    objective_done: bool,
    confirmation: Option<AuthorityConfirmationRef>,
    scrub: Vec<String>,
}

impl Driver {
    fn new(
        config: Arc<CodexControllerConfig>,
        projection: Arc<Mutex<Projection>>,
        changed: watch::Sender<u64>,
    ) -> Self {
        let mut scrub = vec![config.workspace_root.to_string_lossy().into_owned()];
        scrub.extend(config.env.values().filter(|s| !s.is_empty()).cloned());
        scrub.sort_by_key(|s| std::cmp::Reverse(s.len()));
        Self {
            config,
            projection,
            changed,
            transport: None,
            thread: String::new(),
            next_request: 0,
            active: None,
            last_turn: None,
            objective_done: false,
            confirmation: None,
            scrub,
        }
    }

    fn signal(&self) {
        self.changed.send_modify(|v| *v = v.wrapping_add(1));
    }

    fn event(
        &self,
        kind: SessionEventKindV1,
        outcome: Option<SessionTerminalOutcomeV1>,
        summary: Option<String>,
    ) {
        self.projection.lock().push(kind, outcome, summary);
        self.signal();
    }

    fn available(&self, available: bool) {
        let mut p = self.projection.lock();
        p.available = available;
        p.retired_at = if p.terminal || !available {
            Some(Instant::now())
        } else {
            None
        };
        drop(p);
        self.signal();
    }

    async fn disconnect(&mut self) {
        self.available(false);
        if let Some(wire) = self.transport.take() {
            let _ = wire.close().await;
        }
    }

    fn send(&self, value: &Value) -> Result<(), ControllerError> {
        self.transport
            .as_ref()
            .ok_or(ControllerError::Unavailable)?
            .send(value, self.config.max_line_bytes)
    }

    async fn receive(&mut self) -> Result<Value, ControllerError> {
        self.transport
            .as_mut()
            .ok_or(ControllerError::Unavailable)?
            .incoming
            .recv()
            .await
            .ok_or(ControllerError::Unavailable)?
    }

    async fn rpc(
        &mut self,
        method: &str,
        params: Value,
        deadline: Instant,
        replay: bool,
    ) -> Result<Value, ControllerError> {
        self.next_request = self
            .next_request
            .checked_add(1)
            .ok_or(ControllerError::Unavailable)?;
        let id = self.next_request;
        self.send(&json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}))?;
        loop {
            let message = timeout_at(deadline, self.receive())
                .await
                .map_err(|_| ControllerError::Unavailable)??;
            if message.get("method").is_none() {
                if message.get("id").and_then(Value::as_u64) != Some(id) {
                    return Err(ControllerError::Unavailable);
                }
                if message.get("error").is_some() {
                    return Err(refused("codex rejected the protocol request"));
                }
                return message
                    .get("result")
                    .cloned()
                    .ok_or(ControllerError::Unavailable);
            }
            self.handle_message(&message, replay)?;
        }
    }

    async fn initialize(&mut self, deadline: Instant) -> Result<(), ControllerError> {
        self.transport = Some(Transport::spawn(&self.config)?);
        self.rpc(
            "initialize",
            json!({"clientInfo":{"name":"zeroclaw", "version":"0.8.3"}}),
            deadline,
            true,
        )
        .await?;
        self.send(&json!({"jsonrpc":"2.0", "method":"initialized"}))
    }

    async fn start(&mut self, prompt: &str) -> Result<String, ControllerError> {
        let deadline = Instant::now() + self.config.startup_timeout;
        self.initialize(deadline).await?;
        let result = self
            .rpc(
                "thread/start",
                self.config.thread_params(None),
                deadline,
                true,
            )
            .await?;
        self.thread = get_id(&result, "/thread/id")?;
        self.available(true);
        self.start_turn(prompt, deadline).await?;
        Ok(self.thread.clone())
    }

    async fn start_turn(&mut self, text: &str, deadline: Instant) -> Result<(), ControllerError> {
        if self.active.is_some() || self.projection.lock().terminal {
            return Err(refused("codex session is busy or terminal"));
        }
        let params = json!({"threadId":self.thread,"input":[{"type":"text","text":text}]});
        // Validate before changing state or sending any bytes.
        encode_frame(
            &json!({"jsonrpc":"2.0","id":u64::MAX,"method":"turn/start","params":params}),
            self.config.max_line_bytes,
        )?;
        let turn_deadline = Instant::now() + self.config.turn_timeout;
        self.active = Some(ActiveTurn {
            id: None,
            correction: self.objective_done,
            deadline: turn_deadline,
            text: TextBuffer::default(),
        });
        let result = self
            .rpc("turn/start", params, deadline.min(turn_deadline), false)
            .await?;
        let id = get_id(&result, "/turn/id")?;
        // A fast turn can complete before the start response is read.
        if self.last_turn.as_deref() != Some(&id) {
            self.bind_turn(&id)?;
        }
        Ok(())
    }

    fn bind_turn(&mut self, id: &str) -> Result<(), ControllerError> {
        if !valid_id(id) {
            return Err(ControllerError::Unavailable);
        }
        let active = self.active.as_mut().ok_or(ControllerError::Unavailable)?;
        if let Some(current) = &active.id {
            if current != id {
                return Err(ControllerError::Unavailable);
            }
        } else {
            active.id = Some(id.into());
            self.event(
                SessionEventKindV1::Started,
                None,
                Some("codex turn started".into()),
            );
        }
        Ok(())
    }

    fn complete(&mut self, id: &str, status: &str) -> Result<(), ControllerError> {
        if self.last_turn.as_deref() == Some(id) {
            return Ok(());
        }
        self.bind_turn(id)?;
        if !matches!(status, "completed" | "failed" | "interrupted" | "cancelled") {
            return Err(ControllerError::Unavailable);
        }
        let turn = self.active.take().ok_or(ControllerError::Unavailable)?;
        self.last_turn = Some(id.into());
        let summary = turn.text.summary(&self.scrub);
        self.projection.lock().summary = summary.clone();
        match status {
            "completed" if !turn.correction => {
                self.objective_done = true;
                self.event(SessionEventKindV1::InputRequired, None, summary);
            }
            "completed" => self.event(
                SessionEventKindV1::Terminal,
                Some(SessionTerminalOutcomeV1::Completed),
                summary,
            ),
            "failed" => self.event(
                SessionEventKindV1::Terminal,
                Some(SessionTerminalOutcomeV1::Failed),
                Some("codex turn failed".into()),
            ),
            _ => {
                if let Some(confirmation) = self.confirmation.clone() {
                    self.event(
                        SessionEventKindV1::Terminal,
                        Some(SessionTerminalOutcomeV1::Cancelled { confirmation }),
                        summary,
                    );
                } else {
                    self.event(
                        SessionEventKindV1::InputRequired,
                        None,
                        Some("codex turn interrupted".into()),
                    );
                }
            }
        }
        Ok(())
    }

    fn handle_message(&mut self, message: &Value, replay: bool) -> Result<(), ControllerError> {
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .ok_or(ControllerError::Unavailable)?;
        if let Some(id) = message.get("id") {
            if !(id.is_string() || id.is_number()) {
                return Err(ControllerError::Unavailable);
            }
            let result = match method {
                "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                    Some(json!({"decision":"decline"}))
                }
                "item/permissions/requestApproval" => {
                    Some(json!({"permissions":{},"scope":"turn"}))
                }
                "mcpServer/elicitation/request" => Some(json!({"action":"decline","content":null})),
                _ => None,
            };
            let response = if let Some(result) = result {
                json!({"jsonrpc":"2.0","id":id,"result":result})
            } else {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"client operation not supported"}})
            };
            self.send(&response)?;
            return Ok(());
        }
        if replay || self.thread.is_empty() || self.projection.lock().terminal {
            return Ok(());
        }
        let params = message.get("params").ok_or(ControllerError::Unavailable)?;
        if params.get("threadId").and_then(Value::as_str) != Some(self.thread.as_str()) {
            return Ok(());
        }
        if !matches!(
            method,
            "turn/started" | "turn/completed" | "item/agentMessage/delta" | "item/completed"
        ) {
            return Ok(());
        }
        let id = params
            .get("turnId")
            .and_then(Value::as_str)
            .or_else(|| params.pointer("/turn/id").and_then(Value::as_str))
            .ok_or(ControllerError::Unavailable)?;
        if self.last_turn.as_deref() == Some(id) {
            return Ok(());
        }
        // Ignore notifications for other turns and threads, including child agents.
        if self
            .active
            .as_ref()
            .and_then(|t| t.id.as_deref())
            .is_some_and(|known| known != id)
        {
            return Ok(());
        }
        if self.active.is_none() {
            return Ok(());
        }
        self.bind_turn(id)?;
        match method {
            "item/agentMessage/delta" => {
                let text = params
                    .get("delta")
                    .or_else(|| params.get("text"))
                    .and_then(Value::as_str)
                    .ok_or(ControllerError::Unavailable)?;
                if let Some(turn) = self.active.as_mut() {
                    turn.text.append(text);
                }
            }
            "item/completed" => {
                // Tool arguments, commands, paths and tool output stay private.
                self.event(
                    SessionEventKindV1::Progress,
                    None,
                    Some("codex item completed".into()),
                );
            }
            "turn/completed" => self.complete(
                id,
                params
                    .pointer("/turn/status")
                    .and_then(Value::as_str)
                    .ok_or(ControllerError::Unavailable)?,
            )?,
            _ => {}
        }
        Ok(())
    }

    async fn interrupt_turn(&mut self) -> Result<(), ControllerError> {
        let id = self
            .active
            .as_ref()
            .and_then(|t| t.id.clone())
            .ok_or_else(|| refused("codex has no identified active turn"))?;
        let deadline = (Instant::now() + self.config.startup_timeout)
            .min(self.active.as_ref().map_or(Instant::now(), |t| t.deadline));
        self.rpc(
            "turn/interrupt",
            json!({"threadId":self.thread,"turnId":id}),
            deadline,
            false,
        )
        .await?;
        Ok(())
    }

    async fn resume(&mut self) -> Result<(), ControllerError> {
        if self.projection.lock().terminal {
            return Ok(());
        }
        if self.transport.is_some() {
            return Ok(());
        }
        // A lost turn/start acknowledgement without a turn id cannot safely
        // be correlated with persisted history. Never issue that prompt again.
        if self.active.as_ref().is_some_and(|t| t.id.is_none()) {
            return Err(ControllerError::Unavailable);
        }
        let deadline = Instant::now() + self.config.startup_timeout;
        self.initialize(deadline).await?;
        let result = self
            .rpc(
                "thread/resume",
                self.config.thread_params(Some(&self.thread)),
                deadline,
                true,
            )
            .await?;
        if get_id(&result, "/thread/id")? != self.thread {
            return Err(ControllerError::Unavailable);
        }
        if let Some(id) = self.active.as_ref().and_then(|t| t.id.clone()) {
            let turn = result
                .pointer("/thread/turns")
                .and_then(Value::as_array)
                .and_then(|turns| {
                    turns
                        .iter()
                        .find(|t| t.get("id").and_then(Value::as_str) == Some(id.as_str()))
                })
                .ok_or(ControllerError::Unavailable)?;
            let status = turn
                .get("status")
                .and_then(Value::as_str)
                .ok_or(ControllerError::Unavailable)?;
            if !matches!(status, "completed" | "failed" | "interrupted" | "cancelled") {
                return Err(ControllerError::Unavailable);
            }
            // Use persisted complete agent messages, not a partial pre-drop buffer.
            if let Some(active) = self.active.as_mut() {
                active.text = TextBuffer::default();
                if let Some(items) = turn.get("items").and_then(Value::as_array) {
                    for item in items {
                        if let (Some("agentMessage"), Some(text)) = (
                            item.get("type").and_then(Value::as_str),
                            item.get("text").and_then(Value::as_str),
                        ) {
                            active.text.append(text);
                        }
                    }
                }
            }
            self.complete(&id, status)?;
        }
        self.available(true);
        Ok(())
    }

    fn stop_receipt(&self) -> SessionStopReceipt {
        let confirmation =
            self.projection
                .lock()
                .events
                .iter()
                .find_map(|event| match &event.outcome {
                    Some(SessionTerminalOutcomeV1::Cancelled { confirmation }) => {
                        Some(confirmation.clone())
                    }
                    _ => None,
                });
        SessionStopReceipt {
            confirmed: confirmation.is_some(), authority_confirmation_ref: confirmation,
            detail: Some("confirmation requires an observed interrupted turn; process exit alone is insufficient".into()),
        }
    }

    async fn stop(&mut self, graceful: bool) -> Result<SessionStopReceipt, ControllerError> {
        if self.projection.lock().terminal {
            return Ok(self.stop_receipt());
        }
        if graceful && self.active.is_some() && self.transport.is_some() {
            self.confirmation.get_or_insert_with(|| {
                AuthorityConfirmationRef::from_opaque(format!("codex-stop-{}", Uuid::new_v4()))
            });
            let deadline = Instant::now() + STOP_GRACE;
            let attempt = async {
                self.interrupt_turn().await?;
                while self.active.is_some() && !self.projection.lock().terminal {
                    let message = self.receive().await?;
                    self.handle_message(&message, false)?;
                }
                Ok::<(), ControllerError>(())
            };
            let _ = timeout_at(deadline, attempt).await;
        }
        // Forced teardown never invents a terminal or cancellation confirmation.
        self.disconnect().await;
        Ok(self.stop_receipt())
    }
}

async fn run_actor(
    mut d: Driver,
    prompt: String,
    mut commands: mpsc::Receiver<Operation>,
    mut shutdown: oneshot::Receiver<()>,
    ready: Reply<String>,
) {
    let result = tokio::select! {
        _ = &mut shutdown => { d.disconnect().await; return; }
        result = d.start(&prompt) => result,
    };
    let failed = result.is_err();
    if ready.send(result).is_err() || failed {
        d.disconnect().await;
        return;
    }
    // The outer select makes every operation cancellable by Entry::drop,
    // including initialization, recovery and graceful-stop RPCs.
    tokio::select! {
        _ = &mut shutdown => {}
        _ = actor_loop(&mut d, &mut commands) => {}
    }
    d.disconnect().await;
}

async fn actor_loop(d: &mut Driver, commands: &mut mpsc::Receiver<Operation>) {
    loop {
        if d.projection.lock().terminal && d.transport.is_some() {
            d.disconnect().await;
        }
        let deadline = d.active.as_ref().map(|t| t.deadline);
        let has_transport = d.transport.is_some();
        tokio::select! {
            op = commands.recv() => {
                let Some(op) = op else { break; };
                match op {
                    Operation::Prompt(text, reply) => {
                        let result = if !has_transport { Err(ControllerError::Unavailable) }
                            else { d.start_turn(&text, Instant::now() + d.config.startup_timeout).await };
                        // A pending turn after a failed RPC has uncertain execution.
                        if result.is_err() && d.active.as_ref().is_some_and(|t| t.id.is_none()) { d.disconnect().await; }
                        let _ = reply.send(result);
                    }
                    Operation::Interrupt(reply) => {
                        let result = d.interrupt_turn().await;
                        if matches!(result, Err(ControllerError::Unavailable)) { d.disconnect().await; }
                        let _ = reply.send(result);
                    }
                    Operation::Stop(graceful, reply) => { let result = d.stop(graceful).await; let _ = reply.send(result); }
                    Operation::Resume(reply) => {
                        let result = d.resume().await;
                        if result.is_err() { d.disconnect().await; }
                        let _ = reply.send(result);
                    }
                }
            }
            result = async { d.receive().await }, if has_transport => {
                if result.and_then(|m| d.handle_message(&m, false)).is_err() { d.disconnect().await; }
            }
            _ = async {
                if let Some(deadline) = deadline { tokio::time::sleep_until(deadline).await; }
            }, if has_transport && deadline.is_some() => {
                // Timeout is transport uncertainty, not an invented remote failure.
                d.disconnect().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_codex_config_defaults_and_redaction() {
        let cmd = PathBuf::from("/bin/codex");
        let ws = PathBuf::from("/tmp/ws");
        let mut config = CodexControllerConfig::new(cmd.clone(), ws.clone());
        config
            .env
            .insert("SECRET_KEY".into(), "sensitive_token".into());
        config.model = Some("gpt-6-astra".into());

        assert_eq!(config.command, cmd);
        assert_eq!(config.workspace_root, ws);
        assert_eq!(config.args, vec!["app-server", "--stdio"]);
        assert_eq!(config.model, Some("gpt-6-astra".into()));
        assert_eq!(config.sandbox, Some("read-only".into()));
        assert_eq!(config.approval_policy, Some("on-request".into()));

        let debug_str = format!("{config:?}");
        assert!(debug_str.contains("<redacted>"));
        assert!(!debug_str.contains("sensitive_token"));
    }

    #[test]
    fn test_thread_params_construction() {
        let cmd = PathBuf::from("/bin/codex");
        let ws = PathBuf::from("/tmp/workspace");
        let mut config = CodexControllerConfig::new(cmd, ws);
        config.model = Some("gpt-6-astra".into());
        config.sandbox = Some("danger-full-access".into());
        config.approval_policy = Some("never".into());

        let params = config.thread_params(Some("th_12345"));
        assert_eq!(params["cwd"], "/tmp/workspace");
        assert_eq!(params["threadId"], "th_12345");
        assert_eq!(params["model"], "gpt-6-astra");
        assert_eq!(params["sandbox"], "danger-full-access");
        assert_eq!(params["approvalPolicy"], "never");
    }

    #[test]
    fn test_valid_id() {
        assert!(valid_id("th_12345"));
        assert!(valid_id("turn-abc.1:xyz"));
        assert!(!valid_id(""));
        assert!(!valid_id("invalid id with spaces"));
        assert!(!valid_id("bad/slash"));
        assert!(!valid_id(&"a".repeat(300)));
    }

    #[test]
    fn test_text_buffer_truncation_and_scrubbing() {
        let mut buf = TextBuffer::default();
        buf.append("Hello world! My secret is sk-123456789.");
        assert_eq!(buf.text, "Hello world! My secret is sk-123456789.");

        let scrub = vec!["sk-123456789".to_string()];
        let summary = buf.summary(&scrub);
        assert!(summary.is_some());
        let text = summary.unwrap();
        assert!(!text.contains("sk-123456789"));
        assert!(text.contains('*'));
    }

    #[test]
    fn test_projection_pagination_and_monotonicity() {
        let mut proj = Projection::new();
        assert_eq!(proj.seq, 0);

        proj.push(
            SessionEventKindV1::Progress,
            None,
            Some("Working...".into()),
        );
        proj.push(SessionEventKindV1::Progress, None, Some("Done work".into()));
        assert_eq!(proj.seq, 2);

        let page = proj.page(0, 10).unwrap();
        assert_eq!(page.events.len(), 2);
        assert_eq!(page.next_seq, 2);

        let page2 = proj.page(1, 10).unwrap();
        assert_eq!(page2.events.len(), 1);
        assert_eq!(page2.events[0].seq, 2);

        proj.push(
            SessionEventKindV1::Terminal,
            Some(SessionTerminalOutcomeV1::Completed),
            Some("Finished".into()),
        );
        assert!(proj.terminal);
    }

    #[test]
    fn test_supported_and_declared_capabilities() {
        let caps = CodexControllerConfig::supported_capabilities();
        assert!(caps.observe);
        assert!(caps.wait);
        assert!(caps.prompt);
        assert!(caps.cancel);
        assert!(caps.resume);
        assert!(!caps.load);

        let cmd = PathBuf::from("/bin/codex");
        let ws = PathBuf::from("/tmp/ws");
        let config = CodexControllerConfig::new(cmd, ws);
        let declared = config.declared().unwrap();
        assert_eq!(declared, caps);
    }
}

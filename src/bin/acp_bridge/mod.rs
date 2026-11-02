use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex, mpsc},
    time::{Instant, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        http::{HeaderValue, header},
    },
};

mod state;
use state::Turn;

type Result<T> = std::result::Result<T, &'static str>;
type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
const IO: Duration = Duration::from_secs(5);
const MAX_LINE: usize = 128 * 1024;
const MAX_SESSIONS: usize = 16;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    acp_bridge: Settings,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    enabled: bool,
    endpoint: String,
    agent: String,
    session_prefix: String,
    credential_file: PathBuf,
}
struct Config {
    settings: Settings,
    token: String,
}

fn canonical(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
impl Config {
    fn load() -> Result<Self> {
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        if args.len() != 2 || args[0] != "--config" {
            return Err("ACP_CONFIG_REQUIRED");
        }
        let data = std::fs::read(&args[1]).map_err(|_| "ACP_CONFIG_UNAVAILABLE")?;
        if data.len() > MAX_LINE {
            return Err("ACP_CONFIG_TOO_LARGE");
        }
        let cfg: FileConfig =
            toml::from_str(std::str::from_utf8(&data).map_err(|_| "ACP_CONFIG_INVALID")?)
                .map_err(|_| "ACP_CONFIG_INVALID")?;
        let settings = cfg.acp_bridge;
        if !settings.enabled {
            return Err("ACP_DISABLED");
        }
        // SocketAddr parsing excludes DNS, credentials, paths and non-loopback hosts.
        let address = settings
            .endpoint
            .strip_prefix("ws://")
            .and_then(|s| s.strip_suffix("/ws/chat"))
            .ok_or("ACP_ENDPOINT_INVALID")?;
        let address: std::net::SocketAddr = address.parse().map_err(|_| "ACP_ENDPOINT_INVALID")?;
        if !address.ip().is_loopback()
            || address.port() == 0
            || !canonical(&settings.agent)
            || !canonical(&settings.session_prefix)
        {
            return Err("ACP_CONFIG_INVALID");
        }
        let token = credential(&settings.credential_file)?;
        Ok(Self { settings, token })
    }
}

#[cfg(unix)]
fn credential(path: &std::path::Path) -> Result<String> {
    use std::{
        io::Read,
        os::unix::fs::{MetadataExt, OpenOptionsExt},
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| "ACP_CREDENTIAL_INVALID")?;
    let meta = file.metadata().map_err(|_| "ACP_CREDENTIAL_INVALID")?;
    // SAFETY: getuid has no preconditions and only reads this process's identity.
    let uid = unsafe { libc::getuid() };
    if !meta.is_file() || meta.mode() & 0o7777 != 0o600 || meta.uid() != uid || meta.len() > 256 {
        return Err("ACP_CREDENTIAL_INVALID");
    }
    let mut data = String::new();
    file.take(257)
        .read_to_string(&mut data)
        .map_err(|_| "ACP_CREDENTIAL_INVALID")?;
    let token = data.trim();
    if !(32..=128).contains(&token.len())
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("ACP_CREDENTIAL_INVALID");
    }
    Ok(token.to_owned())
}
#[cfg(not(unix))]
fn credential(_: &std::path::Path) -> Result<String> {
    Err("ACP_PLATFORM_UNSUPPORTED")
}

#[derive(Clone)]
struct Output(Arc<Mutex<tokio::io::Stdout>>);
impl Output {
    async fn send(&self, frame: Value) -> Result<()> {
        timeout(IO, async {
            let mut out = self.0.lock().await;
            let text = format!("{frame}\n");
            out.write_all(text.as_bytes())
                .await
                .map_err(|_| "ACP_OUTPUT_CLOSED")?;
            out.flush().await.map_err(|_| "ACP_OUTPUT_CLOSED")
        })
        .await
        .map_err(|_| "ACP_OUTPUT_TIMEOUT")?
    }
    async fn reply(&self, id: Value, result: Value) -> Result<()> {
        self.send(json!({"jsonrpc":"2.0","id":id,"result":result}))
            .await
    }
    async fn error(&self, id: Value, code: i64, message: &str) -> Result<()> {
        self.send(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}))
            .await
    }
    async fn update(&self, session: &str, update: Value) -> Result<()> {
        self.send(json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":update}})).await
    }
}

async fn send(socket: &mut Socket, frame: Value) -> Result<()> {
    timeout(IO, socket.send(Message::Text(frame.to_string().into())))
        .await
        .map_err(|_| "GATEWAY_WRITE_TIMEOUT")?
        .map_err(|_| "GATEWAY_DISCONNECTED")
}
async fn receive(socket: &mut Socket) -> Result<Value> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) if text.len() <= MAX_LINE => {
                return serde_json::from_str(&text).map_err(|_| "GATEWAY_PROTOCOL");
            }
            Some(Ok(Message::Ping(data))) => {
                timeout(IO, socket.send(Message::Pong(data)))
                    .await
                    .map_err(|_| "GATEWAY_WRITE_TIMEOUT")?
                    .map_err(|_| "GATEWAY_DISCONNECTED")?;
            }
            Some(Ok(Message::Pong(_))) => {}
            _ => return Err("GATEWAY_DISCONNECTED"),
        }
    }
}
async fn connect(cfg: &Config, session: &str) -> Result<(Socket, String)> {
    timeout(Duration::from_secs(15), async {
        let url = format!(
            "{}?agent={}&session_id={session}",
            cfg.settings.endpoint, cfg.settings.agent
        );
        let mut request = url.into_client_request().map_err(|_| "GATEWAY_ENDPOINT")?;
        request.headers_mut().insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", cfg.token))
                .map_err(|_| "ACP_CREDENTIAL_INVALID")?,
        );
        request.headers_mut().insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("zeroclaw.v1"),
        );
        let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(MAX_LINE))
            .max_frame_size(Some(MAX_LINE));
        let (mut socket, response) =
            tokio_tungstenite::connect_async_with_config(request, Some(config), false)
                .await
                .map_err(|_| "GATEWAY_CONNECT_REJECTED")?;
        if response
            .headers()
            .get(header::SEC_WEBSOCKET_PROTOCOL)
            .and_then(|h| h.to_str().ok())
            != Some("zeroclaw.v1")
        {
            return Err("GATEWAY_PROTOCOL");
        }
        let start = receive(&mut socket).await?;
        if start["type"] != "session_start" || start["session_id"] != session {
            return Err("GATEWAY_SESSION_MISMATCH");
        }
        send(&mut socket, json!({"type":"connect"})).await?;
        let mut connected = false;
        for _ in 0..16 {
            let frame = receive(&mut socket).await?;
            if frame["type"] == "connected" {
                if frame["owner_bridge_approval_once"] != true
                    || frame["owner_bridge_cancel"] != true
                {
                    return Err("OWNER_BRIDGE_REQUIRED");
                }
                connected = true;
                break;
            }
            if frame["type"] == "error" {
                return Err("GATEWAY_CONNECT_REJECTED");
            }
        }
        if !connected {
            return Err("GATEWAY_PROTOCOL");
        }
        send(
            &mut socket,
            json!({"type":"turn_control","version":1,"op":"negotiate"}),
        )
        .await?;
        for _ in 0..16 {
            let frame = receive(&mut socket).await?;
            if frame["type"] == "turn_ready" && frame["version"] == 1 {
                let epoch = frame["epoch"]
                    .as_str()
                    .filter(|s| uuid::Uuid::parse_str(s).is_ok())
                    .ok_or("GATEWAY_PROTOCOL")?;
                return Ok((socket, epoch.to_owned()));
            }
            if matches!(frame["type"].as_str(), Some("error" | "turn_error")) {
                return Err("GATEWAY_CONNECT_REJECTED");
            }
        }
        Err("GATEWAY_PROTOCOL")
    })
    .await
    .map_err(|_| "GATEWAY_CONNECT_TIMEOUT")?
}

// A bounded actor owns each socket and pending turn. There is no execution store.
enum Command {
    Prompt(Value, String, u64),
    Cancel(u64),
    Permission(Value),
}
#[derive(Default)]
struct Admission {
    current: AtomicU64,
    next: AtomicU64,
}
impl Admission {
    fn claim(&self) -> Result<u64> {
        let generation = self
            .next
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_add(1).filter(|n| *n < u64::MAX)
            })
            .map_err(|_| "ACP_SESSION_LIMIT")?
            + 1;
        self.current
            .compare_exchange(0, generation, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|active| {
                if active == u64::MAX {
                    "ACP_SESSION_INCOMPLETE"
                } else {
                    "ACP_SESSION_BUSY"
                }
            })?;
        Ok(generation)
    }
    fn cancellation(&self) -> Option<Command> {
        match self.current.load(Ordering::Acquire) {
            0 | u64::MAX => None,
            generation => Some(Command::Cancel(generation)),
        }
    }
    fn finish(&self, generation: u64) {
        let _ = self
            .current
            .compare_exchange(generation, 0, Ordering::AcqRel, Ordering::Acquire);
    }
    fn poison(&self) {
        self.current.store(u64::MAX, Ordering::Release);
    }
}
struct Session {
    admission: Arc<Admission>,
    tx: mpsc::Sender<Command>,
    task: tokio::task::JoinHandle<()>,
}
impl Session {
    fn prompt(&self, id: Value, text: String) -> Result<()> {
        let generation = self.admission.claim()?;
        if self
            .tx
            .try_send(Command::Prompt(id, text, generation))
            .is_err()
        {
            self.admission.poison();
            return Err("ACP_SESSION_CLOSED");
        }
        Ok(())
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn prompt(params: &Value) -> Result<String> {
    let blocks = params["prompt"].as_array().ok_or("ACP_INVALID_PROMPT")?;
    if blocks.is_empty() || blocks.len() > 64 {
        return Err("ACP_INVALID_PROMPT");
    }
    let mut text = String::new();
    for block in blocks {
        if block["type"] != "text" {
            return Err("ACP_UNSUPPORTED_CONTENT");
        }
        let part = block["text"].as_str().ok_or("ACP_INVALID_PROMPT")?;
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(part);
        if text.len() > 64 * 1024 {
            return Err("ACP_INPUT_TOO_LARGE");
        }
    }
    if text.trim().is_empty() {
        return Err("ACP_EMPTY_PROMPT");
    }
    Ok(text)
}

pub(super) async fn run() -> Result<()> {
    let cfg = Arc::new(Config::load()?);
    let output = Output(Arc::new(Mutex::new(tokio::io::stdout())));
    let mut input = BufReader::new(tokio::io::stdin());
    let mut initialized = false;
    let mut sessions: HashMap<String, Session> = HashMap::new();
    loop {
        // Only this loop reads stdin: no select cancellation can discard half a line.
        let mut bytes = Vec::new();
        loop {
            let buf = input.fill_buf().await.map_err(|_| "ACP_INPUT_CLOSED")?;
            if buf.is_empty() {
                return Ok(());
            }
            let count = buf
                .iter()
                .position(|b| *b == b'\n')
                .map_or(buf.len(), |i| i + 1);
            if bytes.len() + count > MAX_LINE {
                return Err("ACP_INPUT_TOO_LARGE");
            }
            let end = buf[count - 1] == b'\n';
            bytes.extend_from_slice(&buf[..count]);
            input.consume(count);
            if end {
                break;
            }
        }
        let frame: Value = match serde_json::from_slice(&bytes) {
            Ok(frame) => frame,
            Err(_) => {
                output
                    .error(Value::Null, -32700, "ACP_INVALID_JSON")
                    .await?;
                continue;
            }
        };
        let id = frame.get("id").cloned();
        if frame["jsonrpc"] != "2.0"
            || !frame.is_object()
            || id
                .as_ref()
                .is_some_and(|id| !(id.is_string() || id.is_i64() || id.is_u64()))
        {
            output
                .error(Value::Null, -32600, "ACP_INVALID_REQUEST")
                .await?;
            continue;
        }
        // Permission responses are routed by generated IDs and validated by the actor.
        if frame.get("method").is_none() {
            if let Some(name) = frame["id"]
                .as_str()
                .and_then(|id| id.split_once('/').map(|(s, _)| s))
                && let Some(session) = sessions.get(name)
            {
                let _ = session.tx.try_send(Command::Permission(frame));
            }
            continue;
        }
        let method = frame["method"].as_str().unwrap_or("");
        let params = &frame["params"];
        if method == "session/cancel" && id.is_none() {
            if let Some(session) = params["sessionId"].as_str().and_then(|s| sessions.get(s)) {
                // Backpressure rather than dropping an exact cancellation.
                if let Some(cancel) = session.admission.cancellation() {
                    timeout(IO, session.tx.send(cancel))
                        .await
                        .map_err(|_| "ACP_SESSION_BLOCKED")?
                        .map_err(|_| "ACP_SESSION_CLOSED")?;
                }
            }
            continue;
        }
        let Some(id) = id else {
            continue;
        };
        if method == "initialize" {
            if initialized || params["protocolVersion"].as_u64().is_none() {
                output.error(id, -32602, "ACP_INVALID_INITIALIZE").await?;
            } else {
                initialized = true;
                output.reply(id, json!({"protocolVersion":1,"agentCapabilities":{},"agentInfo":{"name":"zeroclaw-acp-bridge","version":env!("CARGO_PKG_VERSION")},"authMethods":[]})).await?;
            }
            continue;
        }
        if !initialized {
            output.error(id, -32600, "ACP_NOT_INITIALIZED").await?;
            continue;
        }
        match method {
            "session/new" => {
                if !params["mcpServers"].as_array().is_some_and(Vec::is_empty)
                    || !params["cwd"].is_string()
                {
                    output.error(id, -32602, "ACP_RESTRICTED_PROFILE").await?;
                    continue;
                }
                if sessions.len() >= MAX_SESSIONS {
                    output.error(id, -32603, "ACP_SESSION_LIMIT").await?;
                    continue;
                }
                let name = format!(
                    "{}{}",
                    cfg.settings.session_prefix,
                    uuid::Uuid::new_v4().simple()
                );
                match connect(&cfg, &name).await {
                    Ok((socket, epoch)) => {
                        let (tx, rx) = mpsc::channel(8);
                        let (config, out, session) = (cfg.clone(), output.clone(), name.clone());
                        let admission = Arc::new(Admission::default());
                        let actor_admission = admission.clone();
                        let task = zeroclaw_spawn::spawn!(async move {
                            session_loop(config, out, session, socket, epoch, rx, actor_admission)
                                .await;
                        });
                        sessions.insert(
                            name.clone(),
                            Session {
                                tx,
                                task,
                                admission,
                            },
                        );
                        output.reply(id, json!({"sessionId":name})).await?;
                    }
                    Err(code) => output.error(id, -32603, code).await?,
                }
            }
            "session/prompt" => {
                let Some(session) = params["sessionId"].as_str().and_then(|s| sessions.get(s))
                else {
                    output.error(id, -32602, "ACP_UNKNOWN_SESSION").await?;
                    continue;
                };
                match prompt(params) {
                    Ok(text) => {
                        if let Err(code) = session.prompt(id.clone(), text) {
                            output.error(id, -32603, code).await?;
                        }
                    }
                    Err(code) => output.error(id, -32602, code).await?,
                }
            }
            _ => output.error(id, -32601, "ACP_METHOD_UNSUPPORTED").await?,
        }
    }
}

async fn session_loop(
    cfg: Arc<Config>,
    out: Output,
    name: String,
    mut socket: Socket,
    mut epoch: String,
    mut commands: mpsc::Receiver<Command>,
    admission: Arc<Admission>,
) {
    while let Some(command) = commands.recv().await {
        if let Command::Prompt(id, text, generation) = command {
            let mut turn = Turn::new(epoch.clone(), generation);
            let result = execute(
                &cfg,
                &out,
                &name,
                &mut socket,
                &mut turn,
                &text,
                &mut commands,
            )
            .await;
            match result {
                Ok(reason) => {
                    if out.reply(id, json!({"stopReason":reason})).await.is_err() {
                        return;
                    }
                }
                Err(code) => {
                    admission.poison();
                    let _ = out.error(id, -32603, code).await;
                    // Failed or unknown-effect sessions are poisoned. A fresh session
                    // is explicit; no later prompt silently resets this binding.
                    while let Some(command) = commands.recv().await {
                        if let Command::Prompt(id, _, _) = command {
                            let _ = out.error(id, -32603, "ACP_SESSION_INCOMPLETE").await;
                        }
                    }
                    return;
                }
            }
            admission.finish(generation);
            epoch = turn.epoch;
        }
    }
}

struct Permission {
    rpc: String,
    request: String,
    expires: Instant,
}
impl Permission {
    fn decision(&self, response: &Value, cancelled: bool, now: Instant) -> Option<&'static str> {
        if cancelled
            || now >= self.expires
            || response["id"] != self.rpc
            || response.get("error").is_some()
            || response["result"]["outcome"]["outcome"] != "selected"
        {
            return None;
        }
        match response["result"]["outcome"]["optionId"].as_str() {
            Some("allow_once") => Some("approve"),
            Some("reject_once") => Some("deny"),
            _ => None,
        }
    }
}

async fn execute(
    cfg: &Config,
    out: &Output,
    session: &str,
    socket: &mut Socket,
    turn: &mut Turn,
    text: &str,
    commands: &mut mpsc::Receiver<Command>,
) -> Result<&'static str> {
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut permissions: HashMap<String, Permission> = HashMap::new();
    let mut attempts = 0;
    // A failed write has unknown effects: every retry below is resume-only.
    let mut reconnect = send(socket, turn.control("message", json!({"content":text})))
        .await
        .is_err();
    let mut recovering = false;
    let mut buffered = Vec::new();
    loop {
        if Instant::now() >= deadline {
            return Err("ACP_TURN_TIMEOUT");
        }
        if reconnect {
            permissions.clear();
            attempts += 1;
            if attempts > 2 {
                return Err("GATEWAY_RECOVERY_EXHAUSTED");
            }
            tokio::time::sleep(Duration::from_millis(100 * attempts)).await;
            match connect(cfg, session).await {
                Ok((fresh, epoch)) => {
                    if epoch != turn.epoch {
                        return Err("GATEWAY_EPOCH_CHANGED");
                    }
                    *socket = fresh;
                    send(
                        socket,
                        turn.control("resume", json!({"after_seq":turn.seq})),
                    )
                    .await?;
                    recovering = true;
                    reconnect = false;
                }
                Err(_) => continue,
            }
        }
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return Err("ACP_TURN_TIMEOUT"),
            command = commands.recv() => match command {
                None => return Err("ACP_INPUT_CLOSED"),
                Some(Command::Prompt(id, _, _)) => out.error(id, -32603, "ACP_SESSION_BUSY").await?,
                Some(Command::Cancel(generation)) => {
                    if turn.cancel(generation) {
                        permissions.clear();
                        if turn.turn.is_some() { send(socket, turn.control("cancel", json!({"turn_id":turn.turn}))).await?; }
                    }
                }
                Some(Command::Permission(response)) => {
                    if let Some(permission) = response["id"].as_str().and_then(|id| permissions.remove(id))
                        && let Some(decision) = permission.decision(&response, turn.cancelled, Instant::now()) {
                        send(socket, json!({"type":"approval_response","request_id":permission.request,"decision":decision})).await?;
                    }
                }
            },
            frame = receive(socket) => {
                let frame = match frame { Ok(frame) => frame, Err("GATEWAY_DISCONNECTED" | "GATEWAY_WRITE_TIMEOUT") => { reconnect = true; continue; }, Err(e) => return Err(e) };
                let kind = frame["type"].as_str().unwrap_or("");
                if matches!(kind, "turn_ack" | "turn_event" | "turn_replay" | "turn_cancel_ack") && frame["request_id"] != turn.request { continue; }
                match kind {
                    "turn_ack" => {
                        if !matches!(frame["status"].as_str(), Some("accepted" | "duplicate")) { return Err("GATEWAY_TURN_REFUSED"); }
                        turn.bind(&frame)?;
                        if turn.cancelled { send(socket, turn.control("cancel", json!({"turn_id":turn.turn}))).await?; }
                    }
                    "turn_event" if recovering => {
                        if buffered.len() >= 128 || buffered.iter().map(|v: &Value| v.to_string().len()).sum::<usize>() + frame.to_string().len() > MAX_LINE { return Err("GATEWAY_REPLAY_OVERFLOW"); }
                        buffered.push(frame);
                    }
                    "turn_event" => if let Some(reason) = project(out, session, turn, &frame).await? { return Ok(reason); },
                    "turn_replay" => {
                        if !recovering || frame["gap"] != false { return Err("GATEWAY_REPLAY_GAP"); }
                        turn.bind(&frame)?;
                        let events = frame["events"].as_array().filter(|e| e.len() <= 129).ok_or("GATEWAY_PROTOCOL")?;
                        for event in events.iter().chain(buffered.iter()) {
                            if let Some(reason) = project(out, session, turn, event).await? { return Ok(reason); }
                        }
                        buffered.clear(); recovering = false;
                        if turn.cancelled { send(socket, turn.control("cancel", json!({"turn_id":turn.turn}))).await?; }
                    }
                    "approval_request" if !turn.cancelled && !recovering => {
                        let request = frame["request_id"].as_str().filter(|s| uuid::Uuid::parse_str(s).is_ok()).ok_or("GATEWAY_PROTOCOL")?;
                        let rpc = format!("{session}/{request}");
                        if permissions.contains_key(&rpc) { continue; }
                        if permissions.len() >= 16 { return Err("ACP_PERMISSION_LIMIT"); }
                        let expires = Instant::now() + Duration::from_secs(frame["timeout_secs"].as_u64().filter(|n| *n > 0 && *n <= 180).ok_or("GATEWAY_PROTOCOL")?);
                        let permission = Permission {rpc: rpc.clone(), request: request.into(), expires};
                        out.send(json!({"jsonrpc":"2.0","id":permission.rpc,"method":"session/request_permission","params":{"sessionId":session,"toolCall":{"toolCallId":format!("approval-{request}"),"title":frame["tool"],"kind":"other","status":"pending","rawInput":frame["arguments_summary"]},"options":[{"optionId":"allow_once","name":zeroclaw_runtime::i18n::get_required_cli_string("acp-bridge-allow-once"),"kind":"allow_once"},{"optionId":"reject_once","name":zeroclaw_runtime::i18n::get_required_cli_string("acp-bridge-reject-once"),"kind":"reject_once"}]}})).await?;
                        permissions.insert(rpc, permission);
                    }
                    "turn_cancel_ack" => { turn.bind(&frame)?; }
                    "error" | "turn_error" | "turn_gap" => return Err("GATEWAY_TURN_INCOMPLETE"),
                    _ => {}
                }
            }
        }
    }
}

async fn project(
    out: &Output,
    session: &str,
    turn: &mut Turn,
    frame: &Value,
) -> Result<Option<&'static str>> {
    let result = turn.event(frame)?;
    if let Some((reason, text)) = result {
        if let Some(text) = text {
            out.update(session, json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}})).await?;
        }
        return Ok(Some(reason));
    }
    Ok(None)
}

#[cfg(test)]
mod tests;

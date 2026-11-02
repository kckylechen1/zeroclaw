use super::*;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeroclaw_config::schema::{
    AliasedAgentConfig, CustomModelProviderConfig, GatewayBridgeConfig, GatewayDeviceBridgeConfig,
    ModelProviderConfig, RiskProfileConfig, RuntimeProfileConfig,
};

const DEVICE_TOKEN: &str = "synthetic-device-bridge-token";
const OWNER_TOKEN: &str = "synthetic-paired-owner-token";
const SESSION: &str = "device-session";
const LIMIT: Duration = Duration::from_secs(15);

/// Real transport client for adversarial frames that the typed client cannot express.
/// All I/O is bounded; credentials are synthetic and endpoints are loopback-only.
struct DeviceWire(tokio::net::TcpStream);

impl DeviceWire {
    async fn upgrade(address: std::net::SocketAddr, query: &str, token: &str) -> (u16, Self) {
        Self::upgrade_path(address, &format!("/ws/chat?{query}"), token).await
    }

    async fn upgrade_path(address: std::net::SocketAddr, path: &str, token: &str) -> (u16, Self) {
        let mut stream = tokio::time::timeout(LIMIT, tokio::net::TcpStream::connect(address))
            .await
            .unwrap()
            .unwrap();
        let protocol = if path.starts_with("/ws/bridge") {
            "zeroclaw.bridge.v2"
        } else {
            "zeroclaw.v1"
        };
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: {protocol}\r\nAuthorization: Bearer {token}\r\n\r\n"
        );
        tokio::time::timeout(LIMIT, stream.write_all(request.as_bytes()))
            .await
            .unwrap()
            .unwrap();
        let mut header = Vec::new();
        tokio::time::timeout(LIMIT, async {
            while !header.ends_with(b"\r\n\r\n") {
                assert!(header.len() < 16_384, "oversized upgrade response");
                header.push(stream.read_u8().await.unwrap());
            }
        })
        .await
        .unwrap();
        let status = std::str::from_utf8(&header)
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        (status, Self(stream))
    }

    async fn send_opcode(&mut self, opcode: u8, payload: &[u8]) {
        assert!(payload.len() < 65_536);
        let mut frame = vec![0x80 | opcode];
        if payload.len() < 126 {
            frame.push(0x80 | payload.len() as u8);
        } else {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        // RFC6455 masking matters to the real Axum/tungstenite server, not secrecy.
        let mask = [0x12, 0x34, 0x56, 0x78];
        frame.extend_from_slice(&mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(i, byte)| byte ^ mask[i % 4]),
        );
        tokio::time::timeout(LIMIT, self.0.write_all(&frame))
            .await
            .unwrap()
            .unwrap();
    }

    async fn send(&mut self, value: Value) {
        self.send_opcode(1, value.to_string().as_bytes()).await;
    }

    async fn next(&mut self) -> Option<Value> {
        self.next_with_limit(65_536).await
    }

    async fn next_with_limit(&mut self, max_bytes: u64) -> Option<Value> {
        tokio::time::timeout(LIMIT, async {
            loop {
                let first = match self.0.read_u8().await {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return None,
                    Err(error) => panic!("WebSocket read failed: {error}"),
                };
                let second = self.0.read_u8().await.unwrap();
                assert_eq!(first & 0x80, 0x80, "test gateway frames must not fragment");
                assert_eq!(second & 0x80, 0, "server must not mask frames");
                let len = match second & 0x7f {
                    126 => u64::from(self.0.read_u16().await.unwrap()),
                    127 => self.0.read_u64().await.unwrap(),
                    len => u64::from(len),
                };
                assert!(len <= max_bytes, "oversized test frame");
                let mut payload = vec![0; len as usize];
                self.0.read_exact(&mut payload).await.unwrap();
                match first & 0xf {
                    1 => return Some(serde_json::from_slice(&payload).unwrap()),
                    8 => return None,
                    9 => self.send_opcode(10, &payload).await,
                    10 => {}
                    other => panic!("unexpected WebSocket opcode {other}"),
                }
            }
        })
        .await
        .expect("bounded WebSocket response")
    }

    async fn until(&mut self, kind: &str) -> Value {
        for _ in 0..64 {
            let frame = self
                .next()
                .await
                .expect("gateway closed before expected frame");
            if frame["type"] == kind {
                return frame;
            }
            assert_ne!(frame["type"], "error", "unexpected gateway error: {frame}");
        }
        panic!("too many frames before {kind}")
    }

    async fn connected(&mut self, connect: Value) {
        assert_eq!(self.until("session_start").await["session_id"], SESSION);
        self.send(connect).await;
        self.until("connected").await;
    }

    async fn denied(&mut self) {
        // Immediate close is also a valid live revocation contract.
        if let Some(frame) = self.next().await {
            assert_eq!(frame["code"], "BRIDGE_OPERATION_DENIED", "{frame}");
        }
    }
}

/// A deterministic local OpenAI-compatible provider. The gateway still resolves
/// config and constructs its real Agent, tool registry, approval channel and
/// persisted session. No Conversation or provider trait object is injected.
async fn device_provider(
    State(seen): State<Arc<parking_lot::Mutex<Vec<Value>>>>,
    axum::Json(request): axum::Json<Value>,
) -> impl IntoResponse {
    seen.lock().push(request.clone());
    if let Some(response) = turn_control_tests::provider_response(&request).await {
        return response;
    }
    let messages = request["messages"].as_array().unwrap();
    let tool_needed = messages.last().is_some_and(|message| {
        message["role"] == "user" && message["content"].to_string().contains("approval-probe")
    });
    let mut message = json!({"role":"assistant", "content":"device response"});
    let finish = if tool_needed {
        message["content"] = Value::Null;
        message["tool_calls"] = json!([{
            "index":0,"id":"approval-file-call","type":"function",
            "function":{"name":"file_write","arguments":json!({"path":"approved.txt","content":"owner-approved"}).to_string()}
        }]);
        "tool_calls"
    } else {
        "stop"
    };
    if request["stream"] == true {
        (
            [("content-type", "text/event-stream")],
            format!("data: {}\n\ndata: [DONE]\n\n", json!({"id":"local-response","choices":[{"index":0,"delta":message,"finish_reason":finish}]})),
        ).into_response()
    } else {
        axum::Json(json!({"id":"local-response","choices":[{"index":0,"message":message,"finish_reason":finish}]})).into_response()
    }
}

struct DeviceFixture {
    _tmp: tempfile::TempDir,
    state: AppState,
    address: std::net::SocketAddr,
    seen: Arc<parking_lot::Mutex<Vec<Value>>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    workspace: PathBuf,
}

impl Drop for DeviceFixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl DeviceFixture {
    async fn new(operations: &[&str]) -> Self {
        Self::with_pairing(operations, true).await
    }

    async fn with_pairing(operations: &[&str], require_pairing: bool) -> Self {
        Self::with_address(operations, require_pairing, "127.0.0.1:0".parse().unwrap()).await
    }

    async fn with_address(operations: &[&str], require_pairing: bool, address: std::net::SocketAddr) -> Self {
        assert!(address.ip().is_loopback());
        let tmp = tempfile::tempdir().unwrap();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let provider_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider_uri = format!(
            "http://{}/v1/chat/completions",
            provider_listener.local_addr().unwrap()
        );
        let provider_app = axum::Router::new()
            .route("/v1/chat/completions", axum::routing::post(device_provider))
            .with_state(Arc::clone(&seen));
        let provider_task = ::zeroclaw_spawn::spawn!(async move {
            axum::serve(provider_listener, provider_app).await.unwrap();
        });
        let mut state = crate::tests::admin_paircode_state(&tmp, require_pairing, false);
        // A collision must not turn a restricted bridge into a paired Operator.
        state.pairing = Arc::new(zeroclaw_runtime::security::pairing::PairingGuard::new(
            require_pairing,
            &[OWNER_TOKEN.into(), DEVICE_TOKEN.into()],
        ));
        state.session_backend = Some(Arc::new(
            zeroclaw_infra::session_sqlite::SqliteSessionBackend::new(tmp.path()).unwrap(),
        ));
        let workspace = tmp.path().join("agent-workspace");
        std::fs::create_dir(&workspace).unwrap();
        {
            let mut config = state.config.write();
            config.providers.models.custom.insert(
                "test".into(),
                CustomModelProviderConfig {
                    base: ModelProviderConfig {
                        model: Some("local-device-test".into()),
                        uri: Some(provider_uri),
                        native_tools: Some(true),
                        timeout_secs: Some(5),
                        ..Default::default()
                    },
                },
            );
            config.risk_profiles.insert(
                "test".into(),
                RiskProfileConfig {
                    workspace_only: true,
                    allowed_tools: Some(vec!["file_write".into()]),
                    always_ask: vec!["file_write".into()],
                    auto_approve: Vec::new(),
                    ..Default::default()
                },
            );
            config.runtime_profiles.insert(
                "test".into(),
                RuntimeProfileConfig {
                    agentic: true,
                    max_tool_iterations: 4,
                    ..Default::default()
                },
            );
            for alias in ["device-agent", "other-agent"] {
                let mut agent = AliasedAgentConfig {
                    model_provider: "custom.test".into(),
                    risk_profile: "test".into(),
                    runtime_profile: "test".into(),
                    ..Default::default()
                };
                agent.workspace.path = Some(workspace.clone());
                agent.memory.backend = zeroclaw_config::multi_agent::MemoryBackendKind::None;
                config.agents.insert(alias.into(), agent);
            }
            config.gateway.bridges.insert(
                "device".into(),
                GatewayBridgeConfig {
                    token_hash: zeroclaw_config::tokens::hash_token(DEVICE_TOKEN),
                    sessions: vec![SESSION.into()],
                    session_prefix: None,
                    owner: None,
                    device: Some(GatewayDeviceBridgeConfig {
                        agent: "device-agent".into(),
                        operations: operations.iter().map(|op| (*op).into()).collect(),
                    }),
                },
            );
        }
        let app = axum::Router::new()
            .route("/ws/chat", axum::routing::get(handle_ws_chat))
            .route(
                "/api/sessions/{id}",
                axum::routing::delete(crate::api::handle_api_session_delete),
            )
            .route(
                "/ws/bridge",
                axum::routing::get(crate::ws_bridge::handle_ws_bridge),
            )
            .with_state(state.clone())
            .merge(crate::api_attachments::routes(state.clone(), 5));
        let listener = tokio::net::TcpListener::bind(address).await.unwrap();
        let address = listener.local_addr().unwrap();
        let gateway_task = ::zeroclaw_spawn::spawn!(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            _tmp: tmp,
            state,
            address,
            seen,
            tasks: vec![provider_task, gateway_task],
            workspace,
        }
    }

    async fn socket(&self, token: &str) -> DeviceWire {
        let (status, mut client) = DeviceWire::upgrade(
            self.address,
            &format!("agent=device-agent&session_id={SESSION}"),
            token,
        )
        .await;
        assert_eq!(status, 101);
        client.connected(json!({"type":"connect"})).await;
        client
    }

    async fn send_turn(&self, client: &mut DeviceWire, id: &str, content: &str) {
        client
            .send(json!({"type":"message", "id":id, "content":content}))
            .await;
        assert_eq!(client.until("ack").await["status"], "accepted");
        client.until("done").await;
    }
}

#[tokio::test]
async fn device_wire_binding_is_trusted_and_conflicts_fail_before_provider_use() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    for query in [
        "agent=other-agent&session_id=device-session",
        "agent=device-agent&session_id=wrong-session",
        "agent=device-agent",
        "agent=device-agent&session_id=device-session&cwd=%2Ftmp",
    ] {
        let (status, _) = DeviceWire::upgrade(fixture.address, query, DEVICE_TOKEN).await;
        assert_eq!(status, 403, "query {query}");
    }
    for extra in [
        json!({"session_id":"wrong-session"}),
        json!({"session_id":null}),
        json!({"agent":"other-agent"}),
        json!({"agent_alias":"other-agent"}),
        json!({"agentAlias":"other-agent"}),
        json!({"cwd":"/tmp"}),
        json!({"workspaceDir":"/tmp"}),
        json!({"session_id":7}),
    ] {
        let (status, mut client) = DeviceWire::upgrade(
            fixture.address,
            "agent=device-agent&session_id=device-session",
            DEVICE_TOKEN,
        )
        .await;
        assert_eq!(status, 101);
        client.until("session_start").await;
        let mut connect = json!({"type":"connect"});
        connect
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        client.send(connect).await;
        assert_eq!(
            client.until("error").await["code"],
            "BRIDGE_BINDING_REJECTED"
        );
    }
    assert!(fixture.seen.lock().is_empty());
    let (status, mut client) = DeviceWire::upgrade(
        fixture.address,
        "agent=device-agent&session_id=device-session",
        DEVICE_TOKEN,
    )
    .await;
    assert_eq!(status, 101);
    client
        .connected(json!({"type":"connect", "session_id":SESSION, "agent":"device-agent"}))
        .await;
    fixture
        .send_turn(&mut client, "allowed-chat", "hello device")
        .await;
    let seen = fixture.seen.lock();
    assert_eq!(seen.len(), 1, "one admitted turn reaches the real provider");
    assert_eq!(seen[0]["model"], "local-device-test");
    assert!(seen[0]["messages"].to_string().contains("hello device"));
}

#[tokio::test]
async fn device_wire_cannot_consume_owner_approval_but_owner_can_approve() {
    let fixture = DeviceFixture::new(&["chat", "answer", "cancel"]).await;
    let mut device = fixture.socket(DEVICE_TOKEN).await;
    let mut owner = fixture.socket(OWNER_TOKEN).await;
    device
        .send(json!({"type":"message", "id":"needs-approval", "content":"approval-probe"}))
        .await;
    let approval = device.until("approval_request").await;
    let owner_approval = owner.until("approval_request").await;
    assert_eq!(approval["request_id"], owner_approval["request_id"]);
    assert_eq!(approval["tool"], "file_write");
    for decision in ["approve", "always", "deny"] {
        device.send(json!({"type":"approval_response", "request_id":approval["request_id"], "decision":decision})).await;
        assert_eq!(
            device.until("error").await["code"],
            "BRIDGE_OPERATION_DENIED"
        );
        assert!(!fixture.workspace.join("approved.txt").exists());
    }
    // Successful owner completion proves the attack did not consume the pending oneshot.
    owner.send(json!({"type":"approval_response", "request_id":approval["request_id"], "decision":"approve"})).await;
    owner.until("done").await;
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("approved.txt")).unwrap(),
        "owner-approved"
    );
    assert_eq!(fixture.seen.lock().len(), 2);
}

#[tokio::test]
async fn device_wire_each_operation_is_closed_unless_granted() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    let mut device = fixture.socket(DEVICE_TOKEN).await;
    for frame in [
        json!({"type":"cancel"}),
        json!({"type":"answer", "request_id":"missing", "text":"yes"}),
        json!({"type":"source_resume", "source":"telegram:attacker"}),
        json!({"type":"source_disposition", "source":"telegram:attacker"}),
        json!({"type":"message", "id":"source-claim", "content":"no", "source":{}}),
        json!({"type":"message", "id":"attachment-claim", "content":"no", "attachments":["bogus"]}),
        json!({"type":"future-capability"}),
    ] {
        device.send(frame).await;
        assert_eq!(
            device.until("error").await["code"],
            "BRIDGE_OPERATION_DENIED"
        );
    }
    assert!(fixture.seen.lock().is_empty());
    fixture
        .send_turn(&mut device, "still-authorized", "chat remains allowed")
        .await;
    fixture
        .state
        .config
        .write()
        .gateway
        .bridges
        .get_mut("device")
        .unwrap()
        .device
        .as_mut()
        .unwrap()
        .operations
        .clear();
    device
        .send(json!({"type":"message", "id":"reduced", "content":"blocked"}))
        .await;
    device.denied().await;
    assert_eq!(fixture.seen.lock().len(), 1);
}

#[tokio::test]
async fn device_wire_revocation_and_policy_removal_cannot_elevate_open_socket() {
    for remove_entry in [true, false] {
        let fixture = DeviceFixture::new(&["chat"]).await;
        // Revoke this bridge credential alone; the collision case has its own test.
        assert!(fixture.state.pairing.revoke_token(DEVICE_TOKEN));
        let mut device = fixture.socket(DEVICE_TOKEN).await;
        fixture
            .send_turn(&mut device, "before-revoke", "allowed before change")
            .await;
        {
            let mut config = fixture.state.config.write();
            if remove_entry {
                config.gateway.bridges.remove("device");
            } else {
                config.gateway.bridges.get_mut("device").unwrap().device = None;
            }
        }
        device
            .send(json!({"type":"message", "id":"after-revoke", "content":"must never run"}))
            .await;
        device.denied().await;
        assert_eq!(fixture.seen.lock().len(), 1);
        if remove_entry {
            let (status, _) = DeviceWire::upgrade(
                fixture.address,
                "agent=device-agent&session_id=device-session",
                DEVICE_TOKEN,
            )
            .await;
            assert_eq!(status, 401, "revoked bridge cannot open a fresh socket");
        }
    }
}

#[tokio::test]
async fn device_wire_omitted_operations_deny_attachment() {
    let fixture = DeviceFixture::new(&[]).await;
    let (status, _) = DeviceWire::upgrade(
        fixture.address,
        "agent=device-agent&session_id=device-session",
        DEVICE_TOKEN,
    )
    .await;
    assert_eq!(status, 403);
    assert!(fixture.seen.lock().is_empty());
}

#[tokio::test]
async fn device_wire_input_never_becomes_operator_even_with_paired_token_collision() {
    use zeroclaw_api::review::UserMessageSource;
    let fixture = DeviceFixture::new(&["chat"]).await;
    let mut device = fixture.socket(DEVICE_TOKEN).await;
    fixture
        .send_turn(
            &mut device,
            "device-input",
            "device must not teach the operator model",
        )
        .await;
    let mut owner = fixture.socket(OWNER_TOKEN).await;
    fixture
        .send_turn(&mut owner, "owner-input", "owner authorized input")
        .await;
    let rows = fixture
        .state
        .session_backend
        .as_ref()
        .unwrap()
        .load_with_timestamps("gw_device-session");
    let device_rows: Vec<_> = rows
        .iter()
        .filter(|row| {
            row.message.role == "user" && row.message.content.contains("device must not teach")
        })
        .collect();
    assert_eq!(
        device_rows.len(),
        1,
        "the real device turn must have persisted"
    );
    assert!(
        device_rows[0].ingress.is_none(),
        "device text is not Operator learning evidence"
    );
    let owner_rows: Vec<_> = rows
        .iter()
        .filter(|row| {
            row.ingress
                .as_ref()
                .is_some_and(|ingress| ingress.source == UserMessageSource::Operator)
        })
        .collect();
    assert_eq!(owner_rows.len(), 1, "owner provenance remains available");
    assert_eq!(
        owner_rows[0].ingress.as_ref().unwrap().text,
        "owner authorized input"
    );
}

#[tokio::test]
async fn device_wire_cannot_rebind_an_existing_sessions_agent() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    let backend = fixture.state.session_backend.as_ref().unwrap();
    backend
        .set_session_agent_alias("gw_device-session", "other-agent")
        .unwrap();
    let (status, mut device) = DeviceWire::upgrade(
        fixture.address,
        "agent=device-agent&session_id=device-session",
        DEVICE_TOKEN,
    )
    .await;
    assert_eq!(status, 101);
    // Refuse before session_start, history disclosure or Agent construction.
    let frame = device.next().await.unwrap();
    assert_eq!(frame["type"], "error");
    assert_eq!(frame["code"], "BRIDGE_BINDING_REJECTED");
    assert_eq!(
        backend
            .get_session_agent_alias("gw_device-session")
            .unwrap()
            .as_deref(),
        Some("other-agent")
    );
    assert!(fixture.seen.lock().is_empty());
}

#[tokio::test]
async fn device_wire_empty_and_noncanonical_scoped_sessions_are_rejected() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    {
        let mut config = fixture.state.config.write();
        let bridge = config.gateway.bridges.get_mut("device").unwrap();
        bridge.sessions = vec![String::new()];
        bridge.session_prefix = Some("device/".into());
        assert!(bridge.allows_session(""));
        assert!(bridge.allows_session("device/room"));
    }
    for query in [
        "agent=device-agent&session_id=",
        "agent=device-agent&session_id=device%2Froom",
    ] {
        let (status, _) = DeviceWire::upgrade(fixture.address, query, DEVICE_TOKEN).await;
        assert_eq!(status, 403, "scoped but noncanonical session: {query}");
    }
    assert!(fixture.seen.lock().is_empty());
}

/// Real HTTP caller for the auxiliary attachment boundary; not router.oneshot.
async fn device_http(
    address: std::net::SocketAddr,
    method: &str,
    path: &str,
    token: &str,
    body: &str,
) -> (u16, Vec<u8>) {
    tokio::time::timeout(LIMIT, async {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {token}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len(),
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.take(65_537).read_to_end(&mut response).await.unwrap();
        assert!(response.len() <= 65_536, "oversized HTTP response");
        let boundary = response.windows(4).position(|bytes| bytes == b"\r\n\r\n").unwrap();
        let status = std::str::from_utf8(&response[..boundary]).unwrap()
            .split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, response[boundary + 4..].to_vec())
    }).await.expect("bounded HTTP response")
}

#[tokio::test]
async fn device_wire_cannot_open_outbox_or_upload_or_fetch_attachments() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    let (status, _) = DeviceWire::upgrade_path(fixture.address, "/ws/bridge", DEVICE_TOKEN).await;
    assert_eq!(
        status, 401,
        "device policy never grants the outbox transport"
    );
    let upload = "/api/attachments?agent=device-agent&session_id=device-session&file_name=test.txt";
    assert_eq!(
        device_http(fixture.address, "POST", upload, DEVICE_TOKEN, "device data")
            .await
            .0,
        403
    );
    // The same route remains usable by a paired owner, including a real stored handle.
    let (status, body) =
        device_http(fixture.address, "POST", upload, OWNER_TOKEN, "owner data").await;
    assert_eq!(status, 201);
    let attachment: Value = serde_json::from_slice(&body).unwrap();
    let fetch = format!(
        "/api/attachments/{}?agent=device-agent&session_id=device-session",
        attachment["id"].as_str().unwrap()
    );
    assert_eq!(
        device_http(fixture.address, "GET", &fetch, DEVICE_TOKEN, "")
            .await
            .0,
        403
    );
    let (status, body) = device_http(fixture.address, "GET", &fetch, OWNER_TOKEN, "").await;
    assert_eq!(status, 200);
    assert_eq!(body, b"owner data");
    assert!(fixture.seen.lock().is_empty());
}

#[tokio::test]
async fn device_wire_rechecks_revocation_after_waiting_for_session_queue() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    let mut device = fixture.socket(DEVICE_TOKEN).await;
    let guard = fixture
        .state
        .session_queue
        .acquire("gw_device-session")
        .await
        .unwrap();
    device.send(json!({"type":"message", "id":"queued-before-revoke", "content":"must not reach provider"})).await;
    assert_eq!(device.until("ack").await["status"], "accepted");
    // Observe the actual run waiting behind the held permit, not a timing guess.
    tokio::time::timeout(LIMIT, async {
        while fixture
            .state
            .session_queue
            .queue_depth("gw_device-session")
            .await
            != 2
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(fixture.seen.lock().is_empty());
    fixture
        .state
        .config
        .write()
        .gateway
        .bridges
        .remove("device");
    drop(guard);
    // The socket's output guard may close before forwarding the turn's denial.
    // The persisted terminal receipt proves the queued task itself was refused.
    device.denied().await;
    tokio::time::timeout(LIMIT, async {
        loop {
            let receipt = fixture
                .state
                .session_backend
                .as_ref()
                .unwrap()
                .record_request("gw_device-session", "queued-before-revoke", "accepted")
                .unwrap()
                .expect("SQLite must retain the accepted request");
            let RequestReceipt::Duplicate { state } = receipt else {
                panic!("polling must observe an existing receipt, never insert one");
            };
            if state == "error" {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("revoked queued request must reach a terminal error");
    assert!(
        fixture.seen.lock().is_empty(),
        "revoked queued work must never reach Agent execution"
    );
}

#[tokio::test]
async fn device_wire_policy_cannot_be_bypassed_through_anonymous_pairing_mode() {
    let fixture = DeviceFixture::with_pairing(&["chat"], false).await;
    for token in ["", DEVICE_TOKEN] {
        let (status, mut socket) = DeviceWire::upgrade(
            fixture.address,
            "agent=device-agent&session_id=device-session",
            token,
        )
        .await;
        assert_eq!(
            status, 503,
            "device policy needs an authenticated gateway even for a valid token"
        );
        // Upgrade refusal is an ordinary JSON HTTP body, not a WebSocket frame.
        let refusal = tokio::time::timeout(LIMIT, async {
            let mut body = Vec::new();
            loop {
                assert!(body.len() < 16_384, "oversized refusal body");
                body.push(socket.0.read_u8().await.unwrap());
                if let Ok(value) = serde_json::from_slice::<Value>(&body) {
                    break value;
                }
            }
        })
        .await
        .expect("bounded upgrade refusal body");
        assert_eq!(refusal["code"], "BRIDGE_BINDING_REJECTED");
    }
    assert!(fixture.seen.lock().is_empty());
    // Explicitly anonymous legacy installations still work with no device policy.
    fixture.state.config.write().gateway.bridges.clear();
    let mut anonymous = fixture.socket("").await;
    fixture
        .send_turn(&mut anonymous, "legacy-anonymous", "legacy anonymous chat")
        .await;
    assert_eq!(fixture.seen.lock().len(), 1);
}

#[tokio::test]
async fn device_wire_live_policy_activation_closes_an_existing_anonymous_socket() {
    let fixture = DeviceFixture::with_pairing(&["chat"], false).await;
    let policy = fixture
        .state
        .config
        .write()
        .gateway
        .bridges
        .remove("device")
        .unwrap();
    let mut anonymous = fixture.socket("").await;
    assert!(fixture.seen.lock().is_empty());
    fixture
        .state
        .config
        .write()
        .gateway
        .bridges
        .insert("device".into(), policy);
    anonymous.send(json!({"type":"message", "id":"anonymous-after-policy", "content":"must not bypass device policy"})).await;
    anonymous.denied().await;
    assert!(
        fixture.seen.lock().is_empty(),
        "hot activation must not preserve anonymous operation authority"
    );
}

mod turn_control_tests {
    include!("turn_control_tests.rs");
}

fn owner_bridge(fixture: &DeviceFixture, operations: &[&str]) {
    let mut config = fixture.state.config.write();
    let bridge = config.gateway.bridges.get_mut("device").unwrap();
    bridge.device = None;
    bridge.owner = Some(zeroclaw_config::schema::GatewayOwnerBridgeConfig {
        agent: "device-agent".into(),
        operations: operations.iter().map(|s| (*s).into()).collect(),
    });
}

#[tokio::test]
async fn owner_bridge_once_approval_is_explicit_and_never_always() {
    for decision in ["approve", "deny"] {
        let fixture = DeviceFixture::new(&["chat", "cancel"]).await;
        owner_bridge(&fixture, &["chat", "cancel", "approval_response"]);
        let (status, mut client) = DeviceWire::upgrade(fixture.address, &format!("agent=device-agent&session_id={SESSION}"), DEVICE_TOKEN).await;
        assert_eq!(status, 101);
        client.until("session_start").await;
        client.send(json!({"type":"connect"})).await;
        assert_eq!(client.until("connected").await["owner_bridge_approval_once"], true);
        client.send(json!({"type":"turn_control","version":1,"op":"negotiate"})).await;
        let epoch = client.until("turn_ready").await["epoch"].clone();
        let id = uuid::Uuid::new_v4().to_string();
        client.send(json!({"type":"turn_control","version":1,"epoch":epoch,"op":"message","request_id":id,"content":"approval-probe"})).await;
        let approval = client.until("approval_request").await;
        client.send(json!({"type":"approval_response","request_id":approval["request_id"],"decision":"always"})).await;
        assert_eq!(client.until("error").await["code"], "BRIDGE_OPERATION_DENIED");
        assert!(!fixture.workspace.join("approved.txt").exists());
        client.send(json!({"type":"approval_response","request_id":approval["request_id"],"decision":decision})).await;
        loop { let event = client.until("turn_event").await; if event["terminal"] == true { assert_eq!(event["event"]["outcome"], "done"); break; } }
        assert_eq!(fixture.workspace.join("approved.txt").exists(), decision == "approve");
    }
}

#[tokio::test]
async fn owner_bridge_missing_grant_and_class_switch_do_not_elevate() {
    let fixture = DeviceFixture::new(&["chat", "cancel"]).await;
    let mut device = fixture.socket(DEVICE_TOKEN).await;
    owner_bridge(&fixture, &["chat", "cancel", "approval_response"]);
    device.send(json!({"type":"approval_response","request_id":"irrelevant","decision":"approve"})).await;
    device.denied().await;
    let mut owner = fixture.socket(DEVICE_TOKEN).await;
    fixture.state.config.write().gateway.bridges.get_mut("device").unwrap().owner.as_mut().unwrap().operations = vec!["chat".into()];
    owner.send(json!({"type":"approval_response","request_id":"irrelevant","decision":"deny"})).await;
    owner.denied().await;
    let mut missing = fixture.socket(DEVICE_TOKEN).await;
    missing.send(json!({"type":"approval_response","request_id":"irrelevant","decision":"approve"})).await;
    missing.denied().await;
}

#[tokio::test]
async fn owner_bridge_keeps_agent_workspace_and_auxiliary_surfaces_closed() {
    let fixture = DeviceFixture::new(&["chat", "cancel"]).await;
    owner_bridge(&fixture, &["chat", "cancel", "approval_response"]);
    for query in ["agent=other-agent&session_id=device-session", "agent=device-agent&session_id=wrong", "agent=device-agent&session_id=device-session&cwd=%2Ftmp"] {
        assert_eq!(DeviceWire::upgrade(fixture.address, query, DEVICE_TOKEN).await.0, 403);
    }
    assert_eq!(DeviceWire::upgrade_path(fixture.address, "/ws/bridge", DEVICE_TOKEN).await.0, 401);
    let mut client = fixture.socket(DEVICE_TOKEN).await;
    fixture.state.config.write().gateway.bridges.get_mut("device").unwrap().owner = None;
    client.send(json!({"type":"approval_response","request_id":"irrelevant","decision":"approve"})).await;
    client.denied().await;
}

/// Opt-in real gateway fixture for desktop ACP acceptance. Uses temporary data,
/// the production Conversation/approval path and a deterministic HTTP provider.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "manual desktop acceptance; requires ZEROCLAW_ACP_ACCEPTANCE_DIR"]
async fn acp_desktop_gateway_fixture() {
    use std::os::unix::fs::PermissionsExt;
    let directory = PathBuf::from(std::env::var_os("ZEROCLAW_ACP_ACCEPTANCE_DIR").expect("explicit output directory"));
    assert!(directory.is_absolute() && directory.is_dir());
    let address = std::fs::read_to_string(directory.join("bind-address")).unwrap_or_else(|_| "127.0.0.1:0".into()).trim().parse().unwrap();
    let fixture = DeviceFixture::with_address(&["chat", "cancel"], true, address).await;
    owner_bridge(&fixture, &["chat", "cancel", "approval_response"]);
    let token = std::fs::read_to_string(directory.join("credential")).unwrap_or_else(|_| format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple()));
    {
        let mut config = fixture.state.config.write();
        let bridge = config.gateway.bridges.get_mut("device").unwrap();
        bridge.token_hash = zeroclaw_config::tokens::hash_token(&token);
        bridge.session_prefix = Some("acp-".into());
    }
    let credential = directory.join("credential");
    std::fs::write(&credential, token).unwrap();
    std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o600)).unwrap();
    let config = format!("[acp_bridge]\nenabled = true\nendpoint = \"ws://{}/ws/chat\"\nagent = \"device-agent\"\nsession_prefix = \"acp-\"\ncredential_file = {:?}\n", fixture.address, credential);
    std::fs::write(directory.join("adapter.toml"), config).unwrap();
    std::fs::write(directory.join("ready.json"), json!({"address":fixture.address.to_string(),"workspace":fixture.workspace,"pid":std::process::id()}).to_string()).unwrap();
    for _ in 0..1800 {
        if directory.join("stop").exists() { break; }
        std::fs::write(directory.join("provider-count.json"), fixture.seen.lock().len().to_string()).unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    std::fs::write(directory.join("provider-requests.json"), serde_json::to_vec_pretty(&*fixture.seen.lock()).unwrap()).unwrap();
}

#[tokio::test]
async fn owner_bridge_handshake_reports_cancel_only_when_granted() {
    for granted in [false, true] {
        let fixture = DeviceFixture::new(&["chat"]).await;
        let operations = if granted { vec!["chat", "approval_response", "cancel"] } else { vec!["chat", "approval_response"] };
        owner_bridge(&fixture, &operations);
        let (status, mut client) = DeviceWire::upgrade(fixture.address, &format!("agent=device-agent&session_id={SESSION}"), DEVICE_TOKEN).await;
        assert_eq!(status, 101);
        client.until("session_start").await;
        client.send(json!({"type":"connect"})).await;
        let ack = client.until("connected").await;
        assert_eq!(ack["owner_bridge_approval_once"], true);
        assert_eq!(ack["owner_bridge_cancel"], granted);
    }
}


/// Paired clients must retain the configured agent workspace boundary.
#[tokio::test]
async fn paired_session_cwd_rejects_outside_workspace() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    let (status, mut client) = DeviceWire::upgrade(
        fixture.address,
        "agent=device-agent&session_id=device-session&cwd=%2F",
        OWNER_TOKEN,
    )
    .await;
    assert_eq!(status, 101);
    client.connected(json!({"type":"connect"})).await;
    client.send(json!({"type":"message", "id":"cwd-check", "content":"hello"})).await;
    let frame = client.next().await.unwrap();
    assert_eq!(frame["code"], "INVALID_CWD", "{frame}");
    assert!(fixture.seen.lock().is_empty());
}

#[tokio::test]
async fn paired_session_cwd_accepts_workspace_descendant() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    let workspace = fixture.state.config.read().agent_workspace_dir("device-agent");
    let child = workspace.join("project");
    std::fs::create_dir(&child).unwrap();
    let encoded_cwd: String = child.to_str().unwrap().bytes().map(|byte| format!("%{byte:02X}")).collect();
    let query = format!("agent=device-agent&session_id={SESSION}&cwd={encoded_cwd}");
    let (status, mut client) = DeviceWire::upgrade(fixture.address, &query, OWNER_TOKEN).await;
    assert_eq!(status, 101);
    client.connected(json!({"type":"connect"})).await;
    fixture.send_turn(&mut client, "cwd-child", "hello").await;
}

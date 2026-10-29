#![cfg(all(feature = "gateway", feature = "nodes", feature = "agent-runtime"))]
#[path = "../src/node_host.rs"]
mod node_host;
use axum::{
    Router,
    extract::{WebSocketUpgrade, ws::Message as AxumMessage},
    http::HeaderMap,
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{net::SocketAddr, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin},
    sync::mpsc,
};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
use zeroclaw_api::{node::WS_NODES_V2, tool::Tool};
use zeroclaw_gateway::{
    device_identity::DeviceIdentityStore,
    nodes::{NodeRegistry, upgrade_node_socket},
};
use zeroclaw_runtime::security::pairing::PairingGuard;

#[tokio::test]
#[ignore = "spawned as a separate host process by the parent proof"]
async fn host_process() {
    node_host::run().await.unwrap();
}

struct Host {
    child: Child,
    input: ChildStdin,
    events: mpsc::Receiver<Value>,
    device: String,
    init: Value,
}
impl Host {
    async fn connect(&mut self) {
        self.command(&self.init.to_string()).await;
        self.event("ready").await;
    }
    async fn event(&mut self, name: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = self.events.recv().await.expect("host exited before event");
                if event["event"] == name {
                    return event;
                }
            }
        })
        .await
        .expect("host event deadline")
    }
    async fn command(&mut self, value: &str) {
        self.input
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn exit(&mut self) {
        assert!(
            tokio::time::timeout(Duration::from_secs(5), self.child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
}
struct Fixture {
    dir: tempfile::TempDir,
    registry: Arc<NodeRegistry>,
    address: SocketAddr,
    token: String,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.registry.shutdown();
        for task in &self.tasks {
            task.abort();
        }
    }
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = DeviceIdentityStore::open(dir.path(), 16).unwrap();
        let registry = Arc::new(NodeRegistry::new(16).with_identities(store));
        zeroclaw_api::node_tools::install(dir.path(), registry.clone());
        let token = uuid::Uuid::new_v4().to_string();
        let config = zeroclaw_config::schema::NodesConfig {
            enabled: true,
            auth_token: Some(token.clone()),
            ..Default::default()
        };
        let gateway = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway_address = gateway.local_addr().unwrap();
        let owner = registry.clone();
        let router = Router::new().route(
            "/ws/nodes",
            get(move |headers: HeaderMap, ws: WebSocketUpgrade| {
                let registry = owner.clone();
                let config = config.clone();
                async move {
                    upgrade_node_socket(
                        &config,
                        &PairingGuard::new(false, &[]),
                        registry,
                        &headers,
                        None,
                        ws,
                    )
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(gateway, router).await.unwrap();
        });
        // Fault-injection proxy duplicates real Invoke frames and injects stale
        // results. Neither endpoint is mocked; all auth and execution stays real.
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = proxy.local_addr().unwrap();
        let router = Router::new().route("/ws/nodes", get(move |headers: HeaderMap, ws: WebSocketUpgrade| {
            let auth = headers.get("Authorization").cloned().unwrap();
            async move { ws.protocols([WS_NODES_V2]).on_upgrade(move |mut client| async move {
                let mut request = format!("ws://{gateway_address}/ws/nodes").into_client_request().unwrap();
                request.headers_mut().insert("Sec-WebSocket-Protocol", WS_NODES_V2.parse().unwrap());
                request.headers_mut().insert("Authorization", auth);
                let (mut upstream, _) = tokio_tungstenite::connect_async(request).await.unwrap();
                loop {
                    tokio::select! {
                        message = client.recv() => {
                            match message {
                                Some(Ok(AxumMessage::Text(text))) => {
                                    if upstream.send(Message::Text(text.to_string().into())).await.is_err() { break }
                                }
                                _ => break,
                            }
                        }
                        message = upstream.next() => {
                            match message {
                                Some(Ok(Message::Text(text))) => {
                                    let value: Value = serde_json::from_str(&text).unwrap();
                                    if value["type"] == "invoke" {
                                        let stale = json!({"type":"result", "call_id":value["call_id"], "connection_id":"old-connection", "success":true, "output":"STALE"});
                                        if upstream.send(Message::Text(stale.to_string().into())).await.is_err() { break }
                                        if client.send(AxumMessage::Text(text.to_string().into())).await.is_err() { break }
                                    }
                                    if client.send(AxumMessage::Text(text.to_string().into())).await.is_err() { break }
                                }
                                _ => break,
                            }
                        }
                    }
                }
                let _ = client.close().await;
                let _ = upstream.close(None).await;
            }) }
        }));
        let proxy_task = tokio::spawn(async move {
            axum::serve(proxy, router).await.unwrap();
        });
        Self {
            dir,
            registry,
            address,
            token,
            tasks: vec![server, proxy_task],
        }
    }
    async fn host(&self) -> Host {
        let mut host = self.pending_host().await;
        host.connect().await;
        host
    }
    async fn pending_host(&self) -> Host {
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "host_process", "--nocapture"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, events) = mpsc::channel(128);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(event) = serde_json::from_str::<Value>(&line) {
                    if tx.send(event).await.is_err() {
                        break;
                    }
                }
            }
        });
        let mut host = Host {
            child,
            input,
            events,
            device: String::new(),
            init: Value::Null,
        };
        let key = host.event("public_key").await;
        let store = self.registry.identities().unwrap();
        let code = store
            .issue_pairing_code(vec!["system.info".into(), "test.echo".into()])
            .unwrap();
        let identity = store
            .enroll(&code, key["public_key"].as_str().unwrap())
            .unwrap();
        host.device = identity.device_id.clone();
        host.init = json!({"address":self.address, "device_id":identity.device_id, "identity_epoch":identity.identity_epoch, "token":self.token});
        host
    }
    async fn tool(&self, host: &Host, cap: &str) -> Box<dyn Tool> {
        use zeroclaw_runtime::tools::{
            AllToolsResult,
            scoped::{ScopedAssembly, ScopedToolRegistry},
        };
        let name = format!("node:{}:{cap}", host.device);
        let mut config = zeroclaw_config::schema::Config::default();
        config.data_dir = self.dir.path().to_path_buf();
        config.nodes.enabled = true;
        let security = Arc::new(zeroclaw_policy::SecurityPolicy {
            allowed_tools: Some(vec![name.clone()]),
            ..Default::default()
        });
        let built = AllToolsResult {
            tools: vec![],
            ask_user_handle: None,
            reaction_handle: Default::default(),
            poll_handle: None,
            escalate_handle: None,
            channel_room_handle: None,
            unfiltered_tool_arcs: vec![],
        };
        let assembled = ScopedToolRegistry::assemble(ScopedAssembly {
            config: &config,
            agent_alias: "default",
            security: &security,
            built,
            skills: &[],
            runtime: Arc::new(zeroclaw_runtime::platform::NativeRuntime::new()),
            caller_allowed: None,
            connect_mcp: false,
            exclude_memory: false,
            list_deferred_mcp_specs: false,
            emit_assembly_logs: false,
            mcp_registry: None,
        })
        .await;
        assembled
            .registry
            .into_inner()
            .into_iter()
            .find(|tool| tool.name() == name)
            .expect("admitted real scoped tool")
    }
    fn negative(&self, host: &Host) {
        assert!(zeroclaw_api::node_tools::tools(self.dir.path(), None).is_empty());
        for cap in ["face", "drive", "system.run", "camera.snap", "*"] {
            let name = format!("node:{}:{cap}", host.device);
            assert!(zeroclaw_api::node_tools::tools(self.dir.path(), Some(&[name])).is_empty());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seven_real_process_scenarios() {
    let fixture = Fixture::new().await;
    let mut host = fixture.pending_host().await;
    fixture.negative(&host);
    // Exactly one scoped assembly per tool, BEFORE the host connects. These same
    // objects remain in a long-lived Agent across all first-host lifecycle cases.
    let echo: Arc<dyn Tool> = fixture.tool(&host, "test.echo").await.into();
    let info: Arc<dyn Tool> = fixture.tool(&host, "system.info").await.into();
    assert_eq!(
        echo.execute(json!({"text":"offline"}))
            .await
            .unwrap()
            .error
            .as_deref(),
        Some("node_unavailable")
    );
    host.connect().await;
    for args in [
        json!({}),
        json!({"text":null}),
        json!({"text":0}),
        json!({"text":"x","approved":true}),
        json!({"text":"x".repeat(4097)}),
    ] {
        assert!(!echo.execute(args).await.unwrap().success);
    }
    let result = echo.execute(json!({"text":""})).await.unwrap();
    assert!(result.success);
    assert_eq!(result.output, "");
    assert_eq!(host.event("executed").await["count"], 1);
    let result = info.execute(json!({})).await.unwrap();
    assert!(result.success);
    assert!(result.output.contains(std::env::consts::OS));
    assert_eq!(host.event("executed").await["count"], 2);
    assert_eq!(fixture.registry.pending_invocations(), 0);
    println!(
        "PASS immediate: same scoped tools offline -> online, real child/v2 auth, empty echo/info, pending drain, invalid args"
    );
    println!(
        "PASS duplicate: proxy repeats each Invoke, child execution counts 1/2; duplicate Results ignored"
    );
    println!(
        "PASS high-risk: exact allowlist only; face/drive/shell/camera/wildcards absent; approved=true rejected"
    );

    host.command("hold").await;
    host.event("holding").await;
    let tool = echo.clone();
    let old = tokio::spawn(async move { tool.execute(json!({"text":"old"})).await.unwrap() });
    host.event("executed").await;
    host.command("reconnect").await;
    host.event("ready").await;
    let old = old.await.unwrap();
    assert!(!old.success);
    assert!(matches!(
        old.error.as_deref(),
        Some("disconnected_unknown_outcome" | "stale_connection_unknown_outcome")
    ));
    host.command("release").await;
    host.event("released").await;
    let current = echo.execute(json!({"text":"new"})).await.unwrap();
    assert!(current.success);
    assert_eq!(current.output, "new");
    assert_eq!(host.event("executed").await["count"], 1);
    println!(
        "PASS old connection: old pending drained, same tool reconnects without reassembly, injected stale Result cannot settle"
    );

    host.command("hold").await;
    host.event("holding").await;
    let tool = echo.clone();
    let pending =
        tokio::spawn(async move { tool.execute(json!({"text":"reduce"})).await.unwrap() });
    host.event("executed").await;
    host.command("info-only").await;
    host.event("ready").await;
    assert_eq!(
        pending.await.unwrap().error.as_deref(),
        Some("capability_changed")
    );
    assert_eq!(
        echo.execute(json!({"text":"denied"}))
            .await
            .unwrap()
            .error
            .as_deref(),
        Some("capability_unavailable")
    );
    assert_eq!(fixture.registry.pending_invocations(), 0);
    host.command("release").await;
    host.event("released").await;
    assert!(info.execute(json!({})).await.unwrap().success);
    host.event("executed").await;
    host.command("reconnect").await;
    host.event("ready").await;
    assert_eq!(
        echo.execute(json!({"text":"still-denied"}))
            .await
            .unwrap()
            .error
            .as_deref(),
        Some("capability_unavailable")
    );
    assert!(info.execute(json!({})).await.unwrap().success);
    assert_eq!(host.event("executed").await["count"], 1);
    println!(
        "PASS reduction: pending invalidated, same info tool survives revision; local echo reduction survives reconnect"
    );

    host.command("hold").await;
    host.event("holding").await;
    let tool = info.clone();
    let pending = tokio::spawn(async move { tool.execute(json!({})).await.unwrap() });
    host.event("executed").await;
    fixture.registry.revoke_device(&host.device).unwrap();
    assert_eq!(
        pending.await.unwrap().error.as_deref(),
        Some("revoked_unknown_outcome")
    );
    assert_eq!(
        info.execute(json!({})).await.unwrap().error.as_deref(),
        Some("node_unavailable")
    );
    assert_eq!(fixture.registry.pending_invocations(), 0);
    fixture.negative(&host);
    host.exit().await;
    println!(
        "PASS revoke: mid-flight result refused, retained tool cannot route, child exits, pending zero"
    );

    let mut host = fixture.host().await;
    fixture.negative(&host);
    host.command("hold").await;
    host.event("holding").await;
    let before = std::time::Instant::now();
    let result = fixture
        .tool(&host, "test.echo")
        .await
        .execute(json!({"text":"timeout"}))
        .await
        .unwrap();
    assert_eq!(result.error.as_deref(), Some("timeout_unknown_outcome"));
    assert!(before.elapsed() < Duration::from_secs(4));
    assert_eq!(fixture.registry.pending_invocations(), 0);
    host.event("executed").await;
    host.command("release").await;
    host.event("released").await;
    let after_late = fixture
        .tool(&host, "test.echo")
        .await
        .execute(json!({"text":"after-late"}))
        .await
        .unwrap();
    assert!(after_late.success);
    assert_eq!(after_late.output, "after-late");
    assert_eq!(host.event("executed").await["count"], 2);
    assert_eq!(fixture.registry.pending_invocations(), 0);
    println!(
        "PASS timeout: stalled real host, bounded unknown outcome, late result ignored, no pending or replay"
    );
    host.command("hold").await;
    host.event("holding").await;

    let info = fixture.tool(&host, "system.info").await;
    let pending = tokio::spawn(async move { info.execute(json!({})).await.unwrap() });
    host.event("executed").await;
    fixture.registry.shutdown();
    assert_eq!(
        pending.await.unwrap().error.as_deref(),
        Some("shutdown_unknown_outcome")
    );
    assert!(fixture.registry.try_reserve().is_none());
    assert_eq!(fixture.registry.pending_invocations(), 0);
    host.exit().await;
    println!("PASS shutdown: pending drained, admission closed, real host exited");
}

//! The actual router, SQLite claims and MCP transport, with an owned loopback
//! Tachi peer. No model, production gateway, harness process or robot is used.

use super::*;
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
};
use tower::ServiceExt;
use zeroclaw_config::{
    autonomy::AutonomyLevel,
    bridge_access::GatewayDeviceBridgeConfig,
    schema::{AliasedAgentConfig, RiskProfileConfig, RuntimeProfileConfig},
};
use zeroclaw_infra::session_sqlite::SqliteSessionBackend;

const TOKEN: &str = "zcb_synthetic_delegation_token";
const AGENT: &str = "edge_tasks";

#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<Value>>,
    malformed_start: AtomicBool,
    hold_start: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    cancel_mode: AtomicU8,
}

impl Fake {
    fn count(&self, action: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call["action"] == action)
            .count()
    }
}

async fn mcp(
    State(fake): State<Arc<Fake>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let id = body["id"].clone();
    match body["method"].as_str() {
        Some("initialize") => Json(json!({"jsonrpc":"2.0","id":id,"result":{
            "protocolVersion":"2025-06-18","capabilities":{"tools":{}},
            "serverInfo":{"name":"owned-test-tachi","version":"0"}
        }}))
        .into_response(),
        Some("notifications/initialized") => StatusCode::ACCEPTED.into_response(),
        Some("tools/call") => {
            let tool = body["params"]["name"].as_str().unwrap();
            if tool != "tachi_staff" {
                return Json(json!({"jsonrpc":"2.0","id":id,"result":{
                    "isError":true,"content":[{"type":"text","text":"synthetic mirror unavailable"}]
                }}))
                .into_response();
            }
            assert_eq!(headers["x-tachi-agent-identity"], "zeroclaw:edge_tasks");
            let args = body["params"]["arguments"].clone();
            fake.calls.lock().unwrap().push(args.clone());
            let result = match args["action"].as_str().unwrap() {
                "start" => {
                    assert_eq!(args["profile"], "owner-admitted-profile");
                    assert!(args.get("command").is_none());
                    assert!(args.get("cwd").is_none());
                    if fake.hold_start.load(Ordering::SeqCst) {
                        fake.entered.notify_one();
                        fake.release.notified().await;
                    }
                    if fake.malformed_start.load(Ordering::SeqCst) {
                        json!({"missing_canonical_receipt":true})
                    } else {
                        json!({"dispatch_id":"dispatch-owned-1","state":"TASK_STATE_WORKING","run_dir":"/synthetic/run"})
                    }
                }
                "status" => {
                    json!({"dispatch_id":"dispatch-owned-1","state":"TASK_STATE_WORKING","status_revision":7})
                }
                "result" => {
                    json!({"dispatch_id":"dispatch-owned-1","state":"TASK_STATE_COMPLETED","status_revision":8,
                    "result":{"body":"Synthetic untrusted report","truncated":false}})
                }
                "cancel" if args["expected_status_revision"] != 7 => {
                    json!({"receipt":"cancellation_unavailable",
                    "reason":"stale_status_revision","observed_status_revision":7})
                }
                "cancel" => match fake.cancel_mode.load(Ordering::SeqCst) {
                    1 => json!({"receipt":"cancellation_requested","state":"TASK_STATE_WORKING"}),
                    2 => json!({"receipt":"cancellation_confirmed","state":"TASK_STATE_CANCELED"}),
                    3 => json!({"receipt":"termination_unconfirmed","state":"TASK_STATE_FAILED"}),
                    _ => {
                        json!({"receipt":"cancellation_unavailable","reason":"non_managed_custom_execution"})
                    }
                },
                action => panic!("unexpected Staff operation {action}"),
            };
            Json(json!({"jsonrpc":"2.0","id":id,"result":{
                "isError":false,"content":[{"type":"text","text":result.to_string()}]
            }}))
            .into_response()
        }
        _ => StatusCode::BAD_REQUEST.into_response(),
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    state: AppState,
    app: Router,
    fake: Arc<Fake>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Fixture {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = Router::new()
            .route("/mcp", post(mcp))
            .with_state(fake.clone());
        let server = zeroclaw_spawn::spawn!(async move {
            axum::serve(listener, peer).await.unwrap();
        });
        let mut config = Config {
            data_dir: tmp.path().to_path_buf(),
            config_path: tmp.path().join("config.toml"),
            ..Default::default()
        };
        config.channels.session_backend = "sqlite".to_string();
        config.risk_profiles.insert(
            "edge-profile".into(),
            RiskProfileConfig {
                level: AutonomyLevel::Full,
                allowed_tools: Some(vec![
                    "tachi_start".into(),
                    "tachi_status".into(),
                    "tachi_result".into(),
                    "tachi_cancel".into(),
                ]),
                ..Default::default()
            },
        );
        config.runtime_profiles.insert(
            "edge-runtime".into(),
            RuntimeProfileConfig {
                max_actions_per_hour: 100,
                ..Default::default()
            },
        );
        config.agents.insert(
            AGENT.into(),
            AliasedAgentConfig {
                risk_profile: "edge-profile".into(),
                runtime_profile: "edge-runtime".into(),
                ..Default::default()
            },
        );
        config.tachi.enabled = true;
        config.tachi.endpoint = format!("http://{addr}/mcp");
        config.tachi.harnesses =
            HashMap::from([("reviewer".into(), "owner-admitted-profile".into())]);
        config.gateway.bridges.insert(
            "robot-tasks".into(),
            GatewayBridgeConfig {
                token_hash: zeroclaw_config::tokens::hash_token(TOKEN),
                sessions: vec!["voice".into(), "voice-next".into()],
                device: Some(GatewayDeviceBridgeConfig {
                    agent: AGENT.into(),
                    operations: vec![
                        "tachi_start".into(),
                        "tachi_status".into(),
                        "tachi_result".into(),
                        "tachi_cancel".into(),
                    ],
                }),
                ..Default::default()
            },
        );
        config.gateway.operator.token_hashes =
            vec![zeroclaw_config::tokens::hash_token("synthetic-operator")];
        let mut state = crate::api::test_state(config);
        state.pairing = Arc::new(zeroclaw_runtime::security::pairing::PairingGuard::new(
            false,
            &["synthetic-paired".into()],
        ));
        let app = routes(state.clone());
        Self {
            _tmp: tmp,
            state,
            app,
            fake,
            server,
        }
    }

    fn start(&self, id: &str) -> Value {
        json!({"version":1,"session_id":"voice","request_id":id,"harness":"reviewer",
            "task":"Review the synthetic implementation","staffing_reason":"explicit_user_request"})
    }

    fn read(&self, id: &str) -> Value {
        json!({"version":1,"session_id":"voice","request_id":id})
    }

    async fn call(&self, operation: &str, body: Value) -> (StatusCode, Value) {
        call(self.app.clone(), AGENT, operation, Some(TOKEN), body).await
    }

    fn db(&self) -> SqliteSessionBackend {
        SqliteSessionBackend::new(&self.state.config.read().data_dir).unwrap()
    }
}

async fn call(
    app: Router,
    agent: &str,
    op: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(format!("/api/agents/{agent}/delegations/{op}"))
        .header("content-type", "application/json")
        .extension(ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            1111,
        ))));
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let reply = app
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = reply.status();
    let bytes = to_bytes(reply.into_body(), 128 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn bridge_only_gate_agent_session_and_default_permissions_fail_closed() {
    let fixture = Fixture::new().await;
    for token in [None, Some("synthetic-operator"), Some("synthetic-paired")] {
        let (status, _) = call(
            fixture.app.clone(),
            AGENT,
            "start",
            token,
            fixture.start("r1"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(
        call(
            fixture.app.clone(),
            "other",
            "start",
            Some(TOKEN),
            fixture.start("r1")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let mut bad_session = fixture.start("r1");
    bad_session["session_id"] = json!("other-session");
    assert_eq!(
        fixture.call("start", bad_session).await.0,
        StatusCode::FORBIDDEN
    );
    fixture
        .state
        .config
        .write()
        .gateway
        .bridges
        .get_mut("robot-tasks")
        .unwrap()
        .device
        .as_mut()
        .unwrap()
        .operations = vec!["chat".into(), "cancel".into()];
    assert_eq!(
        fixture.call("start", fixture.start("r1")).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(fixture.fake.count("start"), 0);
    assert!(
        fixture
            .db()
            .read_delegation_request(AGENT, "r1")
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn live_readonly_allowlist_and_fresh_approval_rules_are_preserved() {
    let fixture = Fixture::new().await;
    for (level, allowed, always, expected) in [
        (AutonomyLevel::ReadOnly, true, false, "denied"),
        (AutonomyLevel::Supervised, true, false, "approval_required"),
        (AutonomyLevel::Full, true, true, "approval_required"),
        (AutonomyLevel::Full, false, false, "denied"),
    ] {
        {
            let mut cfg = fixture.state.config.write();
            let profile = cfg.risk_profiles.get_mut("edge-profile").unwrap();
            profile.level = level;
            profile.auto_approve = vec!["*".into()];
            profile.always_ask = if always {
                vec!["tachi_start".into()]
            } else {
                vec![]
            };
            profile.allowed_tools = Some(if allowed {
                vec!["tachi_start".into()]
            } else {
                vec![]
            });
        }
        let (status, body) = fixture.call("start", fixture.start("r1")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["output"]["code"], expected);
    }
    assert_eq!(fixture.fake.count("start"), 0);
    assert!(
        fixture
            .db()
            .read_delegation_request(AGENT, "r1")
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn receipts_deduplication_result_trust_and_next_window_recovery_share_one_ledger() {
    let fixture = Fixture::new().await;
    let (status, first) = fixture.call("start", fixture.start("r1")).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(
        first["output"]["receipt"]["dispatch_id"],
        "dispatch-owned-1"
    );
    let (_, repeated) = fixture.call("start", fixture.start("r1")).await;
    assert_eq!(repeated["output"]["replayed"], true);
    assert_eq!(repeated["output"]["dispatch_id"], "dispatch-owned-1");
    assert_eq!(fixture.fake.count("start"), 1);
    let (status, read) = fixture.call("status", fixture.read("r1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["output"]["status"]["status_revision"], 7);
    let (status, report) = fixture.call("result", fixture.read("r1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["output"]["trust"], "untrusted_external_report");
    assert_eq!(report["output"]["accepted_by_body"], false);
    assert_eq!(
        report["output"]["result"]["body"],
        "Synthetic untrusted report"
    );
    fixture
        .db()
        .claim_delegation_request("other-agent", "private-request", "digest", "route")
        .unwrap();
    let fresh_router = routes(fixture.state.clone());
    let (status, recent) = call(
        fresh_router,
        AGENT,
        "list",
        Some(TOKEN),
        json!({"version":1,"session_id":"voice-next"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(recent["request_id"].is_null());
    assert_eq!(recent["output"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(recent["output"]["items"][0]["request_id"], "r1");
    assert_eq!(recent["output"]["items"][0]["route_matches"], true);
    assert_eq!(fixture.fake.count("cancel"), 0);
}

#[tokio::test]
async fn malformed_start_receipt_remains_unresolved_and_never_resubmits() {
    let fixture = Fixture::new().await;
    fixture.fake.malformed_start.store(true, Ordering::SeqCst);
    let (status, first) = fixture.call("start", fixture.start("r1")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(first["output"]["code"], "submission_unknown");
    let (_, again) = fixture.call("start", fixture.start("r1")).await;
    assert_eq!(again["output"]["code"], "submission_unresolved");
    let (_, read) = fixture.call("status", fixture.read("r1")).await;
    assert_eq!(read["output"]["code"], "submission_unresolved");
    let (_, recent) = fixture
        .call("list", json!({"version":1,"session_id":"voice"}))
        .await;
    assert_eq!(recent["output"]["items"][0]["submission"], "unresolved");
    assert_eq!(fixture.fake.count("start"), 1);
    assert_eq!(fixture.fake.count("status"), 0);
}

#[tokio::test]
async fn dropping_http_caller_does_not_abort_admitted_start_or_send_cancel() {
    let fixture = Fixture::new().await;
    fixture.fake.hold_start.store(true, Ordering::SeqCst);
    let app = fixture.app.clone();
    let body = fixture.start("r1");
    let caller =
        zeroclaw_spawn::spawn!(async move { call(app, AGENT, "start", Some(TOKEN), body).await });
    tokio::time::timeout(Duration::from_secs(3), fixture.fake.entered.notified())
        .await
        .unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    fixture.fake.release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if fixture
                .db()
                .read_delegation_request(AGENT, "r1")
                .unwrap()
                .is_some_and(|b| b.dispatch_id.is_some())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(fixture.fake.count("start"), 1);
    assert_eq!(fixture.fake.count("cancel"), 0);
    let (status, read) = fixture.call("status", fixture.read("r1")).await;
    assert_eq!(status, StatusCode::OK, "{read}");
}

#[tokio::test]
async fn cancellation_preserves_revision_unsupported_and_all_three_receipts() {
    let fixture = Fixture::new().await;
    assert_eq!(
        fixture.call("start", fixture.start("r1")).await.0,
        StatusCode::OK
    );
    let mut cancel = fixture.read("r1");
    cancel["expected_status_revision"] = json!(6);
    let (_, stale) = fixture.call("cancel", cancel.clone()).await;
    assert_eq!(stale["output"]["code"], "stale_revision");
    cancel["expected_status_revision"] = json!(7);
    let (_, unsupported) = fixture.call("cancel", cancel.clone()).await;
    assert_eq!(unsupported["output"]["code"], "cancel_unsupported");
    for (mode, expected) in [
        (1, "requested"),
        (2, "confirmed"),
        (3, "termination_unconfirmed"),
    ] {
        fixture.fake.cancel_mode.store(mode, Ordering::SeqCst);
        let (status, reply) = fixture.call("cancel", cancel.clone()).await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        assert_eq!(reply["output"]["receipt"]["outcome"], expected);
    }
}

#[tokio::test]
async fn routing_storage_and_token_changes_cannot_redirect_an_existing_request() {
    let fixture = Fixture::new().await;
    let other = Fixture::new().await;
    assert_eq!(
        fixture.call("start", fixture.start("r1")).await.0,
        StatusCode::OK
    );
    let old_endpoint = fixture.state.config.read().tachi.endpoint.clone();
    fixture.state.config.write().tachi.endpoint = other.state.config.read().tachi.endpoint.clone();
    let (_, conflict) = fixture.call("status", fixture.read("r1")).await;
    assert_eq!(conflict["output"]["code"], "route_conflict");
    assert_eq!(other.fake.count("status"), 0);
    let (_, recent) = fixture
        .call("list", json!({"version":1,"session_id":"voice"}))
        .await;
    assert_eq!(recent["output"]["items"][0]["route_matches"], false);
    fixture.state.config.write().tachi.endpoint = old_endpoint;
    fixture.state.config.write().data_dir = other.state.config.read().data_dir.clone();
    let (_, storage) = fixture.call("status", fixture.read("r1")).await;
    assert_eq!(storage["output"]["code"], "storage_root_changed");
    fixture
        .state
        .config
        .write()
        .gateway
        .bridges
        .get_mut("robot-tasks")
        .unwrap()
        .token_hash = zeroclaw_config::tokens::hash_token("rotated-synthetic-token");
    assert_eq!(
        fixture.call("status", fixture.read("r1")).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(fixture.fake.count("status"), 0);
}

#[tokio::test]
async fn action_budget_survives_successive_http_requests() {
    let fixture = Fixture::new().await;
    fixture
        .state
        .config
        .write()
        .runtime_profiles
        .get_mut("edge-runtime")
        .unwrap()
        .max_actions_per_hour = 1;
    assert_eq!(
        fixture.call("start", fixture.start("r1")).await.0,
        StatusCode::OK
    );
    let (status, denied) = fixture.call("start", fixture.start("r2")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denied["output"]["code"], "denied");
    assert_eq!(fixture.fake.count("start"), 1);
    assert!(
        fixture
            .db()
            .read_delegation_request(AGENT, "r2")
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn ingress_capacity_and_deadline_bound_admission_without_claiming_a_stop() {
    let fixture = Fixture::new().await;
    fixture.fake.hold_start.store(true, Ordering::SeqCst);
    let context = Arc::new(ControlContext {
        selected_data_dir: fixture.state.config.read().data_dir.clone(),
        tracker: PerSenderTracker::new(),
        slots: Arc::new(tokio::sync::Semaphore::new(1)),
        timeout: Duration::from_millis(300),
    });
    let app = router(fixture.state.clone(), context);
    let caller_app = app.clone();
    let start = fixture.start("r1");
    let caller = zeroclaw_spawn::spawn!(async move {
        call(caller_app, AGENT, "start", Some(TOKEN), start).await
    });
    tokio::time::timeout(Duration::from_secs(3), fixture.fake.entered.notified())
        .await
        .unwrap();
    let (status, busy) = call(app, AGENT, "status", Some(TOKEN), fixture.read("r1")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(busy["output"]["code"], "control_busy");
    let (status, expired) = caller.await.unwrap();
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(expired["output"]["code"], "submission_unknown");
    fixture.fake.release.notify_one();
    let (_, unresolved) = fixture.call("status", fixture.read("r1")).await;
    assert_eq!(unresolved["output"]["code"], "submission_unresolved");
    assert_eq!(fixture.fake.count("start"), 1);
    assert_eq!(fixture.fake.count("cancel"), 0);
}

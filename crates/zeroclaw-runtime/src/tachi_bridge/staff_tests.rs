//! `tachi_staff` client tests against a scripted MCP server that speaks the
//! same streamable-HTTP JSON-RPC shape as the Tachi daemon.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};
use zeroclaw_config::tachi::TachiConfig;

use super::staff::{
    CancelOutcome, HEADER_AGENT_IDENTITY, HEADER_CLIENT, HEADER_PROFILE, HEADER_PROJECT, RunState,
    RunStatus, StaffReceipt, StaffRefs, StaffingReason, TACHI_STAFF_TOOL, TACHI_TASK_TOOL,
    TachiStaffClient, TachiStaffError,
};

/// Tachi's golden external-staffing contract (copied verbatim from
/// kckylechen1/tachi `docs/engineering/architecture/`, tachi@fbab02c).
const STAFFING_CONTRACT_FIXTURE: &str =
    include_str!("fixtures/external-staffing-contract-v1.fixture.json");

/// Every field `TachiStaffParams` accepts (tachi-params
/// `facade/orchestration.rs`, `deny_unknown_fields`). A key outside this set
/// would make Tachi refuse the whole call.
const TACHI_STAFF_PARAM_FIELDS: &[&str] = &[
    "action",
    "format",
    "dispatch_id",
    "expected_status_revision",
    "task",
    "staffing_reason",
    "profile",
    "worker",
    "project",
    "stage",
    "issue_ref",
    "pr_ref",
    "flow_id",
    "recommendation_ref",
    "declared_file_scope",
];

// ─────────────────────────────────────────────────────────────────────────
// Scripted MCP server
// ─────────────────────────────────────────────────────────────────────────

enum Reply {
    Ok(Value),
    Text(String),
    ToolError(String),
    RpcError(i64, String),
    Http(StatusCode),
}

struct Call {
    headers: HashMap<String, String>,
    tool: String,
    args: Value,
}

type Script = Box<dyn Fn(&str, &Value) -> Reply + Send + Sync>;

struct Fake {
    script: Script,
    calls: Mutex<Vec<Call>>,
    sessions_opened: AtomicUsize,
    expire_next_call: AtomicBool,
    fail_next_initialize: AtomicBool,
}

impl Fake {
    fn calls(&self) -> Vec<(String, Value)> {
        self.calls
            .lock()
            .expect("calls lock")
            .iter()
            .map(|call| (call.tool.clone(), call.args.clone()))
            .collect()
    }
}

async fn handle(
    State(fake): State<Arc<Fake>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let id = body.get("id").cloned();
    let method = body.get("method").and_then(Value::as_str).unwrap_or("");
    match method {
        "initialize" => {
            let session = fake.sessions_opened.fetch_add(1, Ordering::SeqCst) + 1;
            if fake.fail_next_initialize.swap(false, Ordering::SeqCst) {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            let mut response = Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "fake-tachi", "version": "0" }
                }
            }))
            .into_response();
            response.headers_mut().insert(
                "Mcp-Session-Id",
                format!("session-{session}").parse().expect("header value"),
            );
            response
        }
        "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
        "tools/call" => {
            if fake.expire_next_call.swap(false, Ordering::SeqCst) {
                return StatusCode::NOT_FOUND.into_response();
            }
            let params = body.get("params").cloned().unwrap_or(Value::Null);
            let tool = params["name"].as_str().unwrap_or("").to_string();
            let args = params["arguments"].clone();
            let recorded = headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_ascii_lowercase(),
                        value.to_str().unwrap_or("").to_string(),
                    )
                })
                .collect();
            let reply = (fake.script)(&tool, &args);
            fake.calls.lock().expect("calls lock").push(Call {
                headers: recorded,
                tool,
                args,
            });
            let text_result = |text: String, is_error: bool| {
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": text }],
                        "isError": is_error
                    }
                }))
                .into_response()
            };
            match reply {
                Reply::Ok(value) => text_result(value.to_string(), false),
                Reply::Text(text) => text_result(text, false),
                Reply::ToolError(text) => text_result(text, true),
                Reply::RpcError(code, message) => Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                }))
                .into_response(),
                Reply::Http(status) => status.into_response(),
            }
        }
        _ => StatusCode::BAD_REQUEST.into_response(),
    }
}

async fn serve(
    script: impl Fn(&str, &Value) -> Reply + Send + Sync + 'static,
) -> (Arc<Fake>, String) {
    let fake = Arc::new(Fake {
        script: Box::new(script),
        calls: Mutex::new(Vec::new()),
        sessions_opened: AtomicUsize::new(0),
        expire_next_call: AtomicBool::new(false),
        fail_next_initialize: AtomicBool::new(false),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake tachi");
    let addr = listener.local_addr().expect("fake tachi addr");
    let app = axum::Router::new()
        .route("/mcp", post(handle))
        .with_state(fake.clone());
    zeroclaw_spawn::spawn!(async move {
        axum::serve(listener, app).await.expect("fake tachi serves");
    });
    (fake, format!("http://{addr}/mcp"))
}

fn config(endpoint: &str) -> TachiConfig {
    TachiConfig {
        enabled: true,
        endpoint: endpoint.to_string(),
        project: Some("zeroclaw".to_string()),
        harnesses: HashMap::from([
            ("codex".to_string(), "codex_55_review".to_string()),
            ("claude".to_string(), "claude_plan".to_string()),
        ]),
        ..TachiConfig::default()
    }
}

fn client(endpoint: &str) -> TachiStaffClient {
    TachiStaffClient::from_config(&config(endpoint), "home").expect("enabled config")
}

fn working_receipt(id: &str) -> Value {
    json!({
        "dispatch_id": id,
        "state": "TASK_STATE_WORKING",
        "run_dir": format!("/home/owner/.tachi/runs/{id}"),
        "status": "completed",
        "action": "start",
        "selected_profile": "codex_55_review",
        "a_future_field": { "nested": true }
    })
}

// ─────────────────────────────────────────────────────────────────────────
// start
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn legacy_staff_echoing_modern_version_keeps_the_initialized_session() {
    type Requests = Arc<Mutex<Vec<(HeaderMap, Value)>>>;
    async fn legacy(
        State(requests): State<Requests>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Response {
        requests
            .lock()
            .expect("request log")
            .push((headers.clone(), body.clone()));
        let id = body.get("id").cloned();
        match body["method"].as_str().unwrap_or("") {
            "server/discover" => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "Unexpected message, expect initialize request",
            )
                .into_response(),
            "initialize" => {
                let mut response = Json(json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {
                        "protocolVersion": body["params"]["protocolVersion"],
                        "capabilities": { "tools": {} }
                    }
                }))
                .into_response();
                response.headers_mut().insert(
                    "Mcp-Session-Id",
                    "legacy-echo-session".parse().expect("session header"),
                );
                response
            }
            "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
            "tools/call"
                if headers.get("Mcp-Session-Id").and_then(|h| h.to_str().ok())
                    == Some("legacy-echo-session")
                    && !headers.contains_key("Mcp-Method")
                    && body["params"].get("_meta").is_none() =>
            {
                Json(json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "isError": false,
                        "content": [{ "type": "text", "text": working_receipt("d-legacy-echo").to_string() }] }
                }))
                .into_response()
            }
            _ => StatusCode::UNPROCESSABLE_ENTITY.into_response(),
        }
    }
    let requests = Requests::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind legacy echo peer");
    let endpoint = format!(
        "http://{}/mcp",
        listener.local_addr().expect("peer address")
    );
    let app = axum::Router::new()
        .route("/mcp", post(legacy))
        .with_state(requests.clone());
    zeroclaw_spawn::spawn!(async move {
        axum::serve(listener, app)
            .await
            .expect("legacy peer serves");
    });
    let receipt = client(&endpoint)
        .start(
            "codex",
            "bounded task",
            StaffingReason::ExplicitUserRequest,
            &StaffRefs::default(),
        )
        .await
        .expect("legacy echo start retains its initialized session");
    assert_eq!(receipt.dispatch_id, "d-legacy-echo");
    let requests = requests.lock().expect("request log");
    let methods: Vec<_> = requests
        .iter()
        .map(|(_, body)| body["method"].as_str().unwrap())
        .collect();
    assert_eq!(
        methods,
        [
            "server/discover",
            "initialize",
            "notifications/initialized",
            "tools/call"
        ]
    );
    assert_eq!(requests[1].1["params"]["protocolVersion"], "2026-07-28");
    assert_eq!(requests[3].0["Mcp-Session-Id"], "legacy-echo-session");
    assert_eq!(requests[3].0["x-tachi-profile"], "standard");
    assert_eq!(requests[3].0["x-tachi-agent-identity"], "zeroclaw:home");
}

#[tokio::test]
async fn modern_staff_bootstrap_uses_discover_and_negotiated_request_metadata() {
    type Requests = Arc<Mutex<Vec<(HeaderMap, Value)>>>;
    async fn modern(
        State(requests): State<Requests>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Response {
        requests
            .lock()
            .expect("request log")
            .push((headers.clone(), body.clone()));
        let id = body.get("id").cloned();
        let method = body["method"].as_str().unwrap_or("");
        let wire_ok = headers
            .get("MCP-Protocol-Version")
            .and_then(|h| h.to_str().ok())
            == Some("2026-07-28")
            && headers.get("Mcp-Method").and_then(|h| h.to_str().ok()) == Some(method)
            && body["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"] == "2026-07-28";
        if method == "initialize" || !wire_ok {
            return (StatusCode::BAD_REQUEST, Json(json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": if method == "initialize" { -32022 } else { -32020 },
                    "message": "modern wire required", "data": { "supportedVersions": ["2026-07-28"] } }
            }))).into_response();
        }
        let result = match method {
            "server/discover" => json!({
                "resultType": "complete", "supportedVersions": ["2026-07-28"],
                "capabilities": { "tools": {} }
            }),
            "tools/call" => json!({
                "resultType": "complete", "isError": false,
                "content": [{ "type": "text", "text": working_receipt("d-modern").to_string() }]
            }),
            _ => return StatusCode::BAD_REQUEST.into_response(),
        };
        Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
    }
    let requests = Requests::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind modern peer");
    let endpoint = format!(
        "http://{}/mcp",
        listener.local_addr().expect("modern peer address")
    );
    let app = axum::Router::new()
        .route("/mcp", post(modern))
        .with_state(requests.clone());
    zeroclaw_spawn::spawn!(async move {
        axum::serve(listener, app)
            .await
            .expect("modern peer serves");
    });
    let receipt = client(&endpoint)
        .start(
            "codex",
            "bounded task",
            StaffingReason::ExplicitUserRequest,
            &StaffRefs::default(),
        )
        .await
        .expect("modern start");
    assert_eq!(receipt.dispatch_id, "d-modern");
    let requests = requests.lock().expect("request log");
    let methods: Vec<_> = requests
        .iter()
        .map(|(_, body)| body["method"].as_str().unwrap())
        .collect();
    assert_eq!(
        methods,
        ["server/discover", "server/discover", "tools/call"]
    );
    let (headers, call) = requests.last().expect("tool call");
    assert_eq!(headers["x-tachi-profile"], "standard");
    assert_eq!(headers["x-tachi-agent-identity"], "zeroclaw:home");
    assert_eq!(headers["Mcp-Name"], TACHI_STAFF_TOOL);
    assert!(!headers.contains_key("Mcp-Session-Id"));
    assert_eq!(
        call["params"]["_meta"]["io.modelcontextprotocol/clientInfo"]["name"],
        "zeroclaw"
    );
    assert_eq!(call["params"]["arguments"]["profile"], "codex_55_review");
}

#[tokio::test]
async fn start_maps_harness_to_profile_and_sends_identity_headers() {
    let (fake, endpoint) = serve(|_, _| Reply::Ok(working_receipt("d-1"))).await;
    let client = client(&endpoint);
    let refs = StaffRefs {
        issue_ref: Some("owner/zeroclaw#381".to_string()),
        ..StaffRefs::default()
    };

    let receipt = client
        .start(
            "codex",
            "Have Codex review the claude.rs provider",
            StaffingReason::ExplicitUserRequest,
            &refs,
        )
        .await
        .expect("accepted start");
    assert_eq!(
        receipt,
        StaffReceipt {
            dispatch_id: "d-1".to_string(),
            state: RunState::Working,
            run_dir: "/home/owner/.tachi/runs/d-1".to_string(),
        }
    );

    let calls = fake.calls.lock().expect("calls lock");
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.tool, TACHI_STAFF_TOOL);
    assert_eq!(call.args["action"], "start");
    assert_eq!(call.args["profile"], "codex_55_review");
    assert_eq!(call.args["staffing_reason"], "explicit_user_request");
    // Harness names in the task are ordinary content (ADR-017 §3).
    assert_eq!(
        call.args["task"],
        "Have Codex review the claude.rs provider"
    );
    assert_eq!(call.args["project"], "zeroclaw");
    assert_eq!(call.args["issue_ref"], "owner/zeroclaw#381");
    assert!(call.args.get("pr_ref").is_none());
    // The body names a profile, never a worker, command, or placement.
    assert!(call.args.get("worker").is_none());
    let keys: BTreeSet<&str> = call
        .args
        .as_object()
        .expect("object args")
        .keys()
        .map(String::as_str)
        .collect();
    for key in &keys {
        assert!(
            TACHI_STAFF_PARAM_FIELDS.contains(key),
            "`{key}` is not a TachiStaffParams field; Tachi would refuse the call"
        );
    }

    assert_eq!(call.headers[HEADER_PROFILE], "standard");
    assert_eq!(call.headers[HEADER_AGENT_IDENTITY], "zeroclaw:home");
    assert_eq!(call.headers[HEADER_CLIENT], "zeroclaw");
    assert_eq!(call.headers[HEADER_PROJECT], "zeroclaw");
    assert_eq!(call.headers["mcp-session-id"], "session-1");
}

#[tokio::test]
async fn unknown_harness_and_empty_task_never_reach_tachi() {
    let (fake, endpoint) = serve(|_, _| Reply::Ok(working_receipt("never"))).await;
    let client = client(&endpoint);

    let err = client
        .start(
            "grok",
            "do it",
            StaffingReason::ExplicitUserRequest,
            &StaffRefs::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(err, TachiStaffError::UnknownHarness("grok".to_string()));

    let err = client
        .start(
            "codex",
            "   ",
            StaffingReason::ExplicitUserRequest,
            &StaffRefs::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, TachiStaffError::Refused(_)), "{err:?}");

    assert!(fake.calls().is_empty());
    assert_eq!(fake.sessions_opened.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn one_session_is_reused_across_calls() {
    let (fake, endpoint) = serve(|_, args| {
        Reply::Ok(json!({
            "dispatch_id": args["dispatch_id"],
            "state": "TASK_STATE_WORKING",
            "status_revision": 1
        }))
    })
    .await;
    let client = client(&endpoint);
    client.status("d-1").await.expect("first");
    client.status("d-1").await.expect("second");
    assert_eq!(fake.sessions_opened.load(Ordering::SeqCst), 1);
    assert_eq!(fake.calls().len(), 2);
}

// ─────────────────────────────────────────────────────────────────────────
// status / result
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn status_reads_the_canonical_receipt_and_ignores_extra_fields() {
    let (fake, endpoint) = serve(|_, args| {
        let state = match args["dispatch_id"].as_str() {
            Some("done") => "TASK_STATE_COMPLETED",
            Some("partial") => "TASK_STATE_INPUT_REQUIRED",
            _ => "TASK_STATE_WORKING",
        };
        Reply::Ok(json!({
            "dispatch_id": args["dispatch_id"],
            "state": state,
            "status_revision": 7,
            "closure_kind": "partial",
            "result_written": state == "TASK_STATE_COMPLETED",
            "updated_at": "2026-09-26T10:00:00Z",
            "agent": "codex",
            "authority": { "enforced_by": "certified" },
            "identity_receipt": { "planned": { "model": "x" } },
            "read_projection": { "execution_state": "running" },
            "status": "completed",
            "action": "status"
        }))
    })
    .await;
    let client = client(&endpoint);

    let running = client.status("d-1").await.expect("status");
    assert_eq!(running.state, RunState::Working);
    assert_eq!(running.revision, Some(7));
    assert_eq!(
        running.read_projection.as_ref().unwrap()["execution_state"],
        "running"
    );
    assert!(!running.is_terminal());

    let done = client.status("done").await.expect("status");
    assert_eq!(done.state, RunState::Completed);
    assert_eq!(done.result_written, Some(true));
    assert!(done.is_terminal());

    let partial = client.status("partial").await.expect("status");
    assert!(partial.is_terminal(), "INPUT_REQUIRED + partial is closed");

    let (tool, args) = &fake.calls()[0];
    assert_eq!(tool, TACHI_STAFF_TOOL);
    assert_eq!(
        args,
        &json!({ "action": "status", "format": "json", "dispatch_id": "d-1" })
    );
}

#[tokio::test]
async fn result_reads_result_md_through_tachi_task() {
    let (fake, endpoint) = serve(|_, args| match args["dispatch_id"].as_str() {
        Some("done") => Reply::Ok(json!({
            "status": "ok",
            "dispatch_id": "done",
            "terminal": true,
            "state": "TASK_STATE_COMPLETED",
            "task": {},
            "run_status": {"dispatch_id":"done", "state":"TASK_STATE_COMPLETED", "status_revision":4},
            "result": {
                "body": "# Verdict\n\nAll tests pass.",
                "truncated": true,
                "full_size_chars": 9000,
                "full_size_bytes": 9000
            }
        })),
        _ => Reply::Ok(json!({
            "status": "ok",
            "state": "TASK_STATE_WORKING",
            "run_status": {"dispatch_id":"pending", "state":"TASK_STATE_WORKING"},
            "result": { "body": null, "note": "no result.md found in run directory" }
        })),
    })
    .await;
    let client = client(&endpoint);

    let done = client.result("done").await.expect("result");
    assert_eq!(done.state, RunState::Completed);
    assert_eq!(done.body.as_deref(), Some("# Verdict\n\nAll tests pass."));
    assert!(done.truncated);

    let pending = client.result("pending").await.expect("result");
    assert_eq!(pending.body, None);
    assert!(!pending.truncated);

    let (tool, args) = &fake.calls()[0];
    assert_eq!(tool, TACHI_TASK_TOOL);
    assert_eq!(args["action"], "status");
    assert_eq!(args["include_result"], true);
}

// ─────────────────────────────────────────────────────────────────────────
// cancel
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn cancel_maps_every_tachi_receipt_to_a_typed_outcome() {
    let (fake, endpoint) = serve(|_, args| {
        let unavailable = |reason: &str, observed: Value| {
            Reply::Ok(json!({
                "receipt": "cancellation_unavailable",
                "dispatch_id": args["dispatch_id"],
                "expected_status_revision": args["expected_status_revision"],
                "observed_status_revision": observed,
                "reason": reason,
                "lifecycle_owner": "unknown_or_unavailable",
                "backend": "unknown_or_unavailable",
                "state": null
            }))
        };
        match args["expected_status_revision"].as_u64() {
            Some(1) => unavailable("non_managed_custom_execution", json!(1)),
            Some(2) => unavailable("stale_status_revision", json!(5)),
            Some(3) => Reply::Ok(json!({
                "receipt": "cancellation_confirmed",
                "dispatch_id": args["dispatch_id"],
                "state": "TASK_STATE_CANCELED",
                "lifecycle_owner": "memory_server_managed_custom",
                "backend": "custom"
            })),
            Some(4) => Reply::Ok(json!({
                "receipt": "cancellation_requested",
                "state": "TASK_STATE_WORKING"
            })),
            _ => unavailable("terminal_or_recovery_state", json!(9)),
        }
    })
    .await;
    let client = client(&endpoint);

    // CLI-backed runs: Tachi cannot cancel them, and the body says so.
    assert_eq!(
        client.cancel("cli-run", 1).await.unwrap_err(),
        TachiStaffError::CancelUnsupported {
            reason: "non_managed_custom_execution".to_string()
        }
    );
    assert_eq!(
        client.cancel("d-1", 2).await.unwrap_err(),
        TachiStaffError::StaleRevision {
            expected: 2,
            observed: Some(5)
        }
    );
    let confirmed = client.cancel("custom-run", 3).await.expect("confirmed");
    assert_eq!(confirmed.outcome, CancelOutcome::Confirmed);
    assert_eq!(confirmed.state, Some(RunState::Canceled));
    let requested = client.cancel("custom-run", 4).await.expect("requested");
    assert_eq!(requested.outcome, CancelOutcome::Requested);
    assert!(matches!(
        client.cancel("done", 9).await.unwrap_err(),
        TachiStaffError::Refused(reason) if reason.contains("terminal_or_recovery_state")
    ));

    let (_, args) = &fake.calls()[0];
    assert_eq!(
        args,
        &json!({
            "action": "cancel",
            "dispatch_id": "cli-run",
            "expected_status_revision": 1
        })
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Typed failures and fail-closed
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tachi_failures_are_typed() {
    let (fake, endpoint) = serve(|_, args| match args["dispatch_id"].as_str() {
        Some("tool-error") => {
            Reply::ToolError("staff_status: unknown dispatch_id \"tool-error\"".to_string())
        }
        Some("rpc-error") => Reply::RpcError(-32602, "unknown field `cwd`".to_string()),
        Some("not-json") => Reply::Text("## Tachi staff status".to_string()),
        Some("missing-state") => Reply::Ok(json!({ "dispatch_id": "missing-state" })),
        _ => Reply::Http(StatusCode::INTERNAL_SERVER_ERROR),
    })
    .await;
    let client = client(&endpoint);

    assert!(matches!(
        client.status("tool-error").await.unwrap_err(),
        TachiStaffError::Refused(detail) if detail.contains("unknown dispatch_id")
    ));
    assert!(matches!(
        client.status("rpc-error").await.unwrap_err(),
        TachiStaffError::Refused(detail) if detail.contains("-32602")
    ));
    assert!(matches!(
        client.status("not-json").await.unwrap_err(),
        TachiStaffError::Protocol(_)
    ));
    assert!(matches!(
        client.status("missing-state").await.unwrap_err(),
        TachiStaffError::Protocol(_)
    ));
    assert!(matches!(
        client.status("http-500").await.unwrap_err(),
        TachiStaffError::Unavailable(_)
    ));
    // Refusals keep the session; the transport failure dropped it, so the
    // next call opens a fresh one.
    assert!(client.status("tool-error").await.is_err());
    assert_eq!(fake.sessions_opened.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn tachi_down_is_unavailable_and_nothing_runs() {
    // A port that nothing listens on.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}/mcp", listener.local_addr().expect("addr"));
    drop(listener);
    let client = client(&endpoint);

    let err = client
        .start(
            "codex",
            "fix the flaky test",
            StaffingReason::ExplicitUserRequest,
            &StaffRefs::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, TachiStaffError::Unavailable(_)), "{err:?}");
    assert!(matches!(
        client.status("d-1").await.unwrap_err(),
        TachiStaffError::Unavailable(_)
    ));
    // Nothing local ran: the client module holds no process capability
    // (`tests::module_source_scans_hold` scans staff.rs).
}

#[test]
fn disabled_or_invalid_tachi_config_fails_closed() {
    let disabled = TachiConfig::default();
    assert!(matches!(
        TachiStaffClient::from_config(&disabled, "home").unwrap_err(),
        TachiStaffError::Unavailable(reason) if reason.contains("not enabled")
    ));
    let remote = TachiConfig {
        enabled: true,
        endpoint: "http://192.168.1.9:6919/mcp".to_string(),
        ..TachiConfig::default()
    };
    assert!(matches!(
        TachiStaffClient::from_config(&remote, "home").unwrap_err(),
        TachiStaffError::Unavailable(reason) if reason.contains("loopback")
    ));
}

#[tokio::test]
async fn stale_session_after_tachi_restart_is_reopened_once() {
    let (fake, endpoint) = serve(|_, args| {
        Reply::Ok(json!({
            "dispatch_id": args["dispatch_id"],
            "state": "TASK_STATE_WORKING"
        }))
    })
    .await;
    let client = client(&endpoint);
    client.status("d-1").await.expect("warm session");

    fake.expire_next_call.store(true, Ordering::SeqCst);
    let status = client.status("d-1").await.expect("reopened");
    assert_eq!(status.revision, None);
    assert_eq!(fake.sessions_opened.load(Ordering::SeqCst), 2);
    // The expired request was refused by HTTP 404 before Tachi handled it,
    // so only the two answered calls were recorded.
    assert_eq!(fake.calls().len(), 2);
}

#[tokio::test]
async fn wait_until_terminal_polls_at_the_configured_interval() {
    let polls = Arc::new(AtomicUsize::new(0));
    let counter = polls.clone();
    let (_fake, endpoint) = serve(move |_, _| {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        let state = if n >= 2 {
            "TASK_STATE_COMPLETED"
        } else {
            "TASK_STATE_WORKING"
        };
        Reply::Ok(json!({ "dispatch_id": "d-1", "state": state }))
    })
    .await;
    assert_eq!(client(&endpoint).poll_interval().as_secs(), 15, "default");
    let client = TachiStaffClient::from_config(
        &TachiConfig {
            poll_secs: 1,
            ..config(&endpoint)
        },
        "home",
    )
    .expect("config");
    let started = std::time::Instant::now();
    let status = client
        .wait_until_terminal("d-1", std::time::Duration::from_secs(3600))
        .await
        .expect("terminal");
    assert_eq!(status.state, RunState::Completed);
    assert_eq!(polls.load(Ordering::SeqCst), 3);
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(2),
        "two waits of poll_secs between three polls"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Contract pins
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn golden_staffing_contract_matches_our_receipt_and_terminal_rules() {
    let contract: Value = serde_json::from_str(STAFFING_CONTRACT_FIXTURE).expect("fixture is JSON");
    assert_eq!(contract["schema_version"], "external_staffing.v1");

    // An accepted start carrying exactly the contract's required fields
    // (plus anything else) deserializes into our receipt.
    let required: Vec<&str> = contract["accepted_start"]["required_fields"]
        .as_array()
        .expect("required_fields")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let initial_state = contract["accepted_start"]["initial_state"]
        .as_str()
        .expect("initial_state");
    let mut wire = serde_json::Map::new();
    for field in &required {
        let value = match *field {
            "state" => initial_state.to_string(),
            other => format!("{other}-value"),
        };
        wire.insert((*field).to_string(), Value::String(value));
    }
    wire.insert("unlisted".to_string(), json!(1));
    let receipt: StaffReceipt =
        serde_json::from_value(Value::Object(wire.clone())).expect("receipt from contract");
    assert_eq!(receipt.state, RunState::Working);
    // Dropping any required field must fail: we rely on all of them.
    for field in &required {
        let mut partial = wire.clone();
        partial.remove(*field);
        assert!(
            serde_json::from_value::<StaffReceipt>(Value::Object(partial)).is_err(),
            "receipt must require `{field}`"
        );
    }

    let status = |state: &str, closure: Option<&str>| RunStatus {
        dispatch_id: "d".to_string(),
        state: RunState::from_wire(state),
        revision: None,
        closure_kind: closure.map(str::to_string),
        result_written: None,
        updated_at: None,
        read_projection: None,
    };
    for state in contract["terminal_semantics"]["terminal_states"]
        .as_array()
        .expect("terminal_states")
    {
        let state = state.as_str().expect("state string");
        assert!(status(state, None).is_terminal(), "{state} is terminal");
        assert_eq!(RunState::from_wire(state).as_wire(), state);
    }
    let partial = &contract["terminal_semantics"]["closed_partial"];
    let partial_state = partial["state"].as_str().expect("partial state");
    assert!(status(partial_state, partial["closure_kind"].as_str()).is_terminal());
    assert!(!status(partial_state, None).is_terminal());
    assert!(!status(initial_state, None).is_terminal());
}

#[test]
fn staffing_reasons_use_tachi_wire_spelling() {
    let spelled: Vec<&str> = StaffingReason::ALL.iter().map(|r| r.as_str()).collect();
    assert_eq!(
        spelled,
        [
            "explicit_user_request",
            "durable_cross_session",
            "cross_device_remote",
            "native_subagent_unavailable",
        ]
    );
    for reason in StaffingReason::ALL {
        assert_eq!(
            serde_json::to_value(reason).expect("serialize"),
            json!(reason.as_str())
        );
        assert_eq!(StaffingReason::parse(reason.as_str()), Some(reason));
    }
    assert_eq!(StaffingReason::parse("because"), None);
}

/// Live check against a real local Tachi daemon: `TACHI_LIVE=1 cargo test -p
/// zeroclaw-runtime tachi_live -- --ignored`. It only reads the status of a
/// dispatch id that cannot exist, so it starts nothing; it proves the
/// endpoint, the handshake, the headers, and the typed refusal path.
#[tokio::test]
#[ignore = "needs a running Tachi daemon on 127.0.0.1:6919 and TACHI_LIVE=1"]
async fn tachi_live_status_of_unknown_run_is_a_typed_refusal() {
    if std::env::var("TACHI_LIVE").as_deref() != Ok("1") {
        return;
    }
    let config = TachiConfig {
        enabled: true,
        ..TachiConfig::default()
    };
    let client = TachiStaffClient::from_config(&config, "live-test").expect("config");
    let err = client
        .status("zeroclaw-live-probe-does-not-exist")
        .await
        .unwrap_err();
    assert!(matches!(err, TachiStaffError::Refused(_)), "{err:?}");
}

#[tokio::test]
async fn transmitted_start_on_stale_session_is_unknown_and_never_replayed() {
    let (fake, endpoint) = serve(|_, _| Reply::Http(StatusCode::NOT_FOUND)).await;
    let error = client(&endpoint)
        .start(
            "codex",
            "review Codex on GitHub together",
            StaffingReason::ExplicitUserRequest,
            &StaffRefs::default(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, TachiStaffError::SubmissionUnknown(_)),
        "{error:?}"
    );
    assert_eq!(
        fake.calls().len(),
        1,
        "404 after POST must not trigger another start"
    );
    assert_eq!(fake.sessions_opened.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn result_retains_canonical_receipt_and_inferred_projection_provenance() {
    let (_, endpoint) = serve(|_, _| {
        Reply::Ok(json!({
            "state":"TASK_STATE_FAILED", "task":{"state_basis":"run_stale_timeout"},
            "run_status":{"dispatch_id":"d-1","state":"TASK_STATE_WORKING","status_revision":9},
            "managed_run":{"classification":"managed_custom","cancel_available":false},
            "result":{"body":"Tests pass according to worker", "truncated":false}
        }))
    })
    .await;
    let report = client(&endpoint).result("d-1").await.unwrap();
    assert_eq!(report.state, RunState::Working);
    assert_eq!(report.status.revision, Some(9));
    assert_eq!(report.task_state, RunState::Failed);
    assert_eq!(report.state_basis.as_deref(), Some("run_stale_timeout"));
    assert_eq!(report.managed_run.unwrap()["cancel_available"], false);
}

#[tokio::test]
async fn mismatched_dispatch_receipts_are_not_admitted() {
    let (_, endpoint) = serve(|tool, _| if tool == TACHI_STAFF_TOOL {
        Reply::Ok(json!({"dispatch_id":"wrong", "state":"TASK_STATE_COMPLETED"}))
    } else {
        Reply::Ok(json!({"state":"TASK_STATE_COMPLETED", "run_status":{"dispatch_id":"wrong", "state":"TASK_STATE_COMPLETED"}, "result":{"body":"success"}}))
    }).await;
    let client = client(&endpoint);
    assert!(matches!(
        client.status("d-1").await.unwrap_err(),
        TachiStaffError::Protocol(_)
    ));
    assert!(matches!(
        client.result("d-1").await.unwrap_err(),
        TachiStaffError::Protocol(_)
    ));
}

#[tokio::test]
async fn bounded_watch_returns_last_observation_without_claiming_completion() {
    let (fake, endpoint) =
        serve(|_, _| Reply::Ok(json!({"dispatch_id":"d-1", "state":"TASK_STATE_WORKING"}))).await;
    let client = client(&endpoint);
    client.status("d-1").await.unwrap(); // isolate polling from session setup
    let calls_before = fake.calls().len();
    let status = client
        .wait_until_terminal("d-1", std::time::Duration::from_secs(1))
        .await
        .unwrap();
    assert!(!status.is_terminal());
    assert_eq!(fake.calls().len() - calls_before, 1);
}

fn production_config(temp: &tempfile::TempDir, endpoint: &str) -> zeroclaw_config::schema::Config {
    use zeroclaw_config::schema::{AliasedAgentConfig, RiskProfileConfig};
    let mut cfg = zeroclaw_config::schema::Config {
        data_dir: temp.path().join("data"),
        tachi: config(endpoint),
        composition: Some(zeroclaw_config::composition::Composition::Minimal),
        ..Default::default()
    };
    cfg.agents.insert(
        "home".into(),
        AliasedAgentConfig {
            risk_profile: "delegate".into(),
            ..Default::default()
        },
    );
    cfg.risk_profiles.insert(
        "delegate".into(),
        RiskProfileConfig {
            level: zeroclaw_config::autonomy::AutonomyLevel::Full,
            sandbox_enabled: Some(false),
            ..Default::default()
        },
    );
    cfg
}

fn production_tools(
    cfg: &zeroclaw_config::schema::Config,
    live: Arc<parking_lot::RwLock<zeroclaw_config::schema::Config>>,
) -> Vec<Box<dyn zeroclaw_api::tool::Tool>> {
    let security =
        Arc::new(zeroclaw_config::policy::SecurityPolicy::for_agent(cfg, "home").unwrap());
    crate::tools::all_tools_with_runtime(
        Arc::new(cfg.clone()),
        &security,
        cfg.risk_profile_for_agent("home").unwrap(),
        "home",
        Arc::new(crate::platform::NativeRuntime::new()),
        Arc::new(zeroclaw_memory::NoneMemory::new("none")),
        None,
        None,
        &cfg.browser,
        &cfg.http_request,
        &cfg.web_fetch,
        &cfg.agent_workspace_dir("home"),
        &cfg.agents,
        None,
        cfg,
        false,
        None,
        Some(live),
        None,
    )
    .tools
}

async fn invoke(
    tools: &[Box<dyn zeroclaw_api::tool::Tool>],
    name: &str,
    args: Value,
) -> zeroclaw_api::tool::ToolResult {
    tools
        .iter()
        .find(|tool| tool.name() == name)
        .expect("registered delegation tool")
        .execute(args)
        .await
        .unwrap()
}

fn start_args(id: &str) -> Value {
    json!({"request_id":id,"harness":"codex","task":"Review the Codex adapter on GitHub; compare these together", "staffing_reason":"explicit_user_request"})
}

#[tokio::test]
async fn production_registry_starts_once_and_reads_controls_through_same_request_binding() {
    let (fake, endpoint) = serve(|tool, args| match (tool, args["action"].as_str()) {
        (TACHI_STAFF_TOOL, Some("start")) => Reply::Ok(working_receipt("d-prod")),
        (TACHI_STAFF_TOOL, Some("status")) => Reply::Ok(json!({"dispatch_id":"d-prod", "state":"TASK_STATE_COMPLETED", "status_revision":4})),
        (TACHI_STAFF_TOOL, Some("cancel")) => {
            assert_eq!(args["expected_status_revision"], 4);
            Reply::Ok(json!({"receipt":"cancellation_requested", "state":"TASK_STATE_WORKING"}))
        }
        (TACHI_TASK_TOOL, _) => Reply::Ok(json!({"state":"TASK_STATE_COMPLETED", "run_status":{"dispatch_id":"d-prod","state":"TASK_STATE_COMPLETED","status_revision":4}, "result":{"body":"Worker report"}})),
        _ => panic!("unexpected call {tool} {args}")
    }).await;
    let temp = tempfile::TempDir::new().unwrap();
    let cfg = production_config(&temp, &endpoint);
    let protected_dir = cfg.agent_workspace_dir("home");
    std::fs::create_dir_all(&protected_dir).unwrap();
    std::fs::write(
        protected_dir.join("SOUL.md"),
        "protected-soul-fixture-bytes",
    )
    .unwrap();
    std::fs::write(
        protected_dir.join("USER.md"),
        "protected-user-model-fixture-bytes",
    )
    .unwrap();
    let live = Arc::new(parking_lot::RwLock::new(cfg.clone()));
    let tools = production_tools(&cfg, live.clone());
    let started = invoke(&tools, "tachi_start", start_args("req-prod")).await;
    assert!(started.success, "{:?}", started.error);
    assert_eq!(
        started.output.data().unwrap()["receipt"]["dispatch_id"],
        "d-prod"
    );
    drop(tools);
    let tools = production_tools(&cfg, live);
    let replay = invoke(&tools, "tachi_start", start_args("req-prod")).await;
    assert_eq!(replay.output.data().unwrap()["replayed"], true);
    assert_eq!(
        fake.calls()
            .iter()
            .filter(|(_, args)| args["action"] == "start")
            .count(),
        1
    );
    let status = invoke(&tools, "tachi_status", json!({"request_id":"req-prod"})).await;
    assert_eq!(
        status.output.data().unwrap()["status"]["status_revision"],
        4
    );
    let result = invoke(&tools, "tachi_result", json!({"request_id":"req-prod"})).await;
    assert_eq!(result.output.data().unwrap()["accepted_by_body"], false);
    assert_eq!(
        result.output.data().unwrap()["trust"],
        "untrusted_external_report"
    );
    let watch = invoke(
        &tools,
        "tachi_watch",
        json!({"request_id":"req-prod","max_wait_secs":1}),
    )
    .await;
    assert_eq!(watch.output.data().unwrap()["terminal"], true);
    let cancel = invoke(
        &tools,
        "tachi_cancel",
        json!({"request_id":"req-prod","expected_status_revision":4}),
    )
    .await;
    assert_eq!(
        cancel.output.data().unwrap()["receipt"]["outcome"],
        "requested"
    );
    let starts = fake.calls();
    assert!(starts[0].1.to_string().contains("Codex adapter on GitHub"));
    for (tool, args) in starts {
        let wire = args.to_string();
        for protected in [
            "protected-soul-fixture-bytes",
            "protected-user-model-fixture-bytes",
        ] {
            assert!(!wire.contains(protected));
        }
        if tool == TACHI_STAFF_TOOL && args["action"] == "start" {
            for denied in [
                "worker",
                "command",
                "cwd",
                "credentials",
                "sandbox",
                "allowed_tools",
                "soul",
                "user_model",
                "history",
            ] {
                assert!(args.get(denied).is_none(), "{denied}");
            }
        }
    }
}

#[tokio::test]
async fn production_unsent_initialize_failure_releases_claim_and_same_id_can_retry() {
    let (fake, endpoint) = serve(|_, _| Reply::Ok(working_receipt("d-after-outage"))).await;
    let temp = tempfile::TempDir::new().unwrap();
    let cfg = production_config(&temp, &endpoint);
    let tools = production_tools(&cfg, Arc::new(parking_lot::RwLock::new(cfg.clone())));
    fake.fail_next_initialize.store(true, Ordering::SeqCst);
    let failed = invoke(&tools, "tachi_start", start_args("same-id")).await;
    assert_eq!(failed.output.data().unwrap()["code"], "unavailable");
    assert!(fake.calls().is_empty());
    let conn = rusqlite::Connection::open(cfg.data_dir.join("sessions/sessions.db")).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_delegations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 0, "proven unsent start must not strand its request");

    let retry = invoke(&tools, "tachi_start", start_args("same-id")).await;
    assert_eq!(
        retry.output.data().unwrap()["receipt"]["dispatch_id"],
        "d-after-outage"
    );
    assert_eq!(fake.calls().len(), 1);
    let replay = invoke(&tools, "tachi_start", start_args("same-id")).await;
    assert_eq!(replay.output.data().unwrap()["replayed"], true);
    assert_eq!(fake.calls().len(), 1);
}

#[tokio::test]
async fn production_task_and_refs_reject_execution_and_private_content_before_claim() {
    let (fake, endpoint) = serve(|_, _| panic!("forbidden content must never reach Tachi")).await;
    let temp = tempfile::TempDir::new().unwrap();
    let cfg = production_config(&temp, &endpoint);
    for text in [
        "run in /Users/example/worktrees/change",
        "bash build-script",
        "use cwd chosen by this prompt",
        "run using tmux",
        "reach the host via SSH",
        "skip the sandbox",
        "pass --full-auto",
        "api_key=fixture-only",
        "include the private-dyad identity",
    ] {
        for field in ["task", "issue_ref", "pr_ref", "flow_id"] {
            // Each case has a fresh action tracker so rate limiting cannot
            // mask a broken admission scanner on later cases.
            let tools = production_tools(&cfg, Arc::new(parking_lot::RwLock::new(cfg.clone())));
            let mut args = start_args("forbidden");
            args[field] = json!(text);
            let result = invoke(&tools, "tachi_start", args).await;
            assert_eq!(
                result.output.data().unwrap()["code"],
                "forbidden_content",
                "{field}: {text}"
            );
        }
    }
    let tools = production_tools(&cfg, Arc::new(parking_lot::RwLock::new(cfg.clone())));
    let result = invoke(&tools, "tachi_start", start_args("ghp_fixture-only")).await;
    assert_eq!(result.output.data().unwrap()["code"], "forbidden_content");
    let conn = rusqlite::Connection::open(cfg.data_dir.join("sessions/sessions.db")).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_delegations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
    assert!(fake.calls().is_empty());
    assert_eq!(fake.sessions_opened.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn production_unknown_submit_survives_registry_restart_and_is_never_reissued() {
    let (fake, endpoint) = serve(|_, _| Reply::Http(StatusCode::NOT_FOUND)).await;
    let temp = tempfile::TempDir::new().unwrap();
    let cfg = production_config(&temp, &endpoint);
    let live = Arc::new(parking_lot::RwLock::new(cfg.clone()));
    let tools = production_tools(&cfg, live.clone());
    let first = invoke(&tools, "tachi_start", start_args("lost-response")).await;
    assert_eq!(first.output.data().unwrap()["code"], "submission_unknown");
    drop(tools);
    let tools = production_tools(&cfg, live);
    let replay = invoke(&tools, "tachi_start", start_args("lost-response")).await;
    assert_eq!(
        replay.output.data().unwrap()["code"],
        "submission_unresolved"
    );
    assert_eq!(fake.calls().len(), 1);
    let read = invoke(
        &tools,
        "tachi_status",
        json!({"request_id":"lost-response"}),
    )
    .await;
    assert_eq!(read.output.data().unwrap()["code"], "submission_unresolved");
    assert_eq!(fake.calls().len(), 1);
}

#[tokio::test]
async fn production_closed_unknown_harness_and_raw_authority_have_zero_external_side_effects() {
    let (fake, endpoint) = serve(|_, _| panic!("must not reach Tachi")).await;
    let temp = tempfile::TempDir::new().unwrap();
    let cfg = production_config(&temp, &endpoint);
    let live = Arc::new(parking_lot::RwLock::new(cfg.clone()));
    let tools = production_tools(&cfg, live.clone());
    let mut unknown = start_args("unknown");
    unknown["harness"] = json!("unregistered");
    assert_eq!(
        invoke(&tools, "tachi_start", unknown)
            .await
            .output
            .data()
            .unwrap()["code"],
        "unknown_harness"
    );
    for field in [
        "command",
        "cwd",
        "credentials",
        "worker",
        "allowed_tools",
        "sandbox",
        "soul",
        "user_model",
        "history",
    ] {
        let mut raw = start_args("hostile");
        raw[field] = json!("untrusted");
        assert_eq!(
            invoke(&tools, "tachi_start", raw)
                .await
                .output
                .data()
                .unwrap()["code"],
            "invalid_arguments"
        );
    }
    let store = zeroclaw_infra::session_sqlite::SqliteSessionBackend::new(&cfg.data_dir).unwrap();
    // Other registered session tools may have opened this DB; admission must
    // leave no claim rather than pretending registry construction is I/O-free.
    let conn = rusqlite::Connection::open(cfg.data_dir.join("sessions/sessions.db")).unwrap();
    let count: i64 = conn
        .query_row("SELECT count(*) FROM session_delegations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
    drop(store);
    live.write().tachi.enabled = false;
    assert_eq!(
        invoke(&tools, "tachi_start", start_args("closed"))
            .await
            .output
            .data()
            .unwrap()["code"],
        "unavailable"
    );
    assert_eq!(fake.sessions_opened.load(Ordering::SeqCst), 0);
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn production_status_and_watch_preserve_orphaned_projection_without_terminal_promotion() {
    let projection = json!({
        "execution_state":"orphaned",
        "control_state":"unavailable",
        "outcome_state":"unknown",
        "controller_epoch_id":"previous-epoch",
        "current_controller_epoch_id":"current-epoch",
        "reconciliation":{"verdict":"orphaned"},
        "artifacts_available":{"result.md":false}
    });
    let expected = projection.clone();
    let (_, endpoint) = serve(move |_, args| match args["action"].as_str() {
        Some("start") => Reply::Ok(working_receipt("d-orphaned")),
        Some("status") => Reply::Ok(json!({
            "dispatch_id":"d-orphaned",
            "state":"TASK_STATE_WORKING",
            "status_revision":4,
            "read_projection":projection
        })),
        _ => panic!("unexpected call"),
    })
    .await;
    let temp = tempfile::TempDir::new().unwrap();
    let cfg = production_config(&temp, &endpoint);
    let tools = production_tools(&cfg, Arc::new(parking_lot::RwLock::new(cfg.clone())));
    let started = invoke(&tools, "tachi_start", start_args("orphaned")).await;
    assert_eq!(started.output.data().unwrap()["accepted"], true);
    for name in ["tachi_status", "tachi_watch"] {
        let args = if name == "tachi_watch" {
            json!({"request_id":"orphaned","max_wait_secs":1})
        } else {
            json!({"request_id":"orphaned"})
        };
        let result = invoke(&tools, name, args).await;
        let data = result.output.data().unwrap();
        assert_eq!(data["status"]["state"], "TASK_STATE_WORKING");
        assert_eq!(data["status"]["read_projection"], expected);
        if name == "tachi_watch" {
            assert_eq!(data["terminal"], false);
        }
    }
}

#[tokio::test]
async fn live_agent_construction_rechecks_delegation_policy_after_reload() {
    use zeroclaw_config::multi_agent::MemoryBackendKind;
    use zeroclaw_config::schema::{ModelProviderConfig, OllamaModelProviderConfig};

    let (fake, endpoint) = serve(|_, _| panic!("revoked permission must not reach Tachi")).await;
    let temp = tempfile::TempDir::new().unwrap();
    let mut cfg = production_config(&temp, &endpoint);
    cfg.config_path = temp.path().join("config.toml");
    cfg.providers.models.ollama.insert(
        "offline".into(),
        OllamaModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("fixture-model".into()),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let agent_cfg = cfg.agents.get_mut("home").unwrap();
    agent_cfg.model_provider = "ollama.offline".into();
    agent_cfg.memory.backend = MemoryBackendKind::None;
    let live = Arc::new(parking_lot::RwLock::new(cfg));
    let agent = crate::agent::Agent::from_live_config_with_session_cwd_and_mcp_backchannel(
        live.clone(),
        "home",
        None,
        false,
        true,
    )
    .await
    .unwrap();
    assert!(agent.tool_names().contains(&"tachi_start"));

    live.write()
        .risk_profiles
        .get_mut("delegate")
        .unwrap()
        .level = zeroclaw_config::autonomy::AutonomyLevel::ReadOnly;
    let denied = agent
        .execute_tool_for_test("tachi_start", start_args("real-agent-revoked"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(denied.output.data().unwrap()["code"], "denied");
    assert!(fake.calls().is_empty());
    assert_eq!(fake.sessions_opened.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn production_live_policy_and_profile_revocations_apply_before_submission() {
    let (fake, endpoint) = serve(|_, _| panic!("must not reach Tachi")).await;
    let temp = tempfile::TempDir::new().unwrap();
    let cfg = production_config(&temp, &endpoint);
    let live = Arc::new(parking_lot::RwLock::new(cfg.clone()));
    let tools = production_tools(&cfg, live.clone());
    live.write().tachi.harnesses.clear();
    assert_eq!(
        invoke(&tools, "tachi_start", start_args("revoked-profile"))
            .await
            .output
            .data()
            .unwrap()["code"],
        "unknown_harness"
    );
    live.write().tachi.harnesses = cfg.tachi.harnesses.clone();
    live.write()
        .risk_profiles
        .get_mut("delegate")
        .unwrap()
        .level = zeroclaw_config::autonomy::AutonomyLevel::ReadOnly;
    assert_eq!(
        invoke(&tools, "tachi_start", start_args("read-only"))
            .await
            .output
            .data()
            .unwrap()["code"],
        "denied"
    );
    {
        let mut changed = live.write();
        changed.risk_profiles.get_mut("delegate").unwrap().level =
            zeroclaw_config::autonomy::AutonomyLevel::Full;
        changed
            .risk_profiles
            .get_mut("delegate")
            .unwrap()
            .allowed_tools = Some(vec![]);
    }
    assert_eq!(
        invoke(&tools, "tachi_start", start_args("denied"))
            .await
            .output
            .data()
            .unwrap()["code"],
        "denied"
    );
    assert!(fake.calls().is_empty());
    assert_eq!(fake.sessions_opened.load(Ordering::SeqCst), 0);
}

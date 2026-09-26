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
            "run_status": {},
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

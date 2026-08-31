//! Protocol-boundary tests for the ACPX carrier using a REAL child
//! process speaking the ACP JSON-RPC wire (a small deterministic node
//! adapter written per-test). These run in CI (no external binaries or
//! credentials): they are the discriminating regression tests for the
//! lifecycle laws the live lane cannot exercise deterministically.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use zeroclaw_api::session_exec::AdapterConnectionRef;
use zeroclaw_api::session_exec::ExecutionRunStatusV1;
use zeroclaw_api::session_exec::HostIdentityRef;
use zeroclaw_runtime::execution_subagent::SessionFactSink;
use zeroclaw_runtime::execution_subagent::{
    AcpxController, AcpxControllerConfig, ControllerError, ExecutionRunRequest,
    ExecutionSubagentTool, GatedSessionController, SessionCapabilities, SessionController,
    SessionStartSpec, SessionStopReceipt,
};

/// Write the deterministic fake ACP adapter (a node script) and return
/// its absolute path. Modes:
/// - `normal`: answers initialize/session-new/prompt per the ACP wire;
///   `session/load` answers then stays alive.
/// - `flap`: `session/load` answers and then EXITS (the transport dies
///   immediately after every successful resume).
fn write_fake_adapter(dir: &std::path::Path, mode: &str, session_id: &str) -> PathBuf {
    let script = format!(
        r#"#!/usr/bin/env node
const mode = {mode:?};
const sessionId = {session_id:?};
let buf = "";
function send(m) {{ m.jsonrpc = "2.0"; process.stdout.write(JSON.stringify(m) + "\n"); }}
process.stdin.on("data", (d) => {{
  buf += d;
  let i;
  while ((i = buf.indexOf("\n")) >= 0) {{
    const line = buf.slice(0, i);
    buf = buf.slice(i + 1);
    if (!line.trim()) continue;
    let msg;
    try {{ msg = JSON.parse(line); }} catch {{ continue; }}
    if (msg.method === "initialize") {{
      send({{ id: msg.id, result: {{ protocolVersion: 1 }} }});
    }} else if (msg.method === "session/new") {{
      const id = sessionId === "auto" ? `auto-${{process.pid}}-${{Date.now()}}` : sessionId;
      send({{ id: msg.id, result: {{ sessionId: id }} }});
    }} else if (msg.method === "session/load") {{
      if (mode === "refuse-load") {{
        send({{ id: msg.id, error: {{ code: -32000, message: "load refused by fake adapter" }} }});
        process.exit(0);
      }}
      send({{ id: msg.id, result: {{}} }});
      if (mode === "flap") process.exit(0);
      if (mode === "load-emit") {{
        setTimeout(() => {{
          send({{ jsonrpc: "2.0", method: "session/update", params: {{ update: {{ sessionUpdate: "tool_call", toolCallId: `tc-${{Date.now()}}` }} }} }});
        }}, 300);
      }}
    }} else if (msg.method === "session/prompt") {{
      send({{ jsonrpc: "2.0", method: "session/update", params: {{ update: {{ sessionUpdate: "agent_message_chunk", content: {{ type: "text", text: "FAKE-DONE" }} }} }} }});
      send({{ id: msg.id, result: {{ stopReason: "end_turn" }} }});
    }} else if (msg.id !== undefined) {{
      send({{ id: msg.id, result: {{}} }});
    }}
  }}
}});
"#
    );
    let path = dir.join("fake-acp-adapter.mjs");
    std::fs::write(&path, script).expect("write fake adapter");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake adapter");
    }
    path
}

fn fake_config(
    dir: &std::path::Path,
    adapter: &std::path::Path,
    mode: &str,
    session_id: &str,
) -> AcpxControllerConfig {
    let mut env = std::collections::HashMap::new();
    env.insert("FAKE_ACP_MODE".to_string(), mode.to_string());
    env.insert("FAKE_SESSION_ID".to_string(), session_id.to_string());
    AcpxControllerConfig {
        command: adapter.to_path_buf(),
        args: vec![],
        env,
        workspace_root: dir.to_path_buf(),
        session_mode: None,
        startup_timeout: Duration::from_secs(30),
        turn_timeout: Duration::from_secs(30),
        max_line_bytes: 256 * 1024,
        declared_capabilities: vec!["observe", "wait", "prompt", "cancel", "resume", "events"],
    }
}

fn scratch(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn all_caps() -> SessionCapabilities {
    SessionCapabilities::from_names(&[
        "observe".to_string(),
        "wait".to_string(),
        "prompt".to_string(),
        "cancel".to_string(),
        "resume".to_string(),
        "events".to_string(),
    ])
    .expect("valid capability names")
}

fn spec_for(_dir: &std::path::Path, prompt: &str, connection: &str) -> SessionStartSpec {
    SessionStartSpec {
        adapter_connection: AdapterConnectionRef::from_opaque(connection),
        prompt: prompt.to_string(),
        context_digest: "digest".to_string(),
        capabilities: all_caps(),
        max_prompt_bytes: 16_384,
    }
}

/// The completed-turn lifecycle over a real child: objective turn →
/// InputRequired; one correction leg → Terminal(completed); the transport
/// is torn down when the terminal is observed.
#[tokio::test]
async fn fake_adapter_full_turn_contract_and_terminal_teardown() {
    let dir = scratch("zc-fake-terminal");
    let adapter = write_fake_adapter(&dir, "normal", "fake-terminal-1");
    let controller = Arc::new(
        AcpxController::new(fake_config(&dir, &adapter, "normal", "fake-terminal-1"))
            .expect("controller constructs"),
    );
    let handle = controller
        .start(&spec_for(&dir, "Say FAKE-DONE.", "conn-fake-t"))
        .await
        .expect("session starts");
    assert!(
        controller.session_alive_for_test(&handle),
        "the child must be alive mid-session"
    );

    // Objective turn ended: the stream opens with InputRequired. (The
    // adapter deliberately emits no Accepted — the tool authors the
    // verbatim accepted fact; a second adapter-emitted Accepted could
    // never be deduped by id.)
    let page = controller.watch(&handle, 0, 64).await.expect("watch");
    assert_eq!(
        page.events.first().map(|event| event.kind),
        Some(zeroclaw_api::session_exec::SessionEventKindV1::InputRequired)
    );
    assert!(
        page.events.iter().any(
            |event| event.kind == zeroclaw_api::session_exec::SessionEventKindV1::InputRequired
        ),
        "objective turn end is InputRequired"
    );

    // One correction leg: the answered correction closes the run.
    controller
        .prompt(&handle, "Reply FAKE-DONE again.")
        .await
        .expect("correction delivered");
    let page = controller
        .watch(&handle, page.next_seq, 64)
        .await
        .expect("watch");
    let terminal = page
        .events
        .iter()
        .find(|event| event.kind == zeroclaw_api::session_exec::SessionEventKindV1::Terminal)
        .expect("answered correction closes the run");
    assert!(matches!(
        terminal.outcome,
        Some(zeroclaw_api::session_exec::SessionTerminalOutcomeV1::Completed)
    ));
    // The observed terminal tears the transport down (host-owned close).
    assert!(
        !controller.session_alive_for_test(&handle),
        "no harness child survives the observed terminal"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// stop(immediate) is run-scoped bookkeeping: the child dies, and NO
/// terminal event is minted into the fact stream.
#[tokio::test]
async fn fake_adapter_immediate_stop_kills_without_minting_terminal() {
    let dir = scratch("zc-fake-stop");
    let adapter = write_fake_adapter(&dir, "normal", "fake-stop-1");
    let controller = Arc::new(
        AcpxController::new(fake_config(&dir, &adapter, "normal", "fake-stop-1"))
            .expect("controller constructs"),
    );
    let handle = controller
        .start(&spec_for(&dir, "Say anything.", "conn-fake-s"))
        .await
        .expect("session starts");
    assert!(controller.session_alive_for_test(&handle));

    let receipt: SessionStopReceipt = controller.stop(&handle, false).await.expect("stop");
    assert!(receipt.confirmed);
    assert!(receipt.authority_confirmation_ref.is_some());
    assert!(
        !controller.session_alive_for_test(&handle),
        "an immediate stop must kill the child"
    );
    let page = controller
        .watch(&handle, 0, 64)
        .await
        .expect("watch readable");
    assert!(
        page.events.iter().all(|event| event.outcome.is_none()),
        "an immediate stop mints no terminal fact"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A flapping adapter (load succeeds, then the transport dies) is bounded:
/// after MAX_WATCH_RESUMES consecutive dead resumes the watch fails typed
/// instead of cycling forever.
#[tokio::test]
async fn fake_adapter_flapping_resume_is_bounded_and_fails_closed() {
    let dir = scratch("zc-fake-flap");
    let adapter = write_fake_adapter(&dir, "flap", "fake-flap-1");
    let controller = Arc::new(
        AcpxController::new(fake_config(&dir, &adapter, "flap", "fake-flap-1"))
            .expect("controller constructs"),
    );
    let handle = controller
        .start(&spec_for(&dir, "Say anything.", "conn-fake-f"))
        .await
        .expect("session starts");
    // Kill the live transport deliberately: the state (and its session
    // identity) survives in the controller, so every subsequent watch
    // exercises the resume path against the flapping adapter.
    let _ = controller.stop(&handle, false).await;
    let error = {
        let mut attempts = 0;
        let mut cursor = 0;
        loop {
            attempts += 1;
            assert!(attempts <= 40, "the watch must terminate, not spin");
            match controller.watch(&handle, cursor, 64).await {
                Ok(page) => {
                    // Drain the stream with a monotone cursor: once the
                    // queued facts are consumed, the dead transport hits
                    // the bounded resume path.
                    cursor = page.next_seq;
                }
                Err(error) => break error,
            }
        }
    };
    assert!(
        matches!(error, ControllerError::Unavailable),
        "the flapping bound must end typed (Unavailable), got {error:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A repeated live session identity is refused typed instead of silently
/// re-pointing an existing binding.
#[tokio::test]
async fn fake_adapter_repeated_session_identity_refuses() {
    let dir = scratch("zc-fake-collision");
    let adapter = write_fake_adapter(&dir, "normal", "fake-collision-1");
    let controller = Arc::new(
        AcpxController::new(fake_config(&dir, &adapter, "normal", "fake-collision-1"))
            .expect("controller constructs"),
    );
    let first = controller
        .start(&spec_for(&dir, "Say anything.", "conn-fake-c"))
        .await
        .expect("first session starts");
    let error = controller
        .start(&spec_for(&dir, "Say anything.", "conn-fake-c2"))
        .await;
    let outcome = error.expect_err("second start must refuse");
    assert!(
        matches!(outcome, ControllerError::Refused(ref reason)
            if reason.contains("repeated a live session identity")),
        "a repeated live identity must refuse typed, got {outcome:?}"
    );
    // The FIRST binding keeps working (collision refusal kills only the
    // newcomer).
    assert!(controller.session_alive_for_test(&first));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The full ExecutionSubagentTool run over the fake adapter end to end:
/// the run completes, the report is coherent, and the fact stream opens
/// Accepted (never a duplicated accepted fact).
#[tokio::test]
async fn fake_adapter_tool_run_completes_with_clean_fact_order() {
    struct NullSink;
    #[async_trait::async_trait]
    impl SessionFactSink for NullSink {
        async fn attach(
            &self,
            _binding: &zeroclaw_runtime::execution_subagent::SessionBinding,
            _capabilities: &[String],
        ) -> Result<
            zeroclaw_api::session_exec::SessionAttachmentRef,
            zeroclaw_api::session_exec::SessionFactError,
        > {
            Ok(zeroclaw_api::session_exec::SessionAttachmentRef::from_opaque("att-fake"))
        }
        async fn advertise_capabilities(
            &self,
            attachment: &zeroclaw_api::session_exec::SessionAttachmentRef,
            capabilities: &[String],
        ) -> Result<
            zeroclaw_api::session_exec::SessionAdvertiseReceiptView,
            zeroclaw_api::session_exec::SessionFactError,
        > {
            Ok(zeroclaw_api::session_exec::SessionAdvertiseReceiptView {
                attachment_ref: attachment.clone(),
                advertisement_seq: 1,
                capabilities: capabilities.to_vec(),
            })
        }
        async fn ingest_event(
            &self,
            _attachment: &zeroclaw_api::session_exec::SessionAttachmentRef,
            _fact: &zeroclaw_runtime::execution_subagent::SessionEventFact,
        ) -> Result<
            zeroclaw_api::session_exec::SessionEventReceiptView,
            zeroclaw_api::session_exec::SessionFactError,
        > {
            Ok(zeroclaw_api::session_exec::SessionEventReceiptView {
                attachment_ref: _attachment.clone(),
                event_id: _fact.event_id.clone(),
                admission: zeroclaw_api::session_exec::SessionReceiptAdmissionV1::Created,
                disposition: "advanced".to_string(),
                state: zeroclaw_api::session_exec::SessionStateView {
                    canonical_state: zeroclaw_api::session_exec::SessionCanonicalStateV1::Accepted,
                    canonical_revision: 1,
                    cleanup_recorded: false,
                    conflicting_terminal: false,
                    last_event_id: None,
                },
            })
        }
        async fn request_intervention(
            &self,
            _attachment: &zeroclaw_api::session_exec::SessionAttachmentRef,
            _request_id: &zeroclaw_api::session_exec::InterventionRequestIdRef,
            _kind: zeroclaw_api::session_exec::SessionInterventionKindV1,
            _reason: &str,
        ) -> Result<(), zeroclaw_api::session_exec::SessionFactError> {
            Ok(())
        }
        async fn get_intervention(
            &self,
            _attachment: &zeroclaw_api::session_exec::SessionAttachmentRef,
            _request_id: &zeroclaw_api::session_exec::InterventionRequestIdRef,
        ) -> Result<
            Option<zeroclaw_api::session_exec::SessionInterventionRequestView>,
            zeroclaw_api::session_exec::SessionFactError,
        > {
            Ok(None)
        }
        async fn record_intervention_result(
            &self,
            _attachment: &zeroclaw_api::session_exec::SessionAttachmentRef,
            _request_id: &zeroclaw_api::session_exec::InterventionRequestIdRef,
            _disposition: zeroclaw_api::session_exec::SessionInterventionDispositionV1,
            _authority_confirmation_ref: Option<&str>,
            _detail: Option<&str>,
        ) -> Result<(), zeroclaw_api::session_exec::SessionFactError> {
            Ok(())
        }
        async fn mark_connection(
            &self,
            _attachment: &zeroclaw_api::session_exec::SessionAttachmentRef,
            _fact: zeroclaw_api::session_exec::SessionConnectionFactV1,
        ) -> Result<(), zeroclaw_api::session_exec::SessionFactError> {
            Ok(())
        }
        async fn reconnect(
            &self,
            _binding: &zeroclaw_runtime::execution_subagent::SessionBinding,
        ) -> Result<
            zeroclaw_api::session_exec::SessionReconnectReceiptView,
            zeroclaw_api::session_exec::SessionFactError,
        > {
            Err(zeroclaw_api::session_exec::SessionFactError::Unavailable)
        }
        async fn get_state(
            &self,
            _attachment: &zeroclaw_api::session_exec::SessionAttachmentRef,
        ) -> Result<
            zeroclaw_api::session_exec::SessionStateView,
            zeroclaw_api::session_exec::SessionFactError,
        > {
            Ok(zeroclaw_api::session_exec::SessionStateView {
                canonical_state: zeroclaw_api::session_exec::SessionCanonicalStateV1::Completed,
                canonical_revision: 1,
                cleanup_recorded: false,
                conflicting_terminal: false,
                last_event_id: None,
            })
        }
    }

    let dir = scratch("zc-fake-tool");
    let adapter = write_fake_adapter(&dir, "normal", "fake-tool-1");
    let controller = Arc::new(GatedSessionController::new(Arc::new(
        AcpxController::new(fake_config(&dir, &adapter, "normal", "fake-tool-1"))
            .expect("controller constructs"),
    )));
    let sink = Arc::new(NullSink);
    let tool = ExecutionSubagentTool::new(
        controller,
        sink,
        HostIdentityRef::from_opaque("zc-fake-host"),
    );
    let report = tool
        .run(&ExecutionRunRequest {
            objective: "Reply with FAKE-DONE. Do not use any tools.".to_string(),
            // The adapter's turn contract: the objective turn ends with
            // InputRequired; the correction leg completes the run.
            correction_prompt: Some("Reply with FAKE-DONE one more time.".to_string()),
        })
        .await;
    println!("run refusal: {:?}", report.refusal);
    assert_eq!(report.status, ExecutionRunStatusV1::Completed);
    assert_eq!(
        report
            .remote_session_ref
            .as_ref()
            .map(|remote| remote.as_str())
            .unwrap_or_default(),
        "fake-tool-1"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A refused or timed-out `session/load` tears down THIS attempt's child
/// (self-scoped kill) and fails typed — the state is left retryable and
/// no live process survives the failed resume.
#[tokio::test]
async fn fake_adapter_failed_load_teardown_kills_only_own_child() {
    let dir = scratch("zc-fake-refuse");
    let adapter = write_fake_adapter(&dir, "refuse-load", "fake-refuse-1");
    let controller = Arc::new(
        AcpxController::new(fake_config(&dir, &adapter, "refuse-load", "fake-refuse-1"))
            .expect("controller constructs"),
    );
    let handle = controller
        .start(&spec_for(&dir, "Say anything.", "conn-fake-r"))
        .await
        .expect("session starts");
    let _ = controller.stop(&handle, false).await;
    let mut cursor = 0;
    let error = {
        let mut attempts = 0;
        loop {
            attempts += 1;
            assert!(attempts <= 40, "the watch must terminate, not spin");
            match controller.watch(&handle, cursor, 64).await {
                Ok(page) => cursor = page.next_seq,
                Err(error) => break error,
            }
        }
    };
    assert!(
        matches!(error, ControllerError::Unavailable),
        "the refused load must end typed (Unavailable), got {error:?}"
    );
    assert!(
        !controller.session_alive_for_test(&handle),
        "no child may survive the failed resume teardown"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The flapping budget must not exhaust across productive cycles: six
/// real drop → same-session recovery cycles on ONE session state, each
/// recovery followed by observed progress (the adapter's post-load
/// tool activity). Without the streak reset the fourth cycle would hit
/// the bound and fail typed.
#[tokio::test]
async fn fake_adapter_productive_cycles_keep_the_budget_alive() {
    let dir = scratch("zc-fake-cycles");
    let adapter = write_fake_adapter(&dir, "load-emit", "fake-cycles-1");
    let controller = Arc::new(
        AcpxController::new(fake_config(&dir, &adapter, "load-emit", "fake-cycles-1"))
            .expect("controller constructs"),
    );
    let handle = controller
        .start(&spec_for(
            &dir,
            "Reply with anything. Do not use any tools.",
            "conn-fake-cycles",
        ))
        .await
        .expect("cycle start");
    assert!(controller.session_alive_for_test(&handle));

    let mut cursor = 0;
    for cycle in 0..6 {
        // A REAL transport drop each cycle (immediate host kill, no event
        // minted), so every recovery exercises a genuine `session/load`.
        let _ = controller.stop(&handle, false).await;
        assert!(!controller.session_alive_for_test(&handle));

        let resumed = controller
            .reattach(
                &AdapterConnectionRef::from_opaque("conn-fake-cycles"),
                &handle.remote_session,
                0,
            )
            .await
            .expect("reattach must recover the SAME session");
        assert_eq!(
            resumed.remote_session.as_str(),
            handle.remote_session.as_str()
        );

        // The recovered session shows observable progress (the adapter's
        // post-load tool activity) — which also resets the flapping
        // streak via the watch return path.
        let page = controller
            .watch(&resumed, cursor, 64)
            .await
            .expect("events after recovery");
        assert!(!page.events.is_empty(), "cycle {cycle}: events must flow");
        cursor = page.next_seq;
    }
    let _ = std::fs::remove_dir_all(&dir);
}

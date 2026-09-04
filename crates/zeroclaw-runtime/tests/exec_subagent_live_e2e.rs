//! LIVE acceptance lane for the A2 vertical (real ACPX carrier + real
//! tachi facade transport). Every test here is `#[ignore]`-gated AND
//! requires `ZC_A2_LIVE=1` plus explicit binary paths, so CI never
//! spawns real harness processes. Run shape:
//!
//! ```text
//! ZC_A2_LIVE=1 \
//! ZC_A2_ACPX_BIN=$HOME/.npm-global/bin/codex-acp \
//! ZC_A2_ACPX_ENV_JSON='{"CODEX_HOME":"/path/to/scratch-codex-home"}' \
//! ZC_A2_SESSION_MODE=auto \
//! cargo test -p zeroclaw-runtime --test exec_subagent_live_e2e -- --ignored --nocapture
//! ```
//!
//! The spine-leg scenarios additionally take the tachi binding env vars
//! (ZC_A2_TACHI_BIN, ZC_A2_TACHI_ENV_JSON, ZC_A2_HOST_IDENTITY,
//! ZC_A2_AGENT_ID, ZC_A2_ADMISSION_REF, ZC_A2_WORK_CLAIM_ID,
//! ZC_A2_WORK_CLAIM_REVISION). They consume the PUBLIC tachi MCP facade
//! only — never a DB.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use zeroclaw_api::session_exec::HostIdentityRef;
use zeroclaw_api::session_exec::{
    ExecutionRunStatusV1, InterventionRequestIdRef, SessionAdvertiseReceiptView,
    SessionAttachmentRef, SessionCanonicalStateV1, SessionConnectionFactV1, SessionEventIdRef,
    SessionEventKindV1, SessionEventReceiptView, SessionFactError,
    SessionInterventionDispositionV1, SessionInterventionKindV1, SessionInterventionRequestView,
    SessionReceiptAdmissionV1, SessionReconnectReceiptView, SessionStateView,
};
use zeroclaw_runtime::execution_subagent::{
    AcpxController, AcpxControllerConfig, ControllerError, ExecutionRunRequest,
    ExecutionSubagentTool, GatedSessionController, SessionBinding, SessionCapabilities,
    SessionController, SessionEventFact, SessionFactSink, SessionStartSpec, TachiFactSinkConfig,
    TachiSessionFactSink,
};

// ─────────────────────────────────────────────────────────────────────────
// Environment plumbing
// ─────────────────────────────────────────────────────────────────────────

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn env_json_map(name: &str) -> HashMap<String, String> {
    std::env::var(name)
        .ok()
        .and_then(|value| serde_json::from_str::<HashMap<String, String>>(&value).ok())
        .unwrap_or_default()
}

fn live_enabled() -> bool {
    std::env::var("ZC_A2_LIVE").is_ok_and(|value| value == "1")
}

fn require_live() {
    if !live_enabled() {
        panic!("set ZC_A2_LIVE=1 (and the ZC_A2_* binaries) to run the live acceptance lane");
    }
}

fn scratch_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// A disposable git repository (the harness's workspace). Nothing in the
/// test writes to it after setup — only the harness does.
fn disposable_repo() -> PathBuf {
    let dir = scratch_dir("zc-a2-repo");
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&dir)
        .status()
        .expect("git init");
    assert!(init.success(), "git init failed");
    std::fs::write(dir.join("README.md"), "disposable e2e workspace\n").expect("seed file");
    dir
}

fn acpx_config(workspace: PathBuf, declared: Vec<&'static str>) -> AcpxControllerConfig {
    let command = env_path("ZC_A2_ACPX_BIN").expect("ZC_A2_ACPX_BIN is required for the live lane");
    AcpxControllerConfig {
        command,
        args: vec![],
        env: env_json_map("ZC_A2_ACPX_ENV_JSON"),
        workspace_root: workspace,
        session_mode: std::env::var("ZC_A2_SESSION_MODE")
            .ok()
            .filter(|value| !value.is_empty()),
        startup_timeout: Duration::from_secs(90),
        turn_timeout: Duration::from_secs(300),
        max_line_bytes: 256 * 1024,
        declared_capabilities: declared,
    }
}

fn host() -> HostIdentityRef {
    HostIdentityRef::from_opaque(
        std::env::var("ZC_A2_HOST_IDENTITY").unwrap_or_else(|_| "zc-a2-live".to_string()),
    )
}

// ─────────────────────────────────────────────────────────────────────────
// In-test fact sink double (carrier-side proofs only; the spine leg uses
// the real TachiSessionFactSink below)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct RecordingSink {
    attached: Mutex<Vec<SessionBinding>>,
    facts: Mutex<Vec<(String, SessionEventKindV1, Option<String>)>>,
    unavailable: Mutex<bool>,
    revision: Mutex<u64>,
}

#[async_trait]
impl SessionFactSink for RecordingSink {
    async fn attach(
        &self,
        binding: &SessionBinding,
        _capabilities: &[String],
    ) -> Result<SessionAttachmentRef, SessionFactError> {
        if *self.unavailable.lock() {
            return Err(SessionFactError::Unavailable);
        }
        self.attached.lock().push(binding.clone());
        Ok(SessionAttachmentRef::from_opaque("att-live-1"))
    }

    async fn advertise_capabilities(
        &self,
        attachment: &SessionAttachmentRef,
        capabilities: &[String],
    ) -> Result<SessionAdvertiseReceiptView, SessionFactError> {
        if *self.unavailable.lock() {
            return Err(SessionFactError::Unavailable);
        }
        Ok(SessionAdvertiseReceiptView {
            attachment_ref: attachment.clone(),
            advertisement_seq: 1,
            capabilities: capabilities.to_vec(),
        })
    }

    async fn ingest_event(
        &self,
        attachment: &SessionAttachmentRef,
        fact: &SessionEventFact,
    ) -> Result<SessionEventReceiptView, SessionFactError> {
        if *self.unavailable.lock() {
            return Err(SessionFactError::Unavailable);
        }
        self.facts.lock().push((
            fact.event_id.as_str().to_string(),
            fact.kind,
            fact.summary.clone(),
        ));
        let mut revision = self.revision.lock();
        *revision = (*revision).max(fact.source_revision);
        Ok(SessionEventReceiptView {
            attachment_ref: attachment.clone(),
            event_id: fact.event_id.clone(),
            admission: SessionReceiptAdmissionV1::Created,
            disposition: "advanced".to_string(),
            state: SessionStateView {
                canonical_state: SessionCanonicalStateV1::Accepted,
                canonical_revision: *revision,
                cleanup_recorded: false,
                conflicting_terminal: false,
                last_event_id: None,
            },
        })
    }

    async fn request_intervention(
        &self,
        _attachment: &SessionAttachmentRef,
        _request_id: &InterventionRequestIdRef,
        _kind: SessionInterventionKindV1,
        _reason: &str,
    ) -> Result<(), SessionFactError> {
        Ok(())
    }

    async fn get_intervention(
        &self,
        _attachment: &SessionAttachmentRef,
        _request_id: &InterventionRequestIdRef,
    ) -> Result<Option<SessionInterventionRequestView>, SessionFactError> {
        Ok(None)
    }

    async fn record_intervention_result(
        &self,
        _attachment: &SessionAttachmentRef,
        _request_id: &InterventionRequestIdRef,
        _disposition: SessionInterventionDispositionV1,
        _authority_confirmation_ref: Option<&str>,
        _detail: Option<&str>,
    ) -> Result<(), SessionFactError> {
        Ok(())
    }

    async fn mark_connection(
        &self,
        _attachment: &SessionAttachmentRef,
        _fact: SessionConnectionFactV1,
    ) -> Result<(), SessionFactError> {
        Ok(())
    }

    async fn reconnect(
        &self,
        _binding: &SessionBinding,
    ) -> Result<SessionReconnectReceiptView, SessionFactError> {
        Ok(SessionReconnectReceiptView {
            attachment_ref: SessionAttachmentRef::from_opaque("att-live-1"),
            reconnected: true,
            resume_from_revision: *self.revision.lock(),
            state: SessionStateView {
                canonical_state: SessionCanonicalStateV1::Progressing,
                canonical_revision: *self.revision.lock(),
                cleanup_recorded: false,
                conflicting_terminal: false,
                last_event_id: None,
            },
        })
    }

    async fn get_state(
        &self,
        _attachment: &SessionAttachmentRef,
    ) -> Result<SessionStateView, SessionFactError> {
        Ok(SessionStateView {
            canonical_state: SessionCanonicalStateV1::Accepted,
            canonical_revision: 1,
            cleanup_recorded: false,
            conflicting_terminal: false,
            last_event_id: None,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Proof 1 + 3: real harness session over the real ACPX carrier changes the
// disposable repo; one bounded correction; final result collected.
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "live: spawns a real ACPX harness (ZC_A2_LIVE=1)"]
async fn live_real_acpx_carrier_runs_one_ephemeral_task_to_completion() {
    require_live();
    let workspace = disposable_repo();
    let controller = Arc::new(GatedSessionController::new(Arc::new(
        AcpxController::new(acpx_config(
            workspace.clone(),
            vec!["observe", "wait", "prompt", "cancel", "resume", "events"],
        ))
        .expect("acpx controller constructs"),
    )));
    let sink = Arc::new(RecordingSink::default());
    let tool = ExecutionSubagentTool::new(controller, sink.clone(), host());

    let request = ExecutionRunRequest {
        objective: "Create a file named zc_e2e_note.txt in the current workspace whose entire \
            content is exactly E2E-A2-OK. Do not run shell commands. When the file is written, \
            reply with exactly: E2E-A2-DONE"
            .to_string(),
        correction_prompt: Some(
            "Verify zc_e2e_note.txt exists and contains exactly E2E-A2-OK; fix it if not. Then \
             reply with exactly: E2E-A2-CONFIRMED"
                .to_string(),
        ),
    };
    let report = tool.run(&request).await;
    println!(
        "LIVE run: status={:?} remote={:?} corrections_actions={} events={} facts={} elapsed_ms={}",
        report.status,
        report.remote_session_ref.as_ref().map(|r| r.as_str()),
        report.usage.actions,
        report.usage.events_observed,
        report.usage.facts_reported,
        report.usage.elapsed_ms,
    );
    println!("LIVE collect summary: {:?}", report.collected_summary);
    println!(
        "LIVE final canonical state: {:?}",
        report.final_canonical_state
    );

    assert_eq!(report.status, ExecutionRunStatusV1::Completed);
    // The remote identity was MINTED BY THE HARNESS (ACP session/new).
    let remote = report
        .remote_session_ref
        .as_ref()
        .expect("the harness minted a session identity");
    assert!(!remote.as_str().is_empty());
    // The repository changed THROUGH the harness: the test itself only
    // seeded README.md; zc_e2e_note.txt can only exist if the harness
    // wrote it.
    let note = workspace.join("zc_e2e_note.txt");
    let content = std::fs::read_to_string(&note).expect("harness wrote the file");
    assert!(
        content.contains("E2E-A2-OK"),
        "harness wrote unexpected content: {content:?}"
    );
    // One bounded correction was delivered (the verify leg).
    assert!(
        report
            .collected_summary
            .as_deref()
            .is_some_and(|summary| summary.contains("E2E-A2-CONFIRMED")),
        "the correction turn's final answer must be the collected result"
    );
    // Facts flowed for the whole lifecycle.
    let kinds: Vec<SessionEventKindV1> =
        sink.facts.lock().iter().map(|(_, kind, _)| *kind).collect();
    assert!(kinds.contains(&SessionEventKindV1::Accepted));
    assert!(kinds.contains(&SessionEventKindV1::InputRequired));
    assert!(kinds.contains(&SessionEventKindV1::Terminal));
    assert!(
        !sink.attached.lock().is_empty(),
        "the run attached to the spine"
    );
    let _ = std::fs::remove_dir_all(&workspace);
}

// ─────────────────────────────────────────────────────────────────────────
// Carrier-side reconnect: drop the transport, resume the SAME harness
// session via ACP session/load (no new session identity is minted).
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "live: spawns a real ACPX harness (ZC_A2_LIVE=1)"]
async fn live_transport_drop_then_resume_keeps_the_same_harness_session() {
    require_live();
    let workspace = disposable_repo();
    let controller = Arc::new(
        AcpxController::new(acpx_config(
            workspace.clone(),
            vec!["observe", "wait", "prompt", "cancel", "resume", "events"],
        ))
        .expect("acpx controller constructs"),
    );
    let spec = SessionStartSpec {
        adapter_connection: zeroclaw_api::session_exec::AdapterConnectionRef::from_opaque(
            "conn-live-resume",
        ),
        prompt: "Reply with exactly: RESUME-READY. Do not use any tools.".to_string(),
        context_digest: "digest".to_string(),
        capabilities: SessionCapabilities::from_names(&[
            "observe".to_string(),
            "wait".to_string(),
            "prompt".to_string(),
            "cancel".to_string(),
            "resume".to_string(),
            "events".to_string(),
        ])
        .expect("valid capability names"),
        max_prompt_bytes: 16_384,
    };
    let handle = controller.start(&spec).await.expect("session starts");
    let original = handle.remote_session.as_str().to_string();
    println!("LIVE resume: original session id = {original}");
    assert!(!original.is_empty());

    // Kill the transport by dropping the whole controller (the child is
    // killed with it), then reattach through a FRESH controller bound to
    // the same workspace — the only surviving identity is the harness's
    // session id.
    drop(controller);
    let revived = Arc::new(
        AcpxController::new(acpx_config(
            workspace.clone(),
            vec!["observe", "wait", "prompt", "cancel", "resume", "events"],
        ))
        .expect("acpx controller constructs"),
    );
    let resumed = revived
        .reattach(
            &zeroclaw_api::session_exec::AdapterConnectionRef::from_opaque("conn-live-resume"),
            &handle.remote_session,
            0,
        )
        .await
        .expect("reattach must resume the SAME harness session (typed failure would mean recovery is broken)");
    assert_eq!(
        resumed.remote_session.as_str(),
        original,
        "resume must reuse the harness-minted identity, never mint a new one"
    );
    // Usability, not just identity: the resumed session answers a real
    // turn through the port.
    let prompt = revived
        .prompt(
            &resumed,
            "Reply with exactly: RESUME-USABLE. Do not use any tools.",
        )
        .await
        .expect("the resumed session must accept a prompt");
    assert!(prompt.accepted);
    let page = revived
        .watch(&resumed, 0, 64)
        .await
        .expect("the resumed session's event stream is readable");
    assert!(
        page.events.iter().any(|event| event
            .summary
            .as_deref()
            .is_some_and(|text| text.contains("RESUME-USABLE"))),
        "the resumed session must produce observable facts"
    );
    println!("LIVE resume: same session id after transport drop, usable — PASS");
    let _ = std::fs::remove_dir_all(&workspace);
}

// ─────────────────────────────────────────────────────────────────────────
// Proof 5: unsupported lifecycle op (cancel not advertised) refuses typed;
// no terminal fact is fabricated.
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "live: spawns a real ACPX harness (ZC_A2_LIVE=1)"]
async fn live_unsupported_stop_surfaces_typed_refusal_without_terminal() {
    require_live();
    let workspace = disposable_repo();
    // The session is declared WITHOUT cancel: the stop gate must refuse
    // typed before any harness interaction.
    let controller = Arc::new(GatedSessionController::new(Arc::new(
        AcpxController::new(acpx_config(
            workspace.clone(),
            vec!["observe", "wait", "prompt", "resume", "events"],
        ))
        .expect("acpx controller constructs"),
    )));
    let spec = SessionStartSpec {
        adapter_connection: zeroclaw_api::session_exec::AdapterConnectionRef::from_opaque(
            "conn-live-unsupported",
        ),
        prompt: "Reply with exactly: UNSUPPORTED-READY. Do not use any tools.".to_string(),
        context_digest: "digest".to_string(),
        capabilities: SessionCapabilities::from_names(&[
            "observe".to_string(),
            "wait".to_string(),
            "prompt".to_string(),
            "resume".to_string(),
            "events".to_string(),
        ])
        .expect("valid capability names"),
        max_prompt_bytes: 16_384,
    };
    let handle = controller.start(&spec).await.expect("session starts");
    let error = controller
        .stop(&handle, true)
        .await
        .expect_err("stop must be refused when cancel is not advertised");
    assert!(
        matches!(error, ControllerError::UnsupportedByLifecycleOwner { .. }),
        "expected the typed unsupported refusal, got {error:?}"
    );
    // Zero fabrication, observed through the port: no terminal fact may
    // exist in the session's event stream after the refused stop.
    let page = controller
        .watch_events(&handle, 0, 128)
        .await
        .expect("event stream readable");
    assert!(
        page.events.iter().all(|event| event.outcome.is_none()),
        "a refused stop must fabricate no terminal fact"
    );
    let _ = std::fs::remove_dir_all(&workspace);
}

// ─────────────────────────────────────────────────────────────────────────
// Proof 6: fail closed — unusable controller, unusable sink, and missing
// harness credentials all refuse before any session survives.
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "live: spawns a real ACPX harness (ZC_A2_LIVE=1)"]
async fn live_fail_closed_refuses_before_any_session_or_fact() {
    require_live();
    let workspace = disposable_repo();

    // (a) Controller construction fails closed on a nonexistent binary:
    // no session can ever start.
    let bad_config = AcpxControllerConfig {
        command: PathBuf::from("/nonexistent/zc-a2/acpx-missing"),
        args: vec![],
        env: Default::default(),
        workspace_root: workspace.clone(),
        session_mode: None,
        startup_timeout: Duration::from_secs(10),
        turn_timeout: Duration::from_secs(10),
        max_line_bytes: 256 * 1024,
        declared_capabilities: vec!["observe", "prompt", "cancel", "resume", "events"],
    };
    let outcome = AcpxController::new(bad_config)
        .err()
        .expect("bad binary must refuse");
    assert!(matches!(outcome, ControllerError::Refused(_)));

    // (b) Sink construction fails closed on a nonexistent spine binary.
    let bad_sink = TachiFactSinkConfig {
        command: PathBuf::from("/nonexistent/zc-a2/tachi-missing"),
        args: vec!["serve".to_string()],
        env: Default::default(),
        host_identity: "zc-a2-live".to_string(),
        agent_identity_id: "zc-a2-live".to_string(),
        admission_receipt_ref: "admission-live".to_string(),
        work_claim_id: "claim-live".to_string(),
        expected_transition_revision: 0,
        contract_digest: "digest".to_string(),
        tool_profile: "delegate".to_string(),
        capability_class: "tachi".to_string(),
        protocol_version: 1,
        call_timeout: Duration::from_secs(30),
    };
    assert!(TachiSessionFactSink::new(bad_sink).is_err());

    // (c) Harness credentials unavailable (an empty CODEX_HOME): the run
    // refuses typed, and no session identity is reported.
    let mut cred_env = HashMap::new();
    let empty_home = scratch_dir("zc-a2-empty-codex-home");
    cred_env.insert("CODEX_HOME".to_string(), empty_home.display().to_string());
    let config = AcpxControllerConfig {
        command: env_path("ZC_A2_ACPX_BIN").expect("ZC_A2_ACPX_BIN"),
        args: vec![],
        env: cred_env,
        workspace_root: workspace.clone(),
        session_mode: None,
        startup_timeout: Duration::from_secs(120),
        turn_timeout: Duration::from_secs(60),
        max_line_bytes: 256 * 1024,
        declared_capabilities: vec!["observe", "wait", "prompt", "cancel", "resume", "events"],
    };
    let controller = Arc::new(GatedSessionController::new(Arc::new(
        AcpxController::new(config)
            .expect("controller constructs (credentials fail later, at the harness)"),
    )));
    let sink = Arc::new(RecordingSink::default());
    let tool = ExecutionSubagentTool::new(controller, sink.clone(), host());
    let report = tool
        .run(&ExecutionRunRequest {
            objective: "Reply with exactly: NEVER. Do not use any tools.".to_string(),
            correction_prompt: None,
        })
        .await;
    println!(
        "LIVE fail-closed: status={:?} refusal={:?}",
        report.status, report.refusal
    );
    assert_eq!(report.status, ExecutionRunStatusV1::Refused);
    assert!(
        report
            .refusal
            .as_deref()
            .is_some_and(|text| text.contains("Authentication required")),
        "the harness's real credential refusal must surface verbatim"
    );
    assert!(report.remote_session_ref.is_none());
    assert!(
        sink.facts.lock().is_empty(),
        "no facts may be claimed on a refused start"
    );
    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&empty_home);
}

// ─────────────────────────────────────────────────────────────────────────
// Proof 2/4 (spine leg): the REAL TachiSessionFactSink against the REAL
// tachi MCP facade. Without spine admission configured the run stops at
// the facade's typed refusal — the transport, wire params, and typed
// error path are all real; the admission ceremony (identity ACP grant)
// has no public provisioning surface in tachi yet (documented blocker).
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "live: spawns a real tachi MCP server (ZC_A2_LIVE=1 + ZC_A2_TACHI_BIN)"]
async fn live_fact_sink_reaches_the_real_tachi_facade_and_reports_typed() {
    require_live();
    let Some(tachi_bin) = env_path("ZC_A2_TACHI_BIN") else {
        panic!("ZC_A2_TACHI_BIN is required for the spine-leg live proof");
    };
    let config = TachiFactSinkConfig {
        command: tachi_bin,
        args: vec!["serve".to_string()],
        env: env_json_map("ZC_A2_TACHI_ENV_JSON"),
        host_identity: std::env::var("ZC_A2_HOST_IDENTITY")
            .unwrap_or_else(|_| "zc-a2-live".to_string()),
        agent_identity_id: std::env::var("ZC_A2_AGENT_ID")
            .unwrap_or_else(|_| "zc-a2-live".to_string()),
        admission_receipt_ref: std::env::var("ZC_A2_ADMISSION_REF")
            .unwrap_or_else(|_| "admission-live".to_string()),
        work_claim_id: std::env::var("ZC_A2_WORK_CLAIM_ID")
            .unwrap_or_else(|_| "claim-live".to_string()),
        expected_transition_revision: std::env::var("ZC_A2_WORK_CLAIM_REVISION")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0),
        contract_digest: "zc-a2-live-digest".to_string(),
        tool_profile: "delegate".to_string(),
        capability_class: "tachi".to_string(),
        protocol_version: 1,
        call_timeout: Duration::from_secs(60),
    };
    let sink = TachiSessionFactSink::new(config).expect("sink constructs");
    let binding = SessionBinding {
        host_identity: host(),
        adapter_connection: zeroclaw_api::session_exec::AdapterConnectionRef::from_opaque(
            "conn-live-spine",
        ),
        remote_session: zeroclaw_api::session_exec::RemoteSessionRef::from_opaque("rs-live-spine"),
        idempotency_key: format!("exec-attach-{}", uuid::Uuid::new_v4().simple()),
    };
    let result = sink
        .attach(&binding, &["observe".to_string(), "prompt".to_string()])
        .await;
    match result {
        Ok(attachment) => {
            println!(
                "LIVE spine: attach admitted attachment={}",
                attachment.as_str()
            );
            // Full admission configured: prove the receipt surface end to
            // end (advertise → ingest → replay-exactly-once → state).
            sink.advertise_capabilities(
                &attachment,
                &["observe".to_string(), "prompt".to_string()],
            )
            .await
            .expect("advertise");
            let receipt = sink
                .ingest_event(
                    &attachment,
                    &SessionEventFact {
                        event_id: SessionEventIdRef::from_opaque("ev-live-1"),
                        kind: SessionEventKindV1::Accepted,
                        outcome: None,
                        source_revision: 1,
                        authority_confirmation_ref: None,
                        summary: None,
                        payload_digest: None,
                    },
                )
                .await
                .expect("ingest");
            println!(
                "LIVE spine: first fact admission={:?} revision={}",
                receipt.admission, receipt.state.canonical_revision
            );
            let replay = sink
                .ingest_event(
                    &attachment,
                    &SessionEventFact {
                        event_id: SessionEventIdRef::from_opaque("ev-live-1"),
                        kind: SessionEventKindV1::Accepted,
                        outcome: None,
                        source_revision: 1,
                        authority_confirmation_ref: None,
                        summary: None,
                        payload_digest: None,
                    },
                )
                .await
                .expect("replay");
            assert_eq!(
                replay.admission,
                SessionReceiptAdmissionV1::Replayed,
                "the same fact must replay exactly once (dedup by id)"
            );
            let state = sink.get_state(&attachment).await.expect("state");
            println!(
                "LIVE spine: canonical state read back: {:?}",
                state.canonical_state
            );
        }
        Err(SessionFactError::Refused(reason)) => {
            // The documented admission blocker: the identity carries no
            // ACP capability grant because tachi exposes no public
            // provisioning surface for it. Typed refusal — never a fake
            // success, never an untyped crash.
            println!("LIVE spine: typed refusal from the real facade: {reason}");
            assert!(
                reason.contains("capability grant")
                    || reason.contains("host admission")
                    || reason.contains("host identity"),
                "expected the typed spine admission refusal, got: {reason}"
            );
        }
        Err(SessionFactError::Unavailable) => {
            panic!(
                "the facade transport failed (not a typed refusal) — check the tachi binary/env"
            );
        }
        Err(error) => panic!("unexpected sink error: {error}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Proof 7 (secret-negative) is asserted inside the S1 run above by the
// report scan here; kept as a separate named test for the lane output.
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "live: spawns a real ACPX harness (ZC_A2_LIVE=1)"]
async fn live_report_surface_carries_no_secrets_or_paths() {
    require_live();
    let workspace = disposable_repo();
    let controller = Arc::new(GatedSessionController::new(Arc::new(
        AcpxController::new(acpx_config(
            workspace.clone(),
            vec!["observe", "wait", "prompt", "cancel", "resume", "events"],
        ))
        .expect("acpx controller constructs"),
    )));
    let sink = Arc::new(RecordingSink::default());
    let tool = ExecutionSubagentTool::new(controller, sink, host());
    // Discriminating by construction: the objective makes the harness
    // echo the workspace path itself, so the raw string WOULD reach the
    // report unless the transport scrubs it.
    let report = tool
        .run(&ExecutionRunRequest {
            objective: "Reply with exactly the absolute path of the current working directory and nothing else. Do not use any tools."
                .to_string(),
            correction_prompt: None,
        })
        .await;
    let serialized = serde_json::to_string(&report).expect("report serializes");
    let workspace_text = workspace.display().to_string();
    assert!(
        !serialized.contains(&workspace_text),
        "the report surface must not carry the repository path even when the harness echoes it"
    );
    assert!(
        serialized.contains("<workspace>"),
        "the echoed path must arrive scrubbed"
    );
    let operator_env = env_json_map("ZC_A2_ACPX_ENV_JSON");
    assert!(
        operator_env.values().any(|value| !value.is_empty()),
        "this live gate is only meaningful with an operator env value configured"
    );
    for (key, value) in operator_env {
        assert!(
            !value.is_empty() && !serialized.contains(&value),
            "the report surface must not carry operator env value for {key}"
        );
    }
    println!(
        "LIVE secret-negative: report surface clean (status={:?})",
        report.status
    );
    let _ = std::fs::remove_dir_all(&workspace);
}

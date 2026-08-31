//! PR1 laws: the closed vocabularies, the capability gate table, the
//! gated client's typed refusals and monotone cursor, and the structural
//! module scans (receipts-only sink, no durable store, no process
//! capability anywhere in the module).

use std::sync::Arc;

use zeroclaw_api::session_exec::{
    AdapterConnectionRef, AuthorityConfirmationRef, ExecutionRequestV1, ExecutionRouteV1,
    SessionCanonicalStateV1, SessionEventIdRef, SessionEventKindV1,
    SessionInterventionDispositionV1, SessionTerminalOutcomeV1,
};

use super::controller::{
    ControllerError, ControllerEvent, GatedSessionController, SessionCapabilities,
    SessionController, SessionStartSpec,
};

use super::facts::{SessionBinding, SessionEventFact, SessionFactSink};

use super::fixtures::{InMemoryFactSink, ScriptedController, ScriptedStep};

fn full_caps() -> SessionCapabilities {
    SessionCapabilities {
        observe: true,
        wait: true,
        prompt: true,
        cancel: true,
        resume: true,
        load: true,
        events: true,
        artifacts: true,
    }
}

fn observe_only_caps() -> SessionCapabilities {
    // The canonical observe-only harness profile: observe + events (the
    // spine's observe-only fixture set). Watching is observation; cancel
    // and prompt stay unadmitted.
    SessionCapabilities {
        observe: true,
        events: true,
        ..SessionCapabilities::default()
    }
}

fn start_spec() -> SessionStartSpec {
    SessionStartSpec {
        adapter_connection: AdapterConnectionRef::from_opaque("conn-1"),
        prompt: "review the diff".to_string(),
        context_digest: "digest-1".to_string(),
        capabilities: full_caps(),
        max_prompt_bytes: 8192,
    }
}

fn binding() -> SessionBinding {
    SessionBinding {
        host_identity: zeroclaw_api::session_exec::HostIdentityRef::from_opaque("host-1"),
        adapter_connection: AdapterConnectionRef::from_opaque("conn-1"),
        remote_session: zeroclaw_api::session_exec::RemoteSessionRef::from_opaque("rs-1"),
        idempotency_key: "idem-1".to_string(),
    }
}

fn fact(revision: u64, id: &str) -> SessionEventFact {
    SessionEventFact {
        event_id: SessionEventIdRef::from_opaque(id),
        kind: SessionEventKindV1::Progress,
        outcome: None,
        source_revision: revision,
        authority_confirmation_ref: None,
        summary: Some("step".to_string()),
        payload_digest: None,
    }
}

fn tokio_rt() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Runtime::new().expect("tokio runtime"))
}

// ─────────────────────────────────────────────────────────────────────────
// Capability gate table
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn capability_gate_maps_the_six_operations_onto_the_closed_set() {
    let caps = full_caps();
    for operation in ["watch", "prompt", "interrupt", "stop", "collect"] {
        assert!(
            caps.unsupported_operation(operation).is_none(),
            "{operation} must be admitted by a full set"
        );
    }
    // start is host-minting and not gated.
}

#[test]
fn observe_only_set_refuses_every_lifecycle_operation_typed() {
    let caps = observe_only_caps();
    // observe+events admits watching and reading; every MUTATING or
    // lifecycle operation stays refused.
    assert!(caps.unsupported_operation("watch").is_none());
    assert_eq!(
        caps.unsupported_operation("prompt").as_deref(),
        Some("prompt")
    );
    assert_eq!(caps.unsupported_operation("stop").as_deref(), Some("stop"));
    assert_eq!(
        caps.unsupported_operation("interrupt").as_deref(),
        Some("stop")
    );
    // observe admits collect (reading is observation).
    assert!(caps.unsupported_operation("collect").is_none());
}

#[test]
fn capability_names_are_a_closed_vocabulary() {
    assert!(SessionCapabilities::from_names(&["observe".to_string()]).is_ok());
    let error = SessionCapabilities::from_names(&["exec_shell".to_string()]).unwrap_err();
    assert!(
        error.to_string().contains("unsupported session capability"),
        "unknown capability must refuse typed: {error}"
    );
    // The forbidden capability names are unrepresentable — this IS the
    // negative capability at the seam level.
    for banned in [
        "shell",
        "file_write",
        "file_edit",
        "git",
        "cli_flags",
        "credentials",
    ] {
        assert!(SessionCapabilities::from_names(&[banned.to_string()]).is_err());
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Gated client law
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn gated_client_refuses_unsupported_operations_before_the_transport_is_touched() {
    let controller = Arc::new(ScriptedController::new(observe_only_caps()));
    let gated = GatedSessionController::new(Arc::clone(&controller) as Arc<dyn SessionController>);
    tokio_rt().block_on(async {
        let handle = gated.start(&start_spec()).await.expect("start is ungated");
        // prompt/stop/interrupt refuse typed; the scripted transport
        // records ZERO of them. watch/collect (observe+events) go through.
        for op in ["prompt", "stop", "interrupt"] {
            let error = match op {
                "prompt" => gated.prompt(&handle, "go").await.unwrap_err(),
                "stop" => gated.stop(&handle, true).await.unwrap_err(),
                _ => gated.interrupt(&handle).await.unwrap_err(),
            };
            assert!(
                matches!(error, ControllerError::UnsupportedByLifecycleOwner { .. }),
                "{op} must refuse typed, got {error:?}"
            );
        }
        gated
            .watch_events(&handle, 0, 10)
            .await
            .expect("events admits watch");
        // collect (observe) goes through.
        gated
            .collect(&handle)
            .await
            .expect("observe admits collect");
    });
    assert!(
        controller.prompts.lock().is_empty(),
        "no prompt reached the transport"
    );
    assert!(
        controller.stop_requests.lock().is_empty(),
        "no stop reached the transport"
    );
    assert_eq!(*controller.interrupt_requests.lock(), 0);
}

#[test]
fn watch_cursor_never_regresses_on_a_stale_page() {
    let controller = Arc::new(ScriptedController::new(full_caps()));
    let gated = GatedSessionController::new(Arc::clone(&controller) as Arc<dyn SessionController>);
    tokio_rt().block_on(async {
        let handle = gated.start(&start_spec()).await.expect("start");
        let page = gated.watch_events(&handle, 0, 100).await.expect("watch");
        let advanced = page.next_seq;
        // A transport that "replays from zero" cannot drag the cursor back:
        // the client law clamps next_seq to at least after_seq.
        let stale = gated
            .watch_events(&handle, advanced + 5, 100)
            .await
            .expect("watch");
        assert!(stale.next_seq >= advanced + 5, "cursor must never regress");
    });
}

#[test]
fn transport_unavailable_is_a_typed_fail_closed_error() {
    let controller = Arc::new(ScriptedController::new(full_caps()));
    let gated = GatedSessionController::new(Arc::clone(&controller) as Arc<dyn SessionController>);
    tokio_rt().block_on(async {
        controller.push(ScriptedStep::TransportDown);
        let error = gated.start(&start_spec()).await.unwrap_err();
        assert_eq!(error, ControllerError::Unavailable);
    });
}

#[test]
fn oversized_prompt_refuses_at_the_port_boundary() {
    let controller = Arc::new(ScriptedController::new(full_caps()));
    let gated = GatedSessionController::new(Arc::clone(&controller) as Arc<dyn SessionController>);
    tokio_rt().block_on(async {
        let mut spec = start_spec();
        spec.prompt = "x".repeat(100);
        spec.max_prompt_bytes = 10;
        let error = gated.start(&spec).await.unwrap_err();
        assert!(
            error.to_string().contains("bounded ceiling"),
            "oversize prompt must refuse typed: {error:?}"
        );
        assert_eq!(
            *controller.started_count.lock(),
            0,
            "no session was started"
        );
    });
}

// ─────────────────────────────────────────────────────────────────────────
// Sink laws (test double mirrors the spine's consumer-facing laws)
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn sink_events_are_replay_idempotent_by_event_id() {
    let sink = InMemoryFactSink::default();
    let attachment = sink
        .attach(&binding(), &["observe".to_string()])
        .await
        .unwrap();
    let first = sink
        .ingest_event(&attachment, &fact(1, "ev-1"))
        .await
        .unwrap();
    assert_eq!(
        first.admission,
        zeroclaw_api::session_exec::SessionReceiptAdmissionV1::Created
    );
    let replay = sink
        .ingest_event(&attachment, &fact(1, "ev-1"))
        .await
        .unwrap();
    assert_eq!(
        replay.admission,
        zeroclaw_api::session_exec::SessionReceiptAdmissionV1::Replayed
    );
    assert_eq!(replay.state, first.state, "replay must not move state");
}

#[tokio::test]
async fn sink_stale_facts_cannot_regress_canonical_state() {
    let sink = InMemoryFactSink::default();
    let attachment = sink.attach(&binding(), &[]).await.unwrap();
    sink.ingest_event(&attachment, &fact(5, "ev-5"))
        .await
        .unwrap();
    let before = sink.get_state(&attachment).await.unwrap();
    let stale = sink
        .ingest_event(&attachment, &fact(2, "ev-2"))
        .await
        .unwrap();
    assert_eq!(stale.disposition, "journaled_stale");
    let after = sink.get_state(&attachment).await.unwrap();
    assert_eq!(before.canonical_revision, after.canonical_revision);
}

#[tokio::test]
async fn sink_progress_after_terminal_never_regresses_the_projection() {
    // The spine's rank law: progress (rank 2) facts after a terminal
    // (rank 3) fact — even at a HIGHER revision — cannot drag the
    // canonical projection back out of a terminal phase.
    let sink = InMemoryFactSink::default();
    let attachment = sink.attach(&binding(), &[]).await.unwrap();
    let started = SessionEventFact {
        event_id: SessionEventIdRef::from_opaque("ev-1"),
        kind: SessionEventKindV1::Started,
        outcome: None,
        source_revision: 1,
        authority_confirmation_ref: None,
        summary: None,
        payload_digest: None,
    };
    let terminal = SessionEventFact {
        event_id: SessionEventIdRef::from_opaque("ev-2"),
        kind: SessionEventKindV1::Terminal,
        outcome: Some(SessionTerminalOutcomeV1::Completed),
        source_revision: 2,
        authority_confirmation_ref: None,
        summary: None,
        payload_digest: None,
    };
    let late_progress = SessionEventFact {
        event_id: SessionEventIdRef::from_opaque("ev-3"),
        kind: SessionEventKindV1::Progress,
        outcome: None,
        source_revision: 3,
        authority_confirmation_ref: None,
        summary: None,
        payload_digest: None,
    };
    sink.ingest_event(&attachment, &started).await.unwrap();
    sink.ingest_event(&attachment, &terminal).await.unwrap();
    let after_terminal = sink.get_state(&attachment).await.unwrap();
    assert_eq!(
        after_terminal.canonical_state,
        SessionCanonicalStateV1::Completed
    );
    let late = sink
        .ingest_event(&attachment, &late_progress)
        .await
        .unwrap();
    assert_eq!(
        late.state.canonical_state,
        SessionCanonicalStateV1::Completed,
        "a rank-2 fact at revision 3 must not regress a rank-3 terminal"
    );
}

#[test]
fn start_mints_the_remote_session_identity_the_caller_cannot_choose_it() {
    let controller = Arc::new(ScriptedController::new(full_caps()));
    let gated = GatedSessionController::new(Arc::clone(&controller) as Arc<dyn SessionController>);
    tokio_rt().block_on(async {
        let first = gated.start(&start_spec()).await.expect("start");
        let second = gated.start(&start_spec()).await.expect("start");
        assert_ne!(
            first.remote_session, second.remote_session,
            "each start mints a distinct transport-owned session id"
        );
        assert!(
            first.remote_session.as_str().starts_with("rs-fixture-"),
            "the minted id is observable on the handle, never caller-chosen"
        );
    });
}

#[tokio::test]
async fn sink_unavailable_fails_closed_typed() {
    let sink = InMemoryFactSink::default();
    *sink.unavailable.lock() = true;
    let error = sink.attach(&binding(), &[]).await.unwrap_err();
    assert_eq!(
        error,
        zeroclaw_api::session_exec::SessionFactError::Unavailable
    );
}

#[tokio::test]
async fn cancelled_terminal_facts_carry_confirmation_refs() {
    // Structural at the type level (see the api test); here the fact path:
    // a cancelled outcome without a ref cannot be CONSTRUCTED, so the sink
    // can never receive one.
    let outcome = SessionTerminalOutcomeV1::Cancelled {
        confirmation: AuthorityConfirmationRef::from_opaque("zc-confirm-1"),
    };
    assert!(outcome.authority_confirmation_ref().is_some());
}

// ─────────────────────────────────────────────────────────────────────────
// Route discriminator (transport-independent half; the bridge half is
// discriminated in PR2/PR3 against the real seam)
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn ephemeral_request_routes_ephemeral_never_durable() {
    let request = ExecutionRequestV1 {
        objective: "fix the flaky test".to_string(),
        needs_restart_recovery: false,
        needs_remote: false,
        needs_multi_attempt: false,
        needs_approvals: false,
        needs_evidence: false,
        analysis_only: false,
    };
    assert_eq!(
        ExecutionRouteV1::route(&request),
        ExecutionRouteV1::EphemeralExec
    );
}

#[test]
fn scripted_emit_events_flow_through_the_gated_watch() {
    let controller = Arc::new(ScriptedController::new(full_caps()));
    let gated = GatedSessionController::new(Arc::clone(&controller) as Arc<dyn SessionController>);
    tokio_rt().block_on(async {
        let handle = gated.start(&start_spec()).await.expect("start");
        let base = gated.watch_events(&handle, 0, 100).await.expect("watch");
        controller.push(ScriptedStep::Emit(vec![ControllerEvent {
            seq: 0,
            event_id: SessionEventIdRef::from_opaque("ev-progress"),
            kind: SessionEventKindV1::Progress,
            outcome: None,
            summary: Some("halfway".to_string()),
        }]));
        let page = gated
            .watch_events(&handle, base.next_seq, 100)
            .await
            .expect("watch");
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].event_id.as_str(), "ev-progress");
        // Cursor advanced; the next poll is empty and stays put.
        let empty = gated
            .watch_events(&handle, page.next_seq, 100)
            .await
            .expect("watch");
        assert!(empty.events.is_empty());
        assert_eq!(empty.next_seq, page.next_seq);
        // Transport recovery path is representable and flips the flag.
        controller.push(ScriptedStep::TransportUp);
    });
}

// ─────────────────────────────────────────────────────────────────────────
// Structural module scans
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn module_source_scans_hold() {
    // Per-layer law pinned per file (honest scope: the A2 transport
    // adapters ARE process/transport-capable by design, so the scan is
    // split instead of pretended):
    //
    // (a) the typed supervisor surface (ports, tool, router, fixtures)
    //     stays process-, filesystem-, command-, git-, and credential-
    //     free;
    // (b) the ONLY process-capable file in the module is the ACPX
    //     transport (acpx.rs) — the host-owned spawn the ports point at;
    //     it still bans store/credential tokens;
    // (c) the tachi facade sink opens no database and spawns nothing of
    //     its own (it rides the zeroclaw-tools MCP client), so it keeps
    //     the full ban list;
    // (d) no durable store anywhere in the module (the banned tokens are
    //     assembled at runtime so this scan does not itself read as a
    //     store site to the TB-22 gate);
    // (e) the agent path does not reference this module yet (nothing
    //     registers it — default closed).
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let store_bans = [
        ["rusq", "lite"].join(""),
        ["Connection", "::open"].join(""),
        ["CREATE", " TABLE"].join(""),
        ["api_", "key"].join(""),
    ];
    let full_bans = [
        ["std::", "process"].join(""),
        ["tokio::", "process"].join(""),
        ["process", "::Command"].join(""),
        ["std::", "fs::"].join(""),
        ["child", "::Stdio"].join(""),
        ["Stdio", "::piped"].join(""),
    ]
    .iter()
    .cloned()
    .chain(store_bans.iter().cloned())
    .collect::<Vec<_>>();
    let port_files = [
        "execution_subagent/mod.rs",
        "execution_subagent/controller.rs",
        "execution_subagent/facts.rs",
        "execution_subagent/fixtures.rs",
        "execution_subagent/tool.rs",
        "execution_subagent/router.rs",
    ];
    for file in port_files {
        let source = std::fs::read_to_string(format!("{manifest_dir}/src/{file}"))
            .unwrap_or_else(|error| panic!("read {file}: {error}"));
        for banned in &full_bans {
            assert!(
                !source.contains(banned.as_str()),
                "{file} must not contain {banned} (receipts-only / no-store law)"
            );
        }
    }
    // The ACPX transport is the single process-capable file: it may
    // spawn, but it may never open a store or carry credential tokens.
    let acpx = std::fs::read_to_string(format!("{manifest_dir}/src/execution_subagent/acpx.rs"))
        .expect("read acpx.rs");
    for banned in &store_bans {
        assert!(
            !acpx.contains(banned.as_str()),
            "acpx.rs must not contain {banned} (no-store law)"
        );
    }
    // Every OTHER module file (including the tachi facade sink) stays
    // free of DIRECT spawn tokens. This is a lexical per-file law, not a
    // reachability proof: tachi_sink reaches process spawning indirectly
    // through the shared zeroclaw-tools MCP transport factory — that
    // capability is owned (and environment/stderr-scoped) by the tools
    // crate, not re-declared here.
    let other_files = [
        "execution_subagent/mod.rs",
        "execution_subagent/controller.rs",
        "execution_subagent/facts.rs",
        "execution_subagent/tool.rs",
        "execution_subagent/router.rs",
        "execution_subagent/tachi_sink.rs",
    ];
    let process_bans = [
        ["tokio::", "process"].join(""),
        ["process", "::Command"].join(""),
        ["Stdio", "::piped"].join(""),
    ];
    for file in other_files {
        let source = std::fs::read_to_string(format!("{manifest_dir}/src/{file}"))
            .unwrap_or_else(|error| panic!("read {file}: {error}"));
        for banned in &process_bans {
            assert!(
                !source.contains(banned.as_str()),
                "{file} must not contain {banned} (single process-capable transport law)"
            );
        }
    }
}

#[test]
fn unused_intervention_dispositions_remain_representable() {
    // Guard against an accidental vocabulary shrink: the sink must be
    // able to report every disposition the spine defines.
    for disposition in [
        SessionInterventionDispositionV1::Accepted,
        SessionInterventionDispositionV1::Refused,
        SessionInterventionDispositionV1::Unsupported,
        SessionInterventionDispositionV1::Failed,
    ] {
        assert!(!disposition.as_str().is_empty());
    }
}

// ─────────────────────────────────────────────────────────────────────────
// PR2: tool surface, negative-capability suite, and the router gate
// ─────────────────────────────────────────────────────────────────────────

use super::router::{DispatchError, DispatchPlan, plan_dispatch};
use super::tool::{ExecutionRunRequest, ExecutionSubagentProfile, ExecutionSubagentTool};
use crate::tachi_bridge::{
    BridgeQueryError, RequesterBridgePolicy, ResultProjectionView, StructuralIntentContext,
    SubmitReceipt, SubmitTransportError, TachiBridgeClient, TachiTaskBridge, TaskEventPageView,
    TaskIntentInputs, TaskSnapshotView, compose_intent,
};
use async_trait::async_trait;
use zeroclaw_api::session_exec::{ExecutionRunStatusV1, HostIdentityRef, SessionConnectionFactV1};
use zeroclaw_api::subagent_v1::SubAgentBudgetV1;
use zeroclaw_api::subagent_v1::{LineageRef, ParentRunRef};
use zeroclaw_api::taskintent::{EvaluationRequirement, RequestId, TaskIntentV1};
use zeroclaw_api::taskintent::{InterventionError, InterventionReceipt};
use zeroclaw_api::tool::Tool as _;

fn tool_for_test(
    controller: Arc<ScriptedController>,
    sink: Arc<InMemoryFactSink>,
) -> ExecutionSubagentTool {
    ExecutionSubagentTool::new(
        Arc::new(super::controller::GatedSessionController::new(
            controller as Arc<dyn super::controller::SessionController>,
        )),
        sink as Arc<dyn SessionFactSink>,
        HostIdentityRef::from_opaque("host-test"),
    )
}

fn ephemeral_request() -> zeroclaw_api::session_exec::ExecutionRequestV1 {
    ExecutionRequestV1 {
        objective: "fix the flaky test".to_string(),
        needs_restart_recovery: false,
        needs_remote: false,
        needs_multi_attempt: false,
        needs_approvals: false,
        needs_evidence: false,
        analysis_only: false,
    }
}

fn durable_request() -> zeroclaw_api::session_exec::ExecutionRequestV1 {
    ExecutionRequestV1 {
        objective: "recover the failed migration".to_string(),
        needs_restart_recovery: true,
        needs_remote: false,
        needs_multi_attempt: false,
        needs_approvals: false,
        needs_evidence: false,
        analysis_only: false,
    }
}

/// A bridge transport that is DOWN (TB-20). Sibling of the bridge's own
/// test double; defined here (cfg(test)) so the router gate can prove
/// the durable path fails closed against the REAL client.
struct UnavailableTachiBridge;

#[async_trait]
impl TachiTaskBridge for UnavailableTachiBridge {
    async fn submit(
        &self,
        _intent: &TaskIntentV1,
        _request_id: &RequestId,
    ) -> Result<SubmitReceipt, SubmitTransportError> {
        Ok(SubmitReceipt::Unavailable)
    }

    async fn get(
        &self,
        _task_ref: &zeroclaw_api::taskintent::TaskRef,
    ) -> Result<TaskSnapshotView, BridgeQueryError> {
        Err(BridgeQueryError::Unavailable)
    }

    async fn watch(
        &self,
        _task_ref: &zeroclaw_api::taskintent::TaskRef,
        _after_seq: u64,
        _limit: usize,
    ) -> Result<TaskEventPageView, BridgeQueryError> {
        Err(BridgeQueryError::Unavailable)
    }

    async fn collect(
        &self,
        _task_ref: &zeroclaw_api::taskintent::TaskRef,
        _result_revision: Option<u64>,
    ) -> Result<ResultProjectionView, BridgeQueryError> {
        Err(BridgeQueryError::Unavailable)
    }

    async fn intervene(
        &self,
        _task_ref: &zeroclaw_api::taskintent::TaskRef,
        _intervention: &zeroclaw_api::taskintent::InterventionV1,
        _requester: &zeroclaw_api::taskintent::RequesterRef,
        _request_id: &RequestId,
        _expected_task_revision: Option<u64>,
    ) -> Result<InterventionReceipt, InterventionError> {
        Err(InterventionError::Unavailable)
    }

    async fn request_stop(
        &self,
        _task_ref: &zeroclaw_api::taskintent::TaskRef,
        _mode: zeroclaw_api::taskintent::StopMode,
        _requester: &zeroclaw_api::taskintent::RequesterRef,
        _request_id: &RequestId,
        _expected_task_revision: Option<u64>,
    ) -> Result<zeroclaw_api::taskintent::StopReceipt, InterventionError> {
        Err(InterventionError::Unavailable)
    }
}

#[test]
fn tool_parameter_schema_exposes_only_bounded_inputs() {
    let tool = tool_for_test(
        Arc::new(ScriptedController::new(full_caps())),
        Arc::new(InMemoryFactSink::default()),
    );
    let schema = tool.parameters_schema();
    let mut keys: Vec<&str> = schema["properties"]
        .as_object()
        .expect("properties object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["correction_prompt", "objective"]);
    // No extra key can smuggle a shell flag, a path, or a credential.
    assert_eq!(
        schema["additionalProperties"],
        serde_json::Value::Bool(false)
    );
    assert_eq!(schema["required"], serde_json::json!(["objective"]));
}

#[test]
fn run_inventory_serialized_key_set_pins_the_negative_capability() {
    let tool = tool_for_test(
        Arc::new(ScriptedController::new(full_caps())),
        Arc::new(InMemoryFactSink::default()),
    );
    let inventory = tool.run_inventory(&ExecutionRunRequest {
        objective: "review".to_string(),
        correction_prompt: Some("run make check".to_string()),
    });
    let json = serde_json::to_value(&inventory).expect("serialize");
    let mut keys: Vec<&str> = json
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    // The exact inventory: objective bytes, whether a correction is
    // authorized, the frozen profile identity, the budget ceiling, the
    // declared capability names, the two typed outbound surfaces, and the
    // lineage depth. A credential/workspace/CLI-flag field would change
    // this set observably.
    assert_eq!(
        keys,
        vec![
            "budget_max_actions",
            "correction_authorized",
            "declared_capabilities",
            "lineage_depth",
            "objective_bytes",
            "outbound_surfaces",
            "profile_digest",
            "profile_id",
            "profile_revision",
        ]
    );
    assert_eq!(
        inventory.outbound_surfaces,
        vec!["session-controller", "fact-sink"]
    );
    for banned in [
        "credential",
        "shell",
        "file_write",
        "file_edit",
        "git",
        "worktree",
        "cli",
    ] {
        let rendered = serde_json::to_string(&inventory).unwrap();
        assert!(
            !rendered.to_lowercase().contains(banned),
            "{banned} leaked into inventory"
        );
    }
}

#[tokio::test]
async fn child_context_cannot_run_execution_subagents_d1() {
    let controller = Arc::new(ScriptedController::new(full_caps()));
    let sink = Arc::new(InMemoryFactSink::default());
    let root = LineageRef::new_root(ParentRunRef::from_opaque("root"));
    let tool =
        tool_for_test(Arc::clone(&controller), Arc::clone(&sink)).with_lineage(Some(root.child()));
    let report = tool
        .run(&ExecutionRunRequest {
            objective: "fix".to_string(),
            correction_prompt: None,
        })
        .await;
    assert_eq!(report.status, ExecutionRunStatusV1::Refused);
    assert!(report.refusal.unwrap().contains("child context"));
    assert_eq!(
        *controller.started_count.lock(),
        0,
        "no session was started"
    );
    assert_eq!(
        *sink.attachments_created.lock(),
        0,
        "no attachment was created"
    );
}

#[test]
fn short_lived_review_plans_ephemeral_without_minting_a_task_ref() {
    // The plan itself is pure — but the discrimination is that an
    // EPHEMERAL request does not require the bridge: no TaskRef path is
    // involved at all.
    let plan = plan_dispatch(&ephemeral_request(), false, true).expect("plan");
    match plan {
        DispatchPlan::Ephemeral { run } => {
            assert_eq!(run.objective, "fix the flaky test");
        }
        other => panic!("expected ephemeral, got {other:?}"),
    }
}

#[test]
fn durable_request_with_no_bridge_configured_is_a_typed_error_never_ephemeral() {
    let error = plan_dispatch(&durable_request(), false, true).unwrap_err();
    assert_eq!(error, DispatchError::DurableRequiresBridge);
    // And the mirror: an ephemeral request with no tool configured is
    // also a typed error, never a silent local run.
    assert_eq!(
        plan_dispatch(&ephemeral_request(), true, false).unwrap_err(),
        DispatchError::EphemeralRequiresController
    );
}

#[tokio::test]
async fn durable_request_through_unavailable_bridge_fails_closed_without_local_execution() {
    let controller = Arc::new(ScriptedController::new(full_caps()));
    let sink = Arc::new(InMemoryFactSink::default());
    let tool = tool_for_test(Arc::clone(&controller), Arc::clone(&sink));
    // The route is DURABLE; a bridge IS configured (but down at runtime).
    let plan = plan_dispatch(&durable_request(), true, true).expect("plan");
    assert_eq!(plan, DispatchPlan::Durable);
    // The caller submits through the real bridge client — typed outage.
    let client = TachiBridgeClient::new(Arc::new(UnavailableTachiBridge));
    // The durable path NEVER routes through the ephemeral tool as a
    // fallback; prove by submitting and checking the typed outage, and
    // that the controller/session machinery stayed untouched.
    let intent = compose_minimal_intent();
    let receipt = client
        .submit(
            &intent,
            &RequestId::new("req-durable-1".to_string()).expect("bounded request id"),
        )
        .await;
    assert!(
        matches!(receipt, Ok(SubmitReceipt::Unavailable)),
        "durable outage must surface as the typed Unavailable receipt: {receipt:?}"
    );
    assert_eq!(
        *controller.started_count.lock(),
        0,
        "no ephemeral session was started"
    );
    assert_eq!(
        *sink.attachments_created.lock(),
        0,
        "no attachment was created"
    );
    let _ = &tool; // the tool exists but is not the durable fallback
}

#[test]
fn analysis_request_plans_reason_and_never_touches_the_ports() {
    let request = ExecutionRequestV1 {
        objective: "compare the two designs".to_string(),
        needs_restart_recovery: false,
        needs_remote: false,
        needs_multi_attempt: false,
        needs_approvals: false,
        needs_evidence: false,
        analysis_only: true,
    };
    assert_eq!(
        plan_dispatch(&request, true, true).expect("plan"),
        DispatchPlan::Reason
    );
}

#[tokio::test]
async fn ephemeral_run_through_the_tool_reports_a_structured_receipt() {
    let controller = Arc::new(ScriptedController::new(full_caps()));
    let sink = Arc::new(InMemoryFactSink::default());
    let tool = tool_for_test(Arc::clone(&controller), Arc::clone(&sink));
    controller.push(super::fixtures::ScriptedStep::Emit(vec![
        super::controller::ControllerEvent {
            seq: 0,
            event_id: SessionEventIdRef::from_opaque("ev-started"),
            kind: SessionEventKindV1::Started,
            outcome: None,
            summary: None,
        },
        super::controller::ControllerEvent {
            seq: 0,
            event_id: SessionEventIdRef::from_opaque("ev-progress"),
            kind: SessionEventKindV1::Progress,
            outcome: None,
            summary: Some("halfway".to_string()),
        },
        super::controller::ControllerEvent {
            seq: 0,
            event_id: SessionEventIdRef::from_opaque("ev-terminal"),
            kind: SessionEventKindV1::Terminal,
            outcome: Some(SessionTerminalOutcomeV1::Completed),
            summary: Some("done".to_string()),
        },
    ]));
    let report = tool
        .run(&ExecutionRunRequest {
            objective: "short review".to_string(),
            correction_prompt: None,
        })
        .await;
    assert_eq!(report.status, ExecutionRunStatusV1::Completed);
    assert_eq!(report.route, ExecutionRouteV1::EphemeralExec);
    assert!(report.attachment_ref.is_some(), "facts flowed to the sink");
    assert_eq!(
        report.final_canonical_state,
        Some(SessionCanonicalStateV1::Completed)
    );
    assert!(
        report.collected_digest.is_some(),
        "collect produced the bounded projection"
    );
    // The harness's facts reached the sink.
    let facts = sink.facts.lock();
    let kinds: Vec<SessionEventKindV1> = facts.iter().map(|(fact, _)| fact.kind).collect();
    assert!(kinds.contains(&SessionEventKindV1::Started));
    assert!(kinds.contains(&SessionEventKindV1::Terminal));
    assert!(
        kinds.contains(&SessionEventKindV1::Cleanup),
        "cleanup receipt recorded"
    );
}

#[tokio::test]
async fn controller_unavailable_at_start_refuses_without_touching_the_sink() {
    let controller = Arc::new(ScriptedController::new(full_caps()));
    let sink = Arc::new(InMemoryFactSink::default());
    let tool = tool_for_test(Arc::clone(&controller), Arc::clone(&sink));
    controller.push(super::fixtures::ScriptedStep::TransportDown);
    let report = tool
        .run(&ExecutionRunRequest {
            objective: "short review".to_string(),
            correction_prompt: None,
        })
        .await;
    assert_eq!(report.status, ExecutionRunStatusV1::Refused);
    assert!(report.refusal.unwrap().contains("fail closed"));
    assert_eq!(*sink.attachments_created.lock(), 0);
}

#[tokio::test]
async fn sink_unavailable_after_start_stops_the_session_and_refuses() {
    let controller = Arc::new(ScriptedController::new(full_caps()));
    let sink = Arc::new(InMemoryFactSink::default());
    let tool = tool_for_test(Arc::clone(&controller), Arc::clone(&sink));
    // The controller stays up (start succeeds); the sink is unavailable,
    // so the attach leg fails and the run must stop the session it
    // started — nothing may keep running unobserved.
    *sink.unavailable.lock() = true;
    let report = tool
        .run(&ExecutionRunRequest {
            objective: "short review".to_string(),
            correction_prompt: None,
        })
        .await;
    assert_eq!(report.status, ExecutionRunStatusV1::Refused);
    assert!(report.refusal.unwrap().contains("fail closed"));
    // The session was stopped so nothing runs unobserved.
    assert!(
        !controller.stop_requests.lock().is_empty(),
        "session stopped on abandon"
    );
}

/// Compose a minimal clean intent for the durable-path test (the compose
/// surface's five values; authority fields come from the requester
/// policy, mirroring the bridge compose tests' own builders).
fn compose_minimal_intent() -> TaskIntentV1 {
    use std::collections::BTreeSet;
    use zeroclaw_api::taskintent::{
        ApprovalRequirement, ArtifactClass, ArtifactExpectation, BoundedText, Capability,
        CapabilityRequest, IndependenceClass, PrivacyClass, RoutingPreference, TaskConstraint,
        WorkspaceSourceRef,
    };
    let inputs = TaskIntentInputs {
        objective: BoundedText::new("recover the failed migration").expect("bounded"),
        capability_request: CapabilityRequest {
            capability: Capability::RepositoryImplementation,
        },
        constraints: vec![TaskConstraint {
            description: BoundedText::new("durable recovery; refs, not relay prose")
                .expect("bounded"),
        }],
        expected_artifacts: vec![ArtifactExpectation {
            artifact_class: ArtifactClass::Diff,
            description: BoundedText::new("repository diff").expect("bounded"),
            required: true,
        }],
        evaluation_requirement: EvaluationRequirement {
            independence: IndependenceClass::DeterministicCheck,
        },
    };
    let policy = RequesterBridgePolicy {
        admitted_capabilities: BTreeSet::from([Capability::RepositoryImplementation]),
        workspace_source: Some(WorkspaceSourceRef {
            repo: BoundedText::new("kckylechen1/zeroclaw").expect("bounded"),
            git_ref: Some(BoundedText::new("master").expect("bounded")),
        }),
        routing_preference: Some(RoutingPreference::PreferTachiManaged),
        approval_requirement: ApprovalRequirement::NotRequired,
        privacy_class: PrivacyClass::Internal,
    };
    let structural = StructuralIntentContext {
        requester: zeroclaw_api::taskintent::RequesterRef::claim("agent:parent")
            .expect("bounded requester"),
        parent_ref: None,
        supervisor_ref: None,
        context_bundle_ref: BoundedText::new("bundle-durable-test").expect("bounded"),
        source_refs: Vec::new(),
        expiry: None,
        retry_of: None,
    };
    compose_intent(&inputs, &policy, &structural).expect("clean intent")
}

// ─────────────────────────────────────────────────────────────────────────
// The five vertical discriminations — one NAMED test per contract row
// ─────────────────────────────────────────────────────────────────────────

/// Discrimination 1: a short-lived review/fix routes EPHEMERAL (no
/// TaskRef, no durable claim); a restart-recovery/remote/evidence-required
/// request routes DURABLE through the bridge — and a DURABLE request
/// NEVER degrades: without a bridge it is a typed error; with a down
/// bridge the REAL client returns the typed Unavailable receipt and no
/// session, attachment, or local execution happens.
#[test]
fn discrimination_ephemeral_vs_durable_routing_is_typed_and_never_falls_back_local() {
    // short review/fix → EPHEMERAL: no bridge configured, no TaskRef minted.
    match plan_dispatch(&ephemeral_request(), false, true).expect("plan") {
        DispatchPlan::Ephemeral { .. } => {}
        other => panic!("short review must route ephemeral, got {other:?}"),
    }
    // every durability requirement → DURABLE, flag by flag (the addendum
    // table): with the bridge configured the plan is Durable; without it
    // the SAME request is a typed DurableRequiresBridge error — never an
    // ephemeral or local downgrade.
    for flag in [
        "needs_restart_recovery",
        "needs_remote",
        "needs_multi_attempt",
        "needs_approvals",
        "needs_evidence",
    ] {
        let mut request = ephemeral_request();
        match flag {
            "needs_restart_recovery" => request.needs_restart_recovery = true,
            "needs_remote" => request.needs_remote = true,
            "needs_multi_attempt" => request.needs_multi_attempt = true,
            "needs_approvals" => request.needs_approvals = true,
            _ => request.needs_evidence = true,
        }
        assert_eq!(
            plan_dispatch(&request, true, true).expect("plan"),
            DispatchPlan::Durable,
            "{flag} must route durable"
        );
        assert_eq!(
            plan_dispatch(&request, false, true).unwrap_err(),
            DispatchError::DurableRequiresBridge,
            "{flag} with no bridge must fail closed"
        );
    }
    // analysis → REASON: neither port is required or touched.
    let mut analysis = ephemeral_request();
    analysis.analysis_only = true;
    assert_eq!(
        plan_dispatch(&analysis, false, false),
        Ok(DispatchPlan::Reason)
    );
}

/// Discrimination 2: the subagent cannot reach shell, file_write,
/// file_edit, git, worktree paths, CLI flags, or credentials — through
/// the tool's model-visible schema, the run request/inventory types, the
/// capability vocabulary, or the module's own source.
#[test]
fn discrimination_subagent_cannot_reach_shell_file_git_cli_or_credentials() {
    // (a) the model-visible surface admits exactly two bounded inputs.
    let tool = tool_for_test(
        Arc::new(ScriptedController::new(full_caps())),
        Arc::new(InMemoryFactSink::default()),
    );
    let schema = tool.parameters_schema();
    let mut keys: Vec<String> = schema["properties"]
        .as_object()
        .expect("properties")
        .keys()
        .cloned()
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["correction_prompt", "objective"]);
    assert_eq!(
        schema["additionalProperties"],
        serde_json::Value::Bool(false)
    );

    // (b) the run inventory's serialized key set has no capability field.
    let inventory = tool.run_inventory(&ExecutionRunRequest {
        objective: "x".to_string(),
        correction_prompt: None,
    });
    let rendered = serde_json::to_string(&inventory).unwrap().to_lowercase();
    for banned in [
        "shell",
        "file_write",
        "file_edit",
        "git",
        "worktree",
        "cli",
        "credential",
        "api_key",
    ] {
        assert!(
            !rendered.contains(banned),
            "{banned} reachable via inventory"
        );
    }

    // (c) the closed capability vocabulary refuses every forbidden name.
    for banned in [
        "shell",
        "file_write",
        "file_edit",
        "git",
        "worktree",
        "cli_flags",
        "credentials",
    ] {
        assert!(SessionCapabilities::from_names(&[banned.to_string()]).is_err());
    }

    // (d) the module source carries no execution capability at all.
    module_source_scans_hold();
}

/// Discrimination 3: reconnect after attachment loss replays facts
/// WITHOUT regressing canonical state — replayed ids dedup, stale facts
/// journal without moving the projection, a lower-rank fact after a
/// terminal moves nothing, and the orphaned state recovers on
/// authoritative facts.
#[tokio::test]
async fn discrimination_reconnect_replays_facts_without_regressing_canonical_state() {
    let sink = InMemoryFactSink::default();
    let attachment = sink.attach(&binding(), &[]).await.unwrap();
    // facts stream up: accepted(1) started(2) progress(3)
    for (revision, id) in [(1u64, "ev-1"), (2, "ev-2"), (3, "ev-3")] {
        sink.ingest_event(&attachment, &fact(revision, id))
            .await
            .unwrap_or_else(|_| panic!("ingest {id}"));
    }
    // dropout: the host reports the connection lost; the spine marks the
    // attachment unknown (orphaned) — recoverable, never guessed terminal.
    sink.mark_connection(&attachment, SessionConnectionFactV1::Disconnected)
        .await
        .unwrap();
    let orphaned = sink.get_state(&attachment).await.unwrap();
    let _ = orphaned;
    let reconnect = sink.reconnect(&binding()).await.unwrap();
    assert!(
        reconnect.reconnected,
        "reconnected must reflect the marked dropout (a reconnect with no          dropout is not a recovery)"
    );
    assert_eq!(reconnect.resume_from_revision, 3);
    // A reconnect with NO marked dropout is not a recovery.
    let stray = sink.reconnect(&binding()).await.unwrap();
    assert!(!stray.reconnected);
    // replay: the SAME fact (same id, same revision) dedups; a STALE fact
    // journals without moving anything; the terminal then advances.
    let replay = sink
        .ingest_event(&attachment, &fact(3, "ev-3"))
        .await
        .unwrap();
    assert_eq!(
        replay.admission,
        zeroclaw_api::session_exec::SessionReceiptAdmissionV1::Replayed
    );
    let stale = sink
        .ingest_event(&attachment, &fact(2, "ev-stale"))
        .await
        .unwrap();
    assert_eq!(stale.disposition, "journaled_stale");
    let terminal = SessionEventFact {
        event_id: SessionEventIdRef::from_opaque("ev-terminal"),
        kind: SessionEventKindV1::Terminal,
        outcome: Some(SessionTerminalOutcomeV1::Completed),
        source_revision: 4,
        authority_confirmation_ref: None,
        summary: None,
        payload_digest: None,
    };
    let done = sink.ingest_event(&attachment, &terminal).await.unwrap();
    assert_eq!(
        done.state.canonical_state,
        SessionCanonicalStateV1::Completed
    );
    // a late lower-rank fact cannot drag the terminal back.
    let late = sink
        .ingest_event(&attachment, &fact(5, "ev-late"))
        .await
        .unwrap();
    assert_eq!(late.state, done.state, "no regression after terminal");
}

/// Discrimination 4: unsupported lifecycle operations surface TYPED
/// refusals — never fake success. The gated client refuses before the
/// transport; the tool reports unsupported_operation and no terminal
/// fact, receipt, or intervention is fabricated.
#[test]
fn discrimination_unsupported_lifecycle_operations_surface_typed_refusals() {
    // gated client level
    gated_client_refuses_unsupported_operations_before_the_transport_is_touched();
    // gate table level
    observe_only_set_refuses_every_lifecycle_operation_typed();
}

#[tokio::test]
async fn discrimination_unsupported_stop_fabricates_nothing() {
    // Controller-gate variant: the session is observe-only, so the GATED
    // controller refuses before the transport.
    let controller = Arc::new(ScriptedController::new(observe_only_caps()));
    let sink = Arc::new(InMemoryFactSink::default());
    let tool = tool_for_test(Arc::clone(&controller), Arc::clone(&sink));
    controller.push(ScriptedStep::Emit(vec![
        super::controller::ControllerEvent {
            seq: 0,
            event_id: SessionEventIdRef::from_opaque("ev-input"),
            kind: SessionEventKindV1::InputRequired,
            outcome: None,
            summary: None,
        },
    ]));
    let report = tool
        .run(&ExecutionRunRequest {
            objective: "observe-only session asking for input".to_string(),
            correction_prompt: None,
        })
        .await;
    assert_eq!(
        report.status,
        ExecutionRunStatusV1::UnsupportedOperation,
        "refusal was {:?}",
        report.refusal
    );
    let facts = sink.facts.lock();
    let has_terminal = facts
        .iter()
        .any(|(fact, _)| fact.kind == SessionEventKindV1::Terminal);
    assert!(!has_terminal, "no terminal fact may be fabricated");
    let results = sink.results.lock();
    assert!(
        results.is_empty(),
        "no intervention result may be fabricated"
    );
}

#[tokio::test]
async fn discrimination_unsupported_stop_at_the_spine_gate_fabricates_nothing() {
    // Spine-gate variant: the session CAN cancel, but the attachment's
    // DECLARED capability set (what the host admitted with) does not —
    // the spine's typed unsupported refusal is the first link to fail,
    // and the run must surface it with zero fabrication.
    let controller = Arc::new(ScriptedController::new(full_caps()));
    let sink = Arc::new(InMemoryFactSink::default());
    let observe_events = ExecutionSubagentProfile {
        budget: SubAgentBudgetV1 {
            time_limit_secs: 600,
            max_tokens: 200_000,
            max_actions: 200,
        },
        max_corrections: 0,
        max_prompt_bytes: 16_384,
        declared_capabilities: vec!["observe", "events"],
        ..ExecutionSubagentProfile::default_execution_profile()
    };
    let tool =
        tool_for_test(Arc::clone(&controller), Arc::clone(&sink)).with_profile(observe_events);
    controller.push(ScriptedStep::Emit(vec![
        super::controller::ControllerEvent {
            seq: 0,
            event_id: SessionEventIdRef::from_opaque("ev-input"),
            kind: SessionEventKindV1::InputRequired,
            outcome: None,
            summary: None,
        },
    ]));
    let report = tool
        .run(&ExecutionRunRequest {
            objective: "session whose declared set cannot cancel".to_string(),
            correction_prompt: None,
        })
        .await;
    assert_eq!(
        report.status,
        ExecutionRunStatusV1::UnsupportedOperation,
        "refusal was {:?}",
        report.refusal
    );
    assert!(
        report
            .refusal
            .as_deref()
            .unwrap_or_default()
            .contains("unsupported"),
        "the spine gate's typed refusal must surface: {:?}",
        report.refusal
    );
    let facts = sink.facts.lock();
    assert!(
        !facts
            .iter()
            .any(|(fact_, _)| fact_.kind == SessionEventKindV1::Terminal),
        "no terminal fact may be fabricated at the spine gate"
    );
    assert!(sink.results.lock().is_empty());
}

/// Discrimination 5: no new durable task store exists in ZeroClaw for
/// this vertical — the module owns no DDL, no connection opens, no store
/// crate; facts live in the tachi-owned spine, and the persistence-
/// surface gate's manifest stays clean.
#[test]
fn discrimination_no_new_durable_task_store_in_zero_claw() {
    module_source_scans_hold();
    // The sink trait is receipts-only: it cannot open, create, or write a
    // local store (compile-level: its methods transport receipts; the
    // only in-memory ledger is cfg(test)-gated).
    // CARGO_MANIFEST_DIR is the crate dir; the manifest lives at the
    // workspace root.
    let manifest = std::fs::read_to_string(format!(
        "{}/../../scripts/ci/persistence_surface.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("persistence manifest");
    assert!(
        !manifest.contains("execution_subagent"),
        "the execution_subagent module must never enter the store manifest"
    );
}

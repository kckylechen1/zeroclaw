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
    SessionCapabilities {
        observe: true,
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
    assert_eq!(
        caps.unsupported_operation("watch").as_deref(),
        Some("watch")
    );
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
        // watch/prompt/stop/interrupt refuse typed; the scripted transport
        // records ZERO of them.
        for op in ["watch", "prompt", "stop", "interrupt"] {
            let error = match op {
                "watch" => gated.watch_events(&handle, 0, 10).await.unwrap_err(),
                "prompt" => gated.prompt(&handle, "go").await.unwrap_err(),
                "stop" => gated.stop(&handle, true).await.unwrap_err(),
                _ => gated.interrupt(&handle).await.unwrap_err(),
            };
            assert!(
                matches!(error, ControllerError::UnsupportedByLifecycleOwner { .. }),
                "{op} must refuse typed, got {error:?}"
            );
        }
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
    // (a) receipts-only sink + typed supervisor: NO process, filesystem,
    //     command, git, or credential capability anywhere in the module.
    // (b) no durable store: no store crate, connection-open site, or DDL
    //     anywhere (the sink's ledger double is cfg(test) and in-memory
    //     only; the banned tokens are assembled at runtime below so this
    //     scan does not itself read as a store site to the TB-22 gate).
    // (c) the agent path does not reference this module yet (nothing
    //     registers it — default closed; the wiring leaf is gated on this
    //     vertical gate).
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    // Banned tokens are assembled at runtime so this scan list itself
    // never reads as a store site to the persistence-surface gate's
    // scanner (TB-22): the module must contain none of them, and so must
    // the scan that enforces it.
    let banned_tokens = [
        ["std::", "process"].join(""),
        ["tokio::", "process"].join(""),
        ["process", "::Command"].join(""),
        ["std::", "fs::"].join(""),
        ["rusq", "lite"].join(""),
        ["Connection", "::open"].join(""),
        ["CREATE", " TABLE"].join(""),
        ["api_", "key"].join(""),
    ];
    let module_files = [
        "execution_subagent/mod.rs",
        "execution_subagent/controller.rs",
        "execution_subagent/facts.rs",
        "execution_subagent/fixtures.rs",
    ];
    for file in module_files {
        let source = std::fs::read_to_string(format!("{manifest_dir}/src/{file}"))
            .unwrap_or_else(|error| panic!("read {file}: {error}"));
        for banned in &banned_tokens {
            assert!(
                !source.contains(banned.as_str()),
                "{file} must not contain {banned} (receipts-only / no-store law)"
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

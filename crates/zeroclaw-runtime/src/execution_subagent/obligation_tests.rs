//! Obligation-report laws: one closed typed record per attempted
//! post-execution / best-effort obligation, across the normal tail, the
//! attach-failure and abandon paths, the watch-disconnect leg, and the
//! cancel chain. These tests drive the REAL tool (`run`/`execute`) against
//! the scripted fixtures and assert the serialized records, fact counts,
//! and transport call counts.

use std::sync::Arc;

use zeroclaw_api::session_exec::{
    AuthorityConfirmationRef, ExecutionObligationDispositionV1 as Disposition,
    ExecutionObligationKindV1 as Operation, ExecutionObligationV1, ExecutionRunStatusV1,
    ExecutionSessionReportV1, HostIdentityRef, SessionCanonicalStateV1, SessionEventIdRef,
    SessionEventKindV1, SessionFactError, SessionInterventionDispositionV1,
    SessionTerminalOutcomeV1,
};
use zeroclaw_api::tool::Tool as _;

use super::controller::{
    ControllerError, ControllerEvent, GatedSessionController, SessionCapabilities,
    SessionCollectView, SessionController,
};
use super::facts::SessionFactSink;
use super::fixtures::{InMemoryFactSink, ScriptedController, ScriptedStep};
use super::tool::{ExecutionRunRequest, ExecutionSubagentTool};

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
        events: true,
        ..SessionCapabilities::default()
    }
}

fn tool_for_test(
    controller: Arc<ScriptedController>,
    sink: Arc<InMemoryFactSink>,
) -> ExecutionSubagentTool {
    let controller = Arc::new(GatedSessionController::new(
        controller as Arc<dyn SessionController>,
    ));
    ExecutionSubagentTool::new(
        controller,
        sink as Arc<dyn SessionFactSink>,
        HostIdentityRef::from_opaque("host-obligation-test"),
    )
}

fn request() -> ExecutionRunRequest {
    ExecutionRunRequest {
        objective: "bounded obligation probe".to_string(),
        correction_prompt: None,
    }
}

fn completed_events() -> Vec<ControllerEvent> {
    vec![
        ControllerEvent {
            seq: 0,
            event_id: SessionEventIdRef::from_opaque("ev-started"),
            kind: SessionEventKindV1::Started,
            outcome: None,
            summary: None,
        },
        ControllerEvent {
            seq: 0,
            event_id: SessionEventIdRef::from_opaque("ev-terminal"),
            kind: SessionEventKindV1::Terminal,
            outcome: Some(SessionTerminalOutcomeV1::Completed),
            summary: Some("done".to_string()),
        },
    ]
}

fn input_required_event() -> ControllerEvent {
    ControllerEvent {
        seq: 0,
        event_id: SessionEventIdRef::from_opaque("ev-input"),
        kind: SessionEventKindV1::InputRequired,
        outcome: None,
        summary: None,
    }
}

fn obligation(operation: Operation, disposition: Disposition) -> ExecutionObligationV1 {
    ExecutionObligationV1 {
        operation,
        disposition,
    }
}

fn dispositions(report: &ExecutionSessionReportV1, operation: Operation) -> Vec<Disposition> {
    report
        .obligations
        .iter()
        .filter(|record| record.operation == operation)
        .map(|record| record.disposition)
        .collect()
}

fn has_terminal_fact(sink: &InMemoryFactSink) -> bool {
    sink.facts
        .lock()
        .iter()
        .any(|(fact, _)| fact.kind == SessionEventKindV1::Terminal)
}

#[tokio::test]
async fn healthy_run_records_the_three_satisfied_tail_obligations() {
    let controller = Arc::new(ScriptedController::new(full_caps()));
    let sink = Arc::new(InMemoryFactSink::default());
    let tool = tool_for_test(Arc::clone(&controller), Arc::clone(&sink));
    controller.push(ScriptedStep::Emit(completed_events()));

    let report = tool.run(&request()).await;

    assert_eq!(report.status, ExecutionRunStatusV1::Completed);
    assert_eq!(
        report.obligations,
        vec![
            obligation(Operation::CleanupReceipt, Disposition::Satisfied),
            obligation(Operation::Collection, Disposition::Satisfied),
            obligation(Operation::StateRead, Disposition::Satisfied),
        ]
    );
    assert!(
        dispositions(&report, Operation::Stop).is_empty(),
        "a completed run attempts no stop"
    );
    assert_eq!(
        report.usage.facts_reported, 4,
        "accepted + started + terminal + cleanup"
    );
    let data = serde_json::to_value(&report).expect("serialize");
    assert_eq!(data["obligations"][0]["operation"], "CleanupReceipt");
    assert_eq!(data["obligations"][0]["disposition"], "Satisfied");
    assert_eq!(data["obligations"].as_array().map(Vec::len), Some(3));
}

#[tokio::test]
async fn rejected_cleanup_is_refused_and_not_counted_as_a_fact() {
    let controller = Arc::new(ScriptedController::new(full_caps()));
    let sink = Arc::new(InMemoryFactSink::default());
    let tool = tool_for_test(Arc::clone(&controller), Arc::clone(&sink));
    sink.ingest_refusals
        .lock()
        .push(SessionEventKindV1::Cleanup);
    controller.push(ScriptedStep::Emit(completed_events()));

    let report = tool.run(&request()).await;

    assert_eq!(report.status, ExecutionRunStatusV1::Completed);
    assert_eq!(
        dispositions(&report, Operation::CleanupReceipt),
        vec![Disposition::Refused]
    );
    assert_eq!(
        dispositions(&report, Operation::StateRead),
        vec![Disposition::Partial],
        "an attempted but unacknowledged cleanup makes the state read partial"
    );
    assert_eq!(
        dispositions(&report, Operation::Collection),
        vec![Disposition::Satisfied]
    );
    assert_eq!(
        report.usage.facts_reported, 3,
        "a rejected cleanup receipt must not be counted"
    );
    assert!(
        !sink
            .facts
            .lock()
            .iter()
            .any(|(fact, _)| fact.kind == SessionEventKindV1::Cleanup),
        "the rejected cleanup wrote nothing"
    );
}

#[tokio::test]
async fn collect_failure_is_exposed_on_a_completed_report_json() {
    let mut controller = ScriptedController::new(full_caps());
    controller.collect_refusal = Some(ControllerError::Unavailable);
    let controller = Arc::new(controller);
    let sink = Arc::new(InMemoryFactSink::default());
    let tool = tool_for_test(Arc::clone(&controller), Arc::clone(&sink));
    controller.push(ScriptedStep::Emit(completed_events()));

    let result = tool
        .execute(serde_json::json!({"objective": "bounded obligation probe"}))
        .await
        .expect("execute");
    assert!(
        result.success,
        "execution status stays Completed; the failure travels in obligations"
    );
    let data = result.output.data().cloned().expect("structured report");
    assert_eq!(data["status"], "Completed");
    assert_eq!(data["collected_digest"], serde_json::Value::Null);
    assert_eq!(data["obligations"][1]["operation"], "Collection");
    assert_eq!(data["obligations"][1]["disposition"], "Unavailable");
    assert_eq!(*controller.started_count.lock(), 1);
}

#[tokio::test]
async fn state_read_error_is_recorded_and_preserves_the_last_state() {
    let controller = Arc::new(ScriptedController::new(full_caps()));
    let sink = Arc::new(InMemoryFactSink::default());
    *sink.state_refusal.lock() = Some(SessionFactError::Unavailable);
    let tool = tool_for_test(Arc::clone(&controller), Arc::clone(&sink));
    controller.push(ScriptedStep::Emit(completed_events()));

    let report = tool.run(&request()).await;

    assert_eq!(report.status, ExecutionRunStatusV1::Completed);
    assert_eq!(
        dispositions(&report, Operation::StateRead),
        vec![Disposition::Unavailable]
    );
    assert_eq!(
        report.final_canonical_state,
        Some(SessionCanonicalStateV1::Completed),
        "the last returned canonical state is preserved"
    );
    assert_eq!(
        dispositions(&report, Operation::CleanupReceipt),
        vec![Disposition::Satisfied]
    );
}

#[tokio::test]
async fn conflicting_or_reconciling_state_reads_are_partial() {
    let controller = Arc::new(ScriptedController::new(full_caps()));
    let conflicting_sink = Arc::new(InMemoryFactSink::default());
    *conflicting_sink.conflicting_terminal.lock() = true;
    let tool = tool_for_test(Arc::clone(&controller), Arc::clone(&conflicting_sink));
    controller.push(ScriptedStep::Emit(completed_events()));
    let report = tool.run(&request()).await;
    assert_eq!(
        dispositions(&report, Operation::StateRead),
        vec![Disposition::Partial]
    );
    assert_eq!(
        report.final_canonical_state,
        Some(SessionCanonicalStateV1::Completed),
        "the returned canonical state is preserved through a conflict"
    );

    let controller = Arc::new(ScriptedController::new(full_caps()));
    let reconciling_sink = Arc::new(InMemoryFactSink::default());
    *reconciling_sink.forced_canonical_state.lock() =
        Some(SessionCanonicalStateV1::InconsistentReconciling);
    let tool = tool_for_test(Arc::clone(&controller), Arc::clone(&reconciling_sink));
    controller.push(ScriptedStep::Emit(completed_events()));
    let report = tool.run(&request()).await;
    assert_eq!(
        dispositions(&report, Operation::StateRead),
        vec![Disposition::Partial]
    );
    assert_eq!(
        report.final_canonical_state,
        Some(SessionCanonicalStateV1::InconsistentReconciling),
        "the reconciling state is returned, never guessed terminal"
    );
}

#[tokio::test]
async fn empty_successful_collect_is_satisfied() {
    let controller = Arc::new(ScriptedController::new(full_caps()));
    let sink = Arc::new(InMemoryFactSink::default());
    let tool = tool_for_test(Arc::clone(&controller), Arc::clone(&sink));
    *controller.collect_view.lock() = Some(SessionCollectView {
        summary: None,
        digest: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string(),
        evidence_refs: vec![],
    });
    controller.push(ScriptedStep::Emit(completed_events()));

    let report = tool.run(&request()).await;

    assert_eq!(report.status, ExecutionRunStatusV1::Completed);
    assert_eq!(
        dispositions(&report, Operation::Collection),
        vec![Disposition::Satisfied],
        "a successful empty collection is satisfied, not a failure"
    );
    assert_eq!(report.collected_summary, None);
    assert!(report.collected_digest.is_some());
    assert!(report.evidence_refs.is_empty());
}

#[tokio::test]
async fn attach_failure_records_the_failed_stop_attempt_once() {
    let mut controller = ScriptedController::new(full_caps());
    controller.stop_refusal = Some(ControllerError::Unavailable);
    let controller = Arc::new(controller);
    let sink = Arc::new(InMemoryFactSink::default());
    *sink.unavailable.lock() = true;
    let tool = tool_for_test(Arc::clone(&controller), Arc::clone(&sink));

    let report = tool.run(&request()).await;

    assert_eq!(report.status, ExecutionRunStatusV1::Refused);
    assert_eq!(report.attachment_ref, None);
    assert_eq!(
        report.obligations,
        vec![obligation(Operation::Stop, Disposition::Unavailable)]
    );
    assert_eq!(
        controller.stop_requests.lock().len(),
        1,
        "the stop was attempted exactly once"
    );
    assert_eq!(*sink.attachments_created.lock(), 0);
}

#[tokio::test]
async fn abandon_records_stop_and_connection_report_exactly_once() {
    let mut controller = ScriptedController::new(full_caps());
    controller.stop_refusal = Some(ControllerError::UnsupportedByLifecycleOwner {
        operation: "stop".to_string(),
    });
    let controller = Arc::new(controller);
    let sink = Arc::new(InMemoryFactSink::default());
    *sink.advertise_refusal.lock() = true;
    let tool = tool_for_test(Arc::clone(&controller), Arc::clone(&sink));

    let report = tool.run(&request()).await;

    assert_eq!(report.status, ExecutionRunStatusV1::Refused);
    assert!(report.attachment_ref.is_some());
    assert_eq!(
        report.obligations,
        vec![
            obligation(Operation::Stop, Disposition::Unsupported),
            obligation(Operation::ConnectionReport, Disposition::Satisfied),
        ]
    );
    assert_eq!(controller.stop_requests.lock().len(), 1);
    assert_eq!(sink.connection_facts.lock().len(), 1);
    assert!(sink.advertised.lock().is_empty());
}

#[tokio::test]
async fn watch_disconnect_records_one_connection_report_and_recovers() {
    let controller = Arc::new(ScriptedController::new(full_caps()));
    let sink = Arc::new(InMemoryFactSink::default());
    let tool = tool_for_test(Arc::clone(&controller), Arc::clone(&sink));
    // The transport drops on the FIRST watch call (after start/attach) and
    // is back on the next: the reconnect leg runs against the sink.
    *controller.watch_failures_remaining.lock() = 1;
    controller.push(ScriptedStep::Emit(completed_events()));

    let report = tool.run(&request()).await;

    assert_eq!(report.status, ExecutionRunStatusV1::Completed);
    assert_eq!(report.obligations.len(), 4);
    assert_eq!(
        report.obligations[0],
        obligation(Operation::ConnectionReport, Disposition::Satisfied)
    );
    assert_eq!(
        dispositions(&report, Operation::ConnectionReport).len(),
        1,
        "one dropout reports one connection attempt"
    );
    assert_eq!(
        sink.connection_facts.lock().as_slice(),
        &[zeroclaw_api::session_exec::SessionConnectionFactV1::Disconnected]
    );
    assert_eq!(*sink.reconnections.lock(), 1);
    assert_eq!(
        report.usage.facts_reported, 5,
        "accepted + reconnect + started + terminal + cleanup"
    );
}

#[tokio::test]
async fn unsupported_cancel_records_unsupported_stop_without_fabrication() {
    let controller = Arc::new(ScriptedController::new(observe_only_caps()));
    let sink = Arc::new(InMemoryFactSink::default());
    let tool = tool_for_test(Arc::clone(&controller), Arc::clone(&sink));
    controller.push(ScriptedStep::Emit(vec![input_required_event()]));

    let report = tool.run(&request()).await;

    assert_eq!(report.status, ExecutionRunStatusV1::UnsupportedOperation);
    assert_eq!(
        report.obligations,
        vec![
            obligation(Operation::Stop, Disposition::Unsupported),
            obligation(Operation::CleanupReceipt, Disposition::Satisfied),
            obligation(Operation::Collection, Disposition::Satisfied),
            obligation(Operation::StateRead, Disposition::Satisfied),
        ]
    );
    assert!(
        controller.stop_requests.lock().is_empty(),
        "the gated stop refused before the transport was touched"
    );
    assert!(!has_terminal_fact(&sink));
    assert!(sink.results.lock().is_empty());
}

#[tokio::test]
async fn unconfirmed_cancel_records_partial_stop_and_fabricates_nothing() {
    let controller = Arc::new(ScriptedController::new(full_caps()));
    let sink = Arc::new(InMemoryFactSink::default());
    let tool = tool_for_test(Arc::clone(&controller), Arc::clone(&sink));
    controller.push(ScriptedStep::Emit(vec![input_required_event()]));

    let report = tool.run(&request()).await;

    assert_eq!(report.status, ExecutionRunStatusV1::UnsupportedOperation);
    assert_eq!(
        dispositions(&report, Operation::Stop),
        vec![Disposition::Partial],
        "a requested but unconfirmed stop is partial"
    );
    assert!(
        report
            .refusal
            .as_deref()
            .unwrap_or_default()
            .contains("unconfirmed")
    );
    assert_eq!(report.interventions.len(), 1);
    assert_eq!(
        report.interventions[0].disposition,
        SessionInterventionDispositionV1::Failed
    );
    assert_eq!(
        sink.results.lock().as_slice(),
        &[(
            report.interventions[0].request_id.clone(),
            SessionInterventionDispositionV1::Failed
        )]
    );
    assert!(!has_terminal_fact(&sink), "no terminal was fabricated");
    assert_eq!(
        controller.stop_requests.lock().as_slice(),
        &[true],
        "one graceful stop attempt reached the transport"
    );
}

#[tokio::test]
async fn confirmed_stop_without_a_confirmation_ref_cannot_report_graceful() {
    let mut controller = ScriptedController::new(full_caps());
    controller.stop_confirmed_without_ref = true;
    let controller = Arc::new(controller);
    let sink = Arc::new(InMemoryFactSink::default());
    let tool = tool_for_test(Arc::clone(&controller), Arc::clone(&sink));
    controller.push(ScriptedStep::Emit(vec![input_required_event()]));

    let report = tool.run(&request()).await;

    assert_eq!(
        report.status,
        ExecutionRunStatusV1::UnsupportedOperation,
        "confirmed without an authority ref is not a confirmation"
    );
    assert_eq!(
        dispositions(&report, Operation::Stop),
        vec![Disposition::Partial]
    );
    assert_eq!(report.interventions.len(), 1);
    assert_eq!(
        report.interventions[0].disposition,
        SessionInterventionDispositionV1::Failed
    );
    assert_eq!(
        sink.results.lock().as_slice(),
        &[(
            report.interventions[0].request_id.clone(),
            SessionInterventionDispositionV1::Failed
        )]
    );
    assert!(!has_terminal_fact(&sink), "no terminal was fabricated");
}

#[tokio::test]
async fn confirmed_cancel_records_satisfied_stop_and_the_bound_terminal() {
    let mut controller = ScriptedController::new(full_caps());
    controller.stop_confirmation =
        Some(AuthorityConfirmationRef::from_opaque("confirm-obligation"));
    let controller = Arc::new(controller);
    let sink = Arc::new(InMemoryFactSink::default());
    let tool = tool_for_test(Arc::clone(&controller), Arc::clone(&sink));
    controller.push(ScriptedStep::Emit(vec![input_required_event()]));

    let report = tool.run(&request()).await;

    assert_eq!(report.status, ExecutionRunStatusV1::StoppedGracefully);
    assert_eq!(
        report.obligations,
        vec![
            obligation(Operation::Stop, Disposition::Satisfied),
            obligation(Operation::CleanupReceipt, Disposition::Satisfied),
            obligation(Operation::Collection, Disposition::Satisfied),
            obligation(Operation::StateRead, Disposition::Satisfied),
        ]
    );
    assert_eq!(report.interventions.len(), 1);
    assert_eq!(
        report.interventions[0].disposition,
        SessionInterventionDispositionV1::Accepted
    );
    let facts = sink.facts.lock();
    let terminal = facts
        .iter()
        .find(|(fact, _)| fact.kind == SessionEventKindV1::Terminal)
        .expect("the bound cancelled terminal was ingested");
    assert!(matches!(
        terminal.0.outcome.as_ref(),
        Some(SessionTerminalOutcomeV1::Cancelled { .. })
    ));
    assert_eq!(
        terminal.0.authority_confirmation_ref.as_deref(),
        Some("confirm-obligation")
    );
}

#[tokio::test]
async fn accepted_cleanup_with_missing_readback_is_partial() {
    let controller = Arc::new(ScriptedController::new(full_caps()));
    let sink = Arc::new(InMemoryFactSink::default());
    *sink.cleanup_readback_missing.lock() = true;
    let tool = tool_for_test(Arc::clone(&controller), Arc::clone(&sink));
    controller.push(ScriptedStep::Emit(completed_events()));

    let report = tool.run(&request()).await;

    assert_eq!(report.status, ExecutionRunStatusV1::Completed);
    assert_eq!(
        dispositions(&report, Operation::CleanupReceipt),
        vec![Disposition::Satisfied]
    );
    assert_eq!(
        dispositions(&report, Operation::StateRead),
        vec![Disposition::Partial]
    );
    assert_eq!(
        report.final_canonical_state,
        Some(SessionCanonicalStateV1::Completed)
    );
}

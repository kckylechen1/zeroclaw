//! Vertical V3 tests: the four owner discriminations (named), the
//! supervisor authority negative tests, and the full loop on the
//! in-memory bridge double.

use std::collections::BTreeSet;
use std::sync::Arc;

use zeroclaw_api::subagent_v1::{
    LineageRef, ParentActionKind, ParentRunRef, SubAgentToolNameV1, SupervisorAuthority,
    VersionedProfileRef,
};
use zeroclaw_api::taskintent::{
    ArtifactClass, ArtifactExpectation, BoundedText, Capability, CapabilityRequest,
    EvaluationRequirement, IndependenceClass as IC, InterventionError, InterventionReceipt,
    InterventionStatic, InterventionV1, RequestId, RequesterRef, TaskConstraint, TaskRef,
};

use crate::subagent_v1::{DEFAULT_SUPERVISOR_PROFILE_ID, SubAgentProfileRegistry};
use crate::tachi_bridge::in_memory::InMemoryTachiTaskBridge;
use crate::tachi_bridge::{
    RequesterBridgePolicy, StructuralIntentContext, SubmitReceipt, SupervisorIntervention,
    SupervisorInterventionError, TachiBridgeClient, TachiTaskBridge, TaskIntentInputs,
};

use super::SupervisorSessionV1;

// ─── helpers ─────────────────────────────────────────────────────────────

/// One shared host double per test: the supervisor session, the parent
/// submitter, and the probe clients all bind to the SAME Arc so state
/// (tasks, facts, bindings) is observed consistently.
struct Rig {
    bridge: Arc<InMemoryTachiTaskBridge>,
}

impl Rig {
    fn new() -> Self {
        Self {
            bridge: Arc::new(InMemoryTachiTaskBridge::new()),
        }
    }

    fn client(&self) -> TachiBridgeClient {
        TachiBridgeClient::new(Arc::clone(&self.bridge) as Arc<dyn TachiTaskBridge>)
    }
}

fn registry() -> SubAgentProfileRegistry {
    let mut registry = SubAgentProfileRegistry::new();
    let vref = registry
        .admit(SubAgentProfileRegistry::default_supervisor_profile())
        .expect("default supervisor profile admits");
    assert_eq!(vref.profile_id, DEFAULT_SUPERVISOR_PROFILE_ID);
    registry
}

fn supervisor_vref(registry: &SubAgentProfileRegistry) -> VersionedProfileRef {
    registry
        .latest_ref(DEFAULT_SUPERVISOR_PROFILE_ID)
        .expect("default supervisor profile is admitted")
}

fn requester() -> RequesterRef {
    serde_json::from_value(serde_json::json!("requester:supervisor-test")).expect("requester ref")
}

fn session(rig: &Rig) -> SupervisorSessionV1 {
    let registry = registry();
    SupervisorSessionV1::from_admitted_profile(
        &registry,
        &supervisor_vref(&registry),
        &LineageRef::new_root(ParentRunRef::from_opaque("root-supervisor-test")),
        rig.client(),
        requester(),
        None,
    )
    .expect("supervisor session admits")
}

fn implementation_inputs() -> TaskIntentInputs {
    TaskIntentInputs {
        objective: BoundedText::new("add a bounded marker function with a unit test").unwrap(),
        capability_request: CapabilityRequest {
            capability: Capability::RepositoryImplementation,
        },
        constraints: vec![TaskConstraint {
            description: BoundedText::new("no new durable ledgers; refs, not relay prose").unwrap(),
        }],
        expected_artifacts: vec![
            ArtifactExpectation {
                artifact_class: ArtifactClass::Diff,
                description: BoundedText::new("repository diff implementing the objective")
                    .unwrap(),
                required: true,
            },
            ArtifactExpectation {
                artifact_class: ArtifactClass::VerificationLog,
                description: BoundedText::new("verification ran and passed").unwrap(),
                required: true,
            },
        ],
        evaluation_requirement: EvaluationRequirement {
            independence: IC::FreshContextCrossVendor,
        },
    }
}

fn parent_policy() -> RequesterBridgePolicy {
    RequesterBridgePolicy {
        admitted_capabilities: BTreeSet::from([Capability::RepositoryImplementation]),
        workspace_source: None,
        routing_preference: None,
        approval_requirement: zeroclaw_api::taskintent::ApprovalRequirement::NotRequired,
        privacy_class: zeroclaw_api::taskintent::PrivacyClass::Internal,
    }
}

/// Drive the parent half of the flow: submit the intent the supervisor
/// planned (the PARENT performs the initial submit — SA-29's set has no
/// submit op), hand the minted TaskRef back, return it.
async fn parent_submits_planned_task(supervisor: &mut SupervisorSessionV1, rig: &Rig) -> TaskRef {
    let action = supervisor
        .plan_implementation_task(&implementation_inputs(), &parent_policy())
        .expect("plan composes");
    let payload = action.task_intent_request.expect("typed submit payload");
    match rig
        .client()
        .submit(&payload.intent, &payload.request_id)
        .await
        .expect("parent submit")
    {
        SubmitReceipt::Admitted { task_ref, .. } => {
            supervisor
                .attach_implementation_task(task_ref.clone())
                .expect("attach");
            task_ref
        }
        other => panic!("parent submit not admitted: {other:?}"),
    }
}

fn attempt(n: u64) -> zeroclaw_api::taskintent::AttemptRef {
    serde_json::from_value(serde_json::json!(format!("attempt:test-{n:04}"))).expect("attempt ref")
}

// ─── profile + admission ─────────────────────────────────────────────────

#[test]
fn supervisor_profile_holds_exactly_the_typed_authority_set() {
    // DoD row 1: EXACTLY the ten typed Tachi authorities — never one
    // `can_manage_tachi` bit. The enum is the vocabulary; the default
    // profile carries all ten, and admission refuses empty or
    // duplicate-carrying sets.
    let profile = SubAgentProfileRegistry::default_supervisor_profile();
    let mut expected = vec![
        SupervisorAuthority::ObserveTask,
        SupervisorAuthority::ReadResultRefs,
        SupervisorAuthority::ProvideContext,
        SupervisorAuthority::RequestCorrection,
        SupervisorAuthority::RequestContinuation,
        SupervisorAuthority::RequestIndependentReview,
        SupervisorAuthority::RequestUserInput,
        SupervisorAuthority::RequestGracefulStop,
        SupervisorAuthority::RequestCancel,
        SupervisorAuthority::ProposeJudgment,
    ];
    expected.sort_unstable();
    let mut actual = profile.supervisor_authority_set.clone();
    actual.sort_unstable();
    assert_eq!(actual, expected, "the default set is exactly the ten");
    assert_eq!(actual.len(), 10);

    // There is no `can_manage_tachi` shape: the field is the typed Vec.
    assert!(profile.supervisor_authority_set.len() == 10);

    let mut registry = SubAgentProfileRegistry::new();
    registry
        .admit(SubAgentProfileRegistry::default_supervisor_profile())
        .expect("admits");

    // Empty set refused.
    let mut empty = SubAgentProfileRegistry::default_supervisor_profile();
    empty.profile_id = "supervisor-empty".into();
    empty.revision = 1;
    empty.supervisor_authority_set = Vec::new();
    empty.digest = empty.compute_digest();
    assert!(matches!(
        registry.admit(empty),
        Err(crate::subagent_v1::ProfileAdmissionError::EmptySupervisorAuthority { .. })
    ));

    // Duplicate refused.
    let mut dup = SubAgentProfileRegistry::default_supervisor_profile();
    dup.profile_id = "supervisor-dup".into();
    dup.supervisor_authority_set
        .push(SupervisorAuthority::ObserveTask);
    dup.digest = dup.compute_digest();
    assert!(matches!(
        registry.admit(dup),
        Err(crate::subagent_v1::ProfileAdmissionError::DuplicateSupervisorAuthority { .. })
    ));
}

#[test]
fn supervisor_refuses_pause_resume_escalate_by_structure() {
    // DoD row 1, second half: TB-11 operations outside the grant set are
    // refused for Supervisor profiles. The refusal is STRUCTURAL on two
    // levels: `InterventionStatic::supervisor_authority` maps the three
    // ops to None (no authority exists to grant them), and the
    // supervisor intervention surface (`SupervisorIntervention`) has no
    // variant that could carry them — this test pins the mapping and the
    // advertisement law (pause/resume are never supported by the managed
    // lane: typed refusal, zero mutation).
    for op in [
        InterventionStatic::RequestPause,
        InterventionStatic::RequestResume,
        InterventionStatic::Escalate,
    ] {
        assert_eq!(op.supervisor_authority(), None);
    }
    // The full vocabulary is still the frozen ten on the wire (the
    // bridge serves non-supervisor callers too)…
    assert_eq!(InterventionStatic::ALL.len(), 10);
    // …and the supervisor's typed vocabulary cannot spell the three.
    let wire_names: Vec<String> = ["request_pause", "request_resume", "escalate"]
        .into_iter()
        .map(String::from)
        .collect();
    let surface = [
        SupervisorIntervention::ProvideContext {
            note: BoundedText::new("n").unwrap(),
        },
        SupervisorIntervention::RequestCorrection {
            note: BoundedText::new("n").unwrap(),
        },
        SupervisorIntervention::RequestContinuation {
            note: BoundedText::new("n").unwrap(),
        },
        SupervisorIntervention::RequestUserInput {
            prompt: BoundedText::new("q").unwrap(),
        },
        SupervisorIntervention::RequestGracefulStop {
            reason: BoundedText::new("r").unwrap(),
        },
        SupervisorIntervention::RequestHardCancel {
            reason: BoundedText::new("r").unwrap(),
        },
    ];
    for op in &surface {
        let wire = serde_json::to_value(op.to_wire()).unwrap();
        let name = wire.as_object().map(|m| m.keys().next().cloned());
        if let Some(Some(name)) = name {
            assert!(
                !wire_names.contains(&name),
                "the supervisor surface cannot produce {name}"
            );
        }
    }
}

#[test]
fn supervisor_holds_no_shell_file_or_workspace_authority() {
    // DoD row 2 / SA-29/SA-30/N-R2 boundary 2: no admitted Supervisor
    // profile contains shell/file_write/file_edit, workspace authority,
    // or any direct-execution capability; the transitional trio is a
    // PARENT-kernel marking, never a Supervisor grant.
    for banned in ["shell", "file_write", "file_edit"] {
        assert!(
            SubAgentToolNameV1::parse(banned).is_err(),
            "{banned} is refused at the tool-name type"
        );
    }
    // A supervisor profile declaring ANY tool is refused admission (the
    // V1 child catalog is empty — stronger than the SA-29 minimum).
    let mut tooled = SubAgentProfileRegistry::default_supervisor_profile();
    tooled.profile_id = "supervisor-tooled".into();
    tooled.tool_policy.tools = vec![SubAgentToolNameV1::parse("read_context").unwrap()];
    tooled.digest = tooled.compute_digest();
    let mut registry = SubAgentProfileRegistry::new();
    assert!(matches!(
        registry.admit(tooled),
        Err(crate::subagent_v1::ProfileAdmissionError::NonEmptyToolPolicy { .. })
    ));

    // The session inventory's serialized key set is the
    // negative-capability evidence: no credential/shell/workspace/
    // tool-registry/spawn field can appear without breaking this test.
    let rig = Rig::new();
    let inventory = session(&rig).inventory();
    let value = serde_json::to_value(&inventory).unwrap();
    let mut keys: Vec<String> = value
        .as_object()
        .expect("inventory serializes as an object")
        .keys()
        .cloned()
        .collect();
    keys.sort_unstable();
    let mut expected = vec![
        "profile_id",
        "role",
        "granted_authorities",
        "supervisor_ref",
        "phase",
        "bridge_operations",
        "budget_max_actions",
        "outbound_channel",
    ];
    expected.sort_unstable();
    let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
    assert_eq!(keys, expected);
    let rendered = format!("{value}");
    for banned in ["shell", "workspace", "credential", "tool_registry", "spawn"] {
        assert!(
            !rendered.contains(banned),
            "the supervisor inventory must not surface {banned}"
        );
    }
    assert_eq!(inventory.outbound_channel, "structured-report-only");
}

// ─── the four discriminations ────────────────────────────────────────────

#[tokio::test]
async fn discrimination_continuation_is_not_independent_review() {
    // Discrimination 1 (TB-17 + #207 suite 7 joint test): an
    // independence-marked requirement satisfied only by a continuation
    // is rejected BOTH by the bridge client (pre-check) and by the
    // server-side law (typed refusal, zero mutation).
    let rig = Rig::new();
    let mut supervisor = session(&rig);
    parent_submits_planned_task(&mut supervisor, &rig).await;

    // CLIENT PRE-CHECK: the supervisor's independent-review surface
    // refuses a continuation (and a deterministic check) as the review's
    // class — before any transport call, before any task exists.
    for class in [IC::SameSessionContinuation, IC::DeterministicCheck] {
        let err = supervisor
            .request_independent_review(class, "review the artifact")
            .await
            .expect_err("non-independence-marked class must be refused");
        assert!(
            matches!(
                err,
                super::SupervisorFlowError::ClassNotIndependenceMarked { .. }
            ),
            "wrong error: {err}"
        );
    }
    // And the frozen law itself: a continuation satisfies nothing marked.
    for required in [
        IC::FreshContextSameHarness,
        IC::FreshContextCrossModelSameVendor,
        IC::FreshContextCrossVendor,
        IC::HumanReview,
    ] {
        assert!(!IC::SameSessionContinuation.satisfies_requirement(required));
    }

    // SERVER-SIDE: the session-intervention path refuses
    // RequestIndependentReview with `requires_new_task_lineage` —
    // zero mutation, no fresh-task fallback. This is exactly why the
    // supervisor's mapping SUBMITS a new review task instead.
    let client = rig.client();
    // Submit a fresh task so the intervene target exists.
    let intent_inputs = TaskIntentInputs {
        objective: BoundedText::new("target for the server-side law probe").unwrap(),
        capability_request: CapabilityRequest {
            capability: Capability::ReasoningReview,
        },
        constraints: vec![],
        expected_artifacts: vec![],
        evaluation_requirement: EvaluationRequirement {
            independence: IC::FreshContextCrossVendor,
        },
    };
    let context = StructuralIntentContext {
        requester: requester(),
        parent_ref: None,
        supervisor_ref: None,
        context_bundle_ref: BoundedText::new("bundle-probe").unwrap(),
        source_refs: vec![],
        expiry: None,
        retry_of: None,
    };
    let intent = crate::tachi_bridge::compose_intent(
        &intent_inputs,
        &RequesterBridgePolicy {
            admitted_capabilities: BTreeSet::from([Capability::ReasoningReview]),
            workspace_source: None,
            routing_preference: None,
            approval_requirement: zeroclaw_api::taskintent::ApprovalRequirement::NotRequired,
            privacy_class: zeroclaw_api::taskintent::PrivacyClass::Internal,
        },
        &context,
    )
    .unwrap();
    let rid = RequestId::new("probe-review-1").unwrap();
    let target = match client.submit(&intent, &rid).await.unwrap() {
        SubmitReceipt::Admitted { task_ref, .. } => task_ref,
        other => panic!("probe submit failed: {other:?}"),
    };
    let tasks_before = rig.bridge.task_count();
    let err = client
        .intervene(
            &target,
            &InterventionV1::RequestIndependentReview {
                independence_class: IC::FreshContextCrossVendor,
            },
            &requester(),
            &RequestId::new("probe-review-2").unwrap(),
            None,
        )
        .await
        .expect_err("server-side law refuses the session intervention");
    assert!(
        matches!(err, InterventionError::RequiresNewTaskLineage { .. }),
        "wrong error: {err}"
    );
    // Zero mutation: the refusal created no new task and no new facts.
    assert_eq!(rig.bridge.task_count(), tasks_before);
    let probe_snapshot = client.get(&target).await.expect("snapshot");
    let after_snapshot = client.get(&target).await.expect("snapshot 2");
    assert_eq!(probe_snapshot.task_revision, after_snapshot.task_revision);

    // A CONTINUATION receipt is a continuation fact — the receipt type
    // itself answers "is this an independent review?" with no.
    let continuation = InterventionReceipt::ContinuationRequested {
        intervention_id: "iv-1".into(),
    };
    assert!(continuation.is_continuation());
    // And the ONLY constructor of a ReviewLineageRecord is the
    // independent-review mapping; there is no from-receipt conversion
    // (compile-level: no such API exists).
}

#[tokio::test]
async fn discrimination_same_session_is_not_independence_but_fresh_context_same_harness_is() {
    // Discrimination 2 (TB-17 classes): a review executed in the same
    // harness session as the implementation is a SameSessionContinuation
    // and can never satisfy an independence-marked requirement — while
    // FreshContextSameHarness (fresh context, same harness) remains a
    // VALID distinct frozen class. Independence keys on context/attempt
    // lineage and is recorded explicitly, never assumed from harness
    // identity.
    let rig = Rig::new();
    let mut supervisor = session(&rig);
    parent_submits_planned_task(&mut supervisor, &rig).await;

    // A fresh-context same-harness review is admitted and recorded.
    let record = supervisor
        .request_independent_review(IC::FreshContextSameHarness, "review the artifact")
        .await
        .expect("fresh-context same-harness review admits");
    // The independence class is RECORDED on the review task's lineage…
    assert_eq!(record.independence_class, IC::FreshContextSameHarness);
    // …on a context lineage DISTINCT from the implementation task's.
    assert_ne!(record.context_bundle_ref, record.implementation_bundle_ref);
    // It satisfies its own class and anything weaker-or-equal it covers.
    assert!(record.satisfies(IC::FreshContextSameHarness));
    // It does NOT satisfy a stricter requirement.
    assert!(!record.satisfies(IC::FreshContextCrossVendor));
    // A SameSessionContinuation-class "review" is refused outright
    // (the same-session case cannot even be requested as a class).
    assert!(matches!(
        supervisor
            .request_independent_review(IC::SameSessionContinuation, "x")
            .await,
        Err(super::SupervisorFlowError::ClassNotIndependenceMarked { .. })
    ));
    // Lineage forgery guard: a record whose bundle ref COLLAPSED onto
    // the implementation's does not satisfy, whatever its class label.
    let forged = super::ReviewLineageRecord {
        review_task: record.review_task.clone(),
        review_of: record.review_of.clone(),
        independence_class: IC::FreshContextCrossVendor,
        context_bundle_ref: record.implementation_bundle_ref.clone(),
        implementation_bundle_ref: record.implementation_bundle_ref.clone(),
        request_id: record.request_id.clone(),
    };
    assert!(!forged.satisfies(IC::FreshContextCrossVendor));
    // Session-level acceptance gate agrees.
    assert!(supervisor.latest_review_satisfies(IC::FreshContextSameHarness));
    assert!(!supervisor.latest_review_satisfies(IC::HumanReview));
}

#[tokio::test]
async fn discrimination_worker_success_is_not_adjudication() {
    // Discrimination 3 (TB-13/TB-18): a worker "success" without the
    // required artifact/evidence does not satisfy the evaluation
    // contract; the Task verdict comes from Tachi adjudication state,
    // not worker prose. Worker/process success is a fact, not the
    // verdict.
    let rig = Rig::new();
    let mut supervisor = session(&rig);
    let task = parent_submits_planned_task(&mut supervisor, &rig).await;

    // The worker reports SUCCESS but provides NO verification evidence
    // for the required artifacts.
    rig.bridge.observe_outcome(
        &task,
        attempt(1),
        "success",
        Some("artifact:claimed".into()),
        vec!["evidence:claimed".into()],
        false,
        false,
        "vendor=test; model=stub; basis=reported",
    );
    let projection = supervisor
        .collect_result(&task)
        .await
        .expect("collect works");
    // The worker's classification is exactly that — a claim.
    assert_eq!(projection.terminal_classification, "success");
    // The contract check is artifact-based, NOT prose-based.
    assert!(
        !projection.contract_violations.is_empty(),
        "worker prose must not satisfy the artifact contract"
    );
    // The adjudication dimension is the verdict source and it has NOT
    // accepted anything yet.
    assert_eq!(projection.adjudication.label(), "unreviewed");

    // The supervisor's judgment reads ONLY the adjudication dimension.
    let proposal = supervisor.propose_judgment(&task).await.expect("proposal");
    assert_eq!(proposal.adjudication_observed, "unreviewed");
    assert_eq!(proposal.terminal_classification_observed, "success");
    assert!(
        proposal.proposed_interpretation.contains("not the verdict"),
        "the proposal must state the classification is not the verdict"
    );
}

#[tokio::test]
async fn discrimination_supervisor_judgment_is_not_canonical_eval_truth() {
    // Discrimination 4 (SA-29/KP-21/TB-13): ProposeJudgment produces a
    // receipt-bound proposal; a request is not a lifecycle transition;
    // canonical adjudication/eval state is Tachi's.
    let rig = Rig::new();
    let mut supervisor = session(&rig);
    let task = parent_submits_planned_task(&mut supervisor, &rig).await;
    rig.bridge.observe_outcome(
        &task,
        attempt(1),
        "success",
        Some("artifact:a".into()),
        vec!["evidence:a".into()],
        true,
        true,
        "vendor=test; model=stub; basis=reported",
    );

    let before = supervisor.observe(&task).await.expect("snapshot before");
    let proposals_before = supervisor.judgment_proposals().len();
    let tasks_before = rig.bridge.task_count();

    let proposal = supervisor.propose_judgment(&task).await.expect("proposal");
    assert_eq!(proposal.task_ref, task);
    assert!(proposal.result_revision > 0, "bound to a result revision");

    // No lifecycle transition: the snapshot's adjudication is unchanged,
    // no new task was minted, the proposal count grew by exactly one.
    let after = supervisor.observe(&task).await.expect("snapshot after");
    assert_eq!(
        before.adjudication.label(),
        after.adjudication.label(),
        "a proposal must not touch canonical adjudication state"
    );
    assert_eq!(
        before.task_revision, after.task_revision,
        "a proposal must not append any fact"
    );
    assert_eq!(rig.bridge.task_count(), tasks_before);
    assert_eq!(supervisor.judgment_proposals().len(), proposals_before + 1);

    // The terminal report presents interpretation only (SA-21 channel).
    let report = supervisor.conclude();
    assert_eq!(
        report.status,
        zeroclaw_api::subagent_v1::SubAgentTerminalFact::Completed
    );
    assert!(report.summary.contains("Tachi-side adjudication facts"));
    assert!(report.requested_parent_actions.is_empty());
}

// ─── the loop + stop/cancel + correction ─────────────────────────────────

#[tokio::test]
async fn full_loop_runs_through_the_bridge_surface() {
    // DoD row 4's shape on the double: plan (typed parent action) →
    // parent submit → attach → observe → independent review (new task,
    // fresh lineage) → review outcome → adjudication (Tachi-side) →
    // judgment proposal → SupervisorReport. A correction + re-review
    // cycle rides `correction_cycle_runs_through_the_bridge_surface`.
    let rig = Rig::new();
    let mut supervisor = session(&rig);
    let task = parent_submits_planned_task(&mut supervisor, &rig).await;
    assert_eq!(supervisor.phase(), super::SupervisorPhase::Supervising);

    let record = supervisor
        .request_independent_review(IC::FreshContextCrossVendor, "review the diff and tests")
        .await
        .expect("review task created");
    assert_ne!(record.review_task, task, "distinct Task lineage");
    assert!(record.satisfies(IC::FreshContextCrossVendor));

    // The implementation worker completes WITH its required artifacts
    // (diff + verification evidence)…
    rig.bridge.observe_outcome(
        &task,
        attempt(1),
        "done",
        Some("artifact:impl-diff".into()),
        vec!["evidence:impl-tests".into()],
        true,
        true,
        "vendor=codex; model=codex; basis=attested",
    );
    // …the review worker completes WITH its review evidence; Tachi-side
    // adjudication then accepts the implementation task.
    rig.bridge.observe_outcome(
        &record.review_task,
        attempt(2),
        "done",
        Some("artifact:review".into()),
        vec!["evidence:review".into()],
        true,
        false,
        "vendor=codex; model=codex; basis=attested",
    );
    rig.bridge.ingest_adjudication(&task, "accepted");

    let impl_projection = supervisor.collect_result(&task).await.expect("collect");
    assert_eq!(impl_projection.adjudication.label(), "accepted");
    let proposal = supervisor.propose_judgment(&task).await.expect("proposal");
    assert_eq!(proposal.adjudication_observed, "accepted");

    assert_eq!(supervisor.phase(), super::SupervisorPhase::Supervising);
    let report = supervisor.conclude();
    assert!(report.usage.actions > 0, "bridge ops are billable actions");
    assert!(report.findings.iter().any(|f| {
        f.statement
            .contains(&record.review_task.as_wire().to_string())
    }));
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.statement.contains("adjudication `accepted`"))
    );
}

#[tokio::test]
async fn correction_cycle_runs_through_the_bridge_surface() {
    // DoD row 4: at least one correction + re-review cycle through the
    // #205 surface (TB-11 RequestCorrection then a NEW independent
    // review lineage).
    let rig = Rig::new();
    let mut supervisor = session(&rig);
    let task = parent_submits_planned_task(&mut supervisor, &rig).await;

    // First review completes and adjudication REJECTS.
    let first = supervisor
        .request_independent_review(IC::FreshContextCrossVendor, "review round one")
        .await
        .expect("first review");
    rig.bridge.observe_outcome(
        &first.review_task,
        attempt(1),
        "done",
        Some("artifact:review-1".into()),
        vec!["evidence:review-1".into()],
        true,
        false,
        "vendor=codex; model=codex; basis=attested",
    );
    rig.bridge.observe_outcome(
        &task,
        attempt(3),
        "done",
        Some("artifact:impl-diff-1".into()),
        vec!["evidence:impl-1".into()],
        true,
        true,
        "vendor=codex; model=codex; basis=attested",
    );
    rig.bridge.ingest_adjudication(&task, "rejected");

    // The owner leg declares correction support (TB-15: advertisement is
    // a typed, revisioned fact — the managed-lane baseline is stops
    // only, exactly like the real tachi LifecycleMode baseline).
    rig.bridge.declare_owner_capabilities(
        &task,
        &[
            InterventionStatic::RequestCorrection,
            InterventionStatic::RequestContinuation,
        ],
    );

    // RequestCorrection through the supervisor-gated surface.
    let receipt = supervisor
        .intervene_on_implementation(SupervisorIntervention::RequestCorrection {
            note: BoundedText::new("tighten the boundary check the review flagged").unwrap(),
        })
        .await
        .expect("correction requested");
    assert!(matches!(
        receipt,
        InterventionReceipt::CorrectionRequested { .. }
    ));

    // Re-review: a NEW review task on ANOTHER fresh lineage.
    let second = supervisor
        .request_independent_review(IC::FreshContextCrossVendor, "review round two")
        .await
        .expect("second review");
    assert_ne!(second.review_task, first.review_task);
    assert_ne!(second.context_bundle_ref, first.context_bundle_ref);
    rig.bridge.observe_outcome(
        &second.review_task,
        attempt(2),
        "done",
        Some("artifact:review-2".into()),
        vec!["evidence:review-2".into()],
        true,
        false,
        "vendor=codex; model=codex; basis=attested",
    );
    rig.bridge.ingest_adjudication(&task, "accepted");
    let proposal = supervisor.propose_judgment(&task).await.expect("proposal");
    assert_eq!(proposal.adjudication_observed, "accepted");
}

#[tokio::test]
async fn stop_is_receipt_bound_never_locally_cancelled() {
    // DoD row 9: a supervisor-driven cancel produces a receipt-bound
    // state, never a locally minted `cancelled`.
    let rig = Rig::new();
    let mut supervisor = session(&rig);
    let task = parent_submits_planned_task(&mut supervisor, &rig).await;

    let receipt = supervisor
        .intervene_on_implementation(SupervisorIntervention::RequestHardCancel {
            reason: BoundedText::new("scope withdrawn by the requester").unwrap(),
        })
        .await
        .expect("stop receipt");
    // The stop variants resolve to the single stop authority (TB-11);
    // the receipt is the multi-stage fact at a PRE-confirmation stage.
    let stop = match receipt {
        InterventionReceipt::Stop(stop) => stop,
        other => panic!("stop-alias must resolve to the stop authority, got {other:?}"),
    };
    assert_ne!(
        stop.stage,
        zeroclaw_api::taskintent::StopStage::Confirmed,
        "only the lifecycle owner can confirm a cancellation"
    );

    // The projection shows cancellation_requested — never `cancelled`.
    let snapshot = supervisor.observe(&task).await.expect("snapshot");
    assert_eq!(snapshot.execution.label(), "cancellation_requested");

    // TB-7 rule 6 replay through the SAME tuple returns the SAME stop.
    let client = rig.client();
    let rid = RequestId::new("stop-replay-1").unwrap();
    let first = client
        .request_stop(
            &task,
            zeroclaw_api::taskintent::StopMode::Hard,
            "replay",
            &requester(),
            &rid,
            None,
        )
        .await
        .expect("first stop");
    let replay = client
        .request_stop(
            &task,
            zeroclaw_api::taskintent::StopMode::Hard,
            "replay",
            &requester(),
            &rid,
            None,
        )
        .await
        .expect("replayed stop");
    assert_eq!(first.stop_id, replay.stop_id);
    // Still not cancelled after the replay.
    let snapshot = supervisor.observe(&task).await.expect("snapshot 2");
    assert_ne!(snapshot.execution.label(), "cancelled");
}

#[tokio::test]
async fn unsupported_intervention_is_a_typed_refusal_with_zero_mutation() {
    // TB-11: RequestCorrection without owner advertisement is refused
    // typed, with zero state mutation (no fresh-task fallback).
    let rig = Rig::new();
    let mut supervisor = session(&rig);
    let task = parent_submits_planned_task(&mut supervisor, &rig).await;
    let before = supervisor.observe(&task).await.expect("snapshot");
    let err = supervisor
        .intervene_on_implementation(SupervisorIntervention::RequestCorrection {
            note: BoundedText::new("not advertised").unwrap(),
        })
        .await
        .expect_err("unadvertised correction must be refused");
    assert!(
        matches!(
            err,
            super::SupervisorFlowError::Intervention(SupervisorInterventionError::Host(
                InterventionError::UnsupportedByLifecycleOwner { .. }
            ))
        ),
        "wrong error: {err}"
    );
    let after = supervisor.observe(&task).await.expect("snapshot 2");
    assert_eq!(before.task_revision, after.task_revision, "zero mutation");
    assert_eq!(rig.bridge.task_count(), 1, "no fresh-task fallback");
}

#[tokio::test]
async fn authority_not_granted_refuses_the_operation() {
    // Least-authority negative: a supervisor whose granted set omits an
    // authority cannot exercise it — refused before any transport call.
    let mut registry = registry();
    let mut reduced = SubAgentProfileRegistry::default_supervisor_profile();
    reduced.profile_id = "supervisor-reduced".into();
    reduced.supervisor_authority_set = vec![SupervisorAuthority::ObserveTask];
    reduced.digest = reduced.compute_digest();
    registry.admit(reduced).expect("reduced profile admits");
    let vref = registry
        .latest_ref("supervisor-reduced")
        .expect("reduced ref");

    let rig = Rig::new();
    let client = rig.client();
    let mut supervisor = SupervisorSessionV1::from_admitted_profile(
        &registry,
        &vref,
        &LineageRef::new_root(ParentRunRef::from_opaque("root-reduced")),
        client,
        requester(),
        None,
    )
    .expect("admits");
    let task = parent_submits_planned_task(&mut supervisor, &rig).await;
    assert!(matches!(
        supervisor
            .request_independent_review(IC::FreshContextCrossVendor, "x")
            .await,
        Err(super::SupervisorFlowError::AuthorityNotGranted {
            authority: SupervisorAuthority::RequestIndependentReview
        })
    ));
    assert!(matches!(
        supervisor.collect_result(&task).await,
        Err(super::SupervisorFlowError::AuthorityNotGranted {
            authority: SupervisorAuthority::ReadResultRefs
        })
    ));
}

#[tokio::test]
async fn wrong_phase_transitions_are_refused() {
    let rig = Rig::new();
    let mut supervisor = session(&rig);
    // Nothing planned yet: attach refuses.
    let stray: TaskRef = serde_json::from_value(serde_json::json!("task:stray")).expect("ref");
    assert!(matches!(
        supervisor.attach_implementation_task(stray),
        Err(super::SupervisorFlowError::WrongPhase { .. })
    ));
    // Plan twice refuses (the second plan is a new submission in
    // waiting; the flow demands attach first).
    let _ = supervisor
        .plan_implementation_task(&implementation_inputs(), &parent_policy())
        .unwrap();
    assert!(matches!(
        supervisor.plan_implementation_task(&implementation_inputs(), &parent_policy()),
        Err(super::SupervisorFlowError::WrongPhase { .. })
    ));
    // Review before attach refuses with the no-task error.
    assert!(matches!(
        supervisor
            .request_independent_review(IC::FreshContextCrossVendor, "x")
            .await,
        Err(super::SupervisorFlowError::NoImplementationTask)
    ));
}

#[test]
fn supervisor_module_holds_no_local_spawn_or_execution_surface() {
    // SA-12 = D1 and N-R2 boundary 2, source-scan halves: the supervisor
    // module contains no process/spawn/filesystem-write surface, and
    // nothing durable (no sqlite, no file writes). The compile-level
    // half is the session's field set (pinned by the inventory test).
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let source = std::fs::read_to_string(format!("{manifest_dir}/src/supervisor_v1/mod.rs"))
        .expect("supervisor source");
    for banned in [
        "std::process",
        "tokio::process",
        "process::Command",
        "tokio::spawn",
        "spawn_subagent",
        "reasoning_subagent",
        "std::fs::write",
        "OpenOptions",
        "rusqlite",
        "zeroclaw_eval",
    ] {
        assert!(
            !source.contains(banned),
            "supervisor_v1 must not contain {banned} (SA-12/SA-26/SA-29/TB-22)"
        );
    }
}

#[test]
fn typed_submit_action_carries_the_composed_intent() {
    // The typed parent action is complete: the PARENT can submit from
    // the payload alone (SA-21/SA-25 + DoD row 2's parent-submits law),
    // and only the submit_task_intent kind carries the payload.
    let rig = Rig::new();
    let mut supervisor = session(&rig);
    let action = supervisor
        .plan_implementation_task(&implementation_inputs(), &parent_policy())
        .expect("plan");
    assert_eq!(action.action, ParentActionKind::SubmitTaskIntent);
    assert!(ParentActionKind::SubmitTaskIntent.requires_task_intent_payload());
    assert!(!ParentActionKind::AskUser.requires_task_intent_payload());
    let payload = action.task_intent_request.clone().expect("payload present");
    assert_eq!(
        payload.intent.capability_request.capability,
        Capability::RepositoryImplementation
    );
    // The intent names the supervising run (TB-14 supervisor_ref).
    assert!(payload.intent.supervisor_ref.is_some());
    assert!(
        payload
            .intent
            .supervisor_ref
            .as_ref()
            .is_some_and(|r| r.as_wire().starts_with("subrun:"))
    );
    // The request id is supervisor-minted and replayable as a tuple.
    assert!(payload.request_id.to_string().starts_with("supv-"));
    // A wire round-trip keeps the payload (SA-21 report hygiene).
    let wire = serde_json::to_value(&action).expect("serializes");
    let back: zeroclaw_api::subagent_v1::RequestedParentAction =
        serde_json::from_value(wire).expect("decodes");
    assert_eq!(back, action);
}

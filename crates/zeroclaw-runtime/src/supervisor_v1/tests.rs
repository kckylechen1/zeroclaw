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
                .await
                .expect("attach — digest matches the planned intent");
            // Round 0 of the implementation: completed WITHOUT
            // verification (the state a first review reacts to), and
            // the session observes it — reviews are bound to the
            // implementation revision they review.
            rig.bridge.observe_outcome(
                &task_ref,
                attempt(0),
                "done",
                Some("artifact:impl-r0".into()),
                vec!["evidence:impl-r0".into()],
                false,
                true,
                "vendor=codex; model=codex; basis=attested",
            );
            let _ = supervisor
                .collect_result(&task_ref)
                .await
                .expect("implementation result observed");
            task_ref
        }
        other => panic!("parent submit not admitted: {other:?}"),
    }
}

/// Session-level satisfaction of the LATEST review record (the gate the
/// Parent consults), including implementation-observation corroboration.
fn record_satisfies(supervisor: &SupervisorSessionV1, required: IC) -> bool {
    supervisor.latest_review_satisfies(required)
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
    // Discrimination 1 (TB-17 joint test, adjudication suite 7): an
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
    assert_eq!(record.independence_class(), IC::FreshContextSameHarness);
    // …on a context lineage DISTINCT from the implementation task's.
    assert_ne!(
        record.context_bundle_ref(),
        record.implementation_bundle_ref()
    );
    // ADMISSION IS NOT COMPLETION: before the review's result is
    // observed, the record satisfies NOTHING (fail closed).
    assert!(!record.satisfies(IC::FreshContextSameHarness));
    assert!(!supervisor.latest_review_satisfies(IC::FreshContextSameHarness));
    // A SameSessionContinuation-class "review" is refused outright
    // (the same-session case cannot even be requested as a class).
    assert!(matches!(
        supervisor
            .request_independent_review(IC::SameSessionContinuation, "x")
            .await,
        Err(super::SupervisorFlowError::ClassNotIndependenceMarked { .. })
    ));
    // The review completes; the session records the observation and the
    // gate opens FOR ITS OWN CLASS ONLY.
    rig.bridge.observe_outcome(
        record.review_task(),
        attempt(9),
        "done",
        Some("artifact:review-sh".into()),
        vec!["evidence:review-sh".into()],
        true,
        false,
        "vendor=glm; model=glm; basis=attested",
    );
    let _review_observation = supervisor
        .observe_review_result(record.review_task())
        .await
        .expect("observation recorded (self-collected)");
    assert!(record_satisfies(&supervisor, IC::FreshContextSameHarness));
    // It does NOT satisfy a stricter requirement (fresh-context
    // same-harness cannot stand in for cross-vendor).
    let latest = supervisor.review_records().last().unwrap();
    assert!(!latest.satisfies(IC::FreshContextCrossVendor));
    // Lineage forgery guard (red-team seam): a forged record whose
    // bundle ref COLLAPSED onto the implementation's, or whose review
    // task EQUALS the reviewed task, satisfies nothing — whatever its
    // class label or observation state.
    let observed = super::ResultObservation {
        result_revision: 1,
        provenance_vendor: "glm".into(),
        provenance_model: "glm".into(),
        terminal_classification: "done".into(),
    };
    let implementation = super::ResultObservation {
        result_revision: 1,
        provenance_vendor: "codex".into(),
        provenance_model: "codex".into(),
        terminal_classification: "done".into(),
    };
    let collapsed = super::ReviewLineageRecord::forge_for_test(
        record.review_task().clone(),
        record.review_of().clone(),
        IC::FreshContextCrossVendor,
        record.implementation_bundle_ref().to_string(),
        record.implementation_bundle_ref().to_string(),
        record.request_id().to_string(),
        implementation.clone(),
        Some(observed.clone()),
    );
    assert!(!collapsed.satisfies(IC::FreshContextCrossVendor));
    let self_review = super::ReviewLineageRecord::forge_for_test(
        record.review_task().clone(),
        record.review_task().clone(),
        IC::FreshContextCrossVendor,
        "bundle-x".into(),
        record.implementation_bundle_ref().to_string(),
        record.request_id().to_string(),
        implementation,
        Some(observed),
    );
    assert!(!self_review.satisfies(IC::FreshContextCrossVendor));
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
    assert_ne!(record.review_task(), &task, "distinct Task lineage");
    // Admission is not completion: nothing satisfied yet.
    assert!(!supervisor.latest_review_satisfies(IC::FreshContextCrossVendor));

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
    // …the review worker (a DIFFERENT vendor) completes WITH its review
    // evidence; Tachi-side adjudication then accepts the implementation
    // task.
    rig.bridge.observe_outcome(
        record.review_task(),
        attempt(2),
        "done",
        Some("artifact:review".into()),
        vec!["evidence:review".into()],
        true,
        false,
        "vendor=glm; model=glm; basis=attested",
    );
    rig.bridge.ingest_adjudication(&task, "accepted");

    // Observe BOTH results through the session; the cross-vendor claim
    // is then corroborated (codex implementation, glm review).
    let impl_projection = supervisor.collect_result(&task).await.expect("collect");
    let _review_observation = supervisor
        .observe_review_result(record.review_task())
        .await
        .expect("observation recorded (self-collected)");
    assert!(supervisor.latest_review_satisfies(IC::FreshContextCrossVendor));
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
    // bridge surface (TB-11 RequestCorrection then a NEW independent
    // review lineage).
    let rig = Rig::new();
    let mut supervisor = session(&rig);
    let task = parent_submits_planned_task(&mut supervisor, &rig).await;

    // First review completes and adjudication REJECTS.
    let first = supervisor
        .request_independent_review(IC::FreshContextSameHarness, "review round one")
        .await
        .expect("first review");
    rig.bridge.observe_outcome(
        first.review_task(),
        attempt(1),
        "done",
        Some("artifact:review-1".into()),
        vec!["evidence:review-1".into()],
        true,
        false,
        "vendor=glm; model=glm; basis=attested",
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
    let _first_observation = supervisor
        .observe_review_result(first.review_task())
        .await
        .expect("observation recorded (self-collected)");

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

    // The CORRECTED implementation result lands BEFORE the re-review:
    // verification now present, a NEW result revision the re-review is
    // bound to (the round-1 result had none — see the projection above).
    rig.bridge.observe_outcome(
        &task,
        attempt(3),
        "done",
        Some("artifact:impl-diff-1".into()),
        vec!["evidence:impl-2-verified".into()],
        true,
        true,
        "vendor=codex; model=codex; basis=attested",
    );
    let impl_round2 = supervisor
        .collect_result(&task)
        .await
        .expect("collect impl r2");
    assert!(impl_round2.contract_violations.is_empty());

    // Re-review: a NEW review task on ANOTHER fresh lineage.
    let second = supervisor
        .request_independent_review(IC::FreshContextSameHarness, "review round two")
        .await
        .expect("second review");
    assert_ne!(second.review_task(), first.review_task());
    assert_ne!(second.context_bundle_ref(), first.context_bundle_ref());
    rig.bridge.observe_outcome(
        second.review_task(),
        attempt(2),
        "done",
        Some("artifact:review-2".into()),
        vec!["evidence:review-2".into()],
        true,
        false,
        "vendor=glm; model=glm; basis=attested",
    );
    rig.bridge.ingest_adjudication(&task, "accepted");
    let _second_observation = supervisor
        .observe_review_result(second.review_task())
        .await
        .expect("observation recorded (self-collected)");
    assert!(supervisor.latest_review_satisfies(IC::FreshContextSameHarness));
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
    // The reduced session plans; the parent submits and attaches —
    // WITHOUT the observation step (this session cannot even read the
    // implementation result: that is the point).
    let action = supervisor
        .plan_implementation_task(&implementation_inputs(), &parent_policy())
        .expect("plan");
    let payload = action.task_intent_request.clone().expect("payload");
    let task = match rig
        .client()
        .submit(&payload.intent, &payload.request_id)
        .await
        .expect("parent submit")
    {
        SubmitReceipt::Admitted { task_ref, .. } => {
            supervisor
                .attach_implementation_task(task_ref.clone())
                .await
                .expect("attach");
            task_ref
        }
        other => panic!("submit failed: {other:?}"),
    };
    assert!(matches!(
        supervisor.collect_result(&task).await,
        Err(super::SupervisorFlowError::AuthorityNotGranted {
            authority: SupervisorAuthority::ReadResultRefs
        })
    ));
    assert!(matches!(
        supervisor
            .request_independent_review(IC::FreshContextCrossVendor, "x")
            .await,
        Err(super::SupervisorFlowError::AuthorityNotGranted {
            authority: SupervisorAuthority::RequestIndependentReview
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
        supervisor.attach_implementation_task(stray).await,
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
    // The sqlite and file-open markers are assembled by concatenation
    // so THIS file does not itself trip the persistence-surface gate's
    // detector (the gate greps the whole tree).
    let sqlite_marker = ["rusql", "ite"].concat();
    let open_options = ["Open", "Options"].concat();
    for banned in [
        "std::process",
        "tokio::process",
        "process::Command",
        "tokio::spawn",
        "spawn_subagent",
        "reasoning_subagent",
        "std::fs::write",
        open_options.as_str(),
        sqlite_marker.as_str(),
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

#[tokio::test]
async fn attach_refuses_a_task_whose_intent_digest_is_not_the_planned_one() {
    // Codex round-1 finding 5, enforcement half: attach_implementation_task
    // is receipt-bound — a TaskRef whose snapshot carries a DIFFERENT
    // intent digest than the planned intent is refused, so the session
    // can only supervise the task the Parent actually submitted from the
    // planned intent.
    let rig = Rig::new();
    let mut supervisor = session(&rig);
    let _ = supervisor
        .plan_implementation_task(&implementation_inputs(), &parent_policy())
        .expect("plan");
    // An unrelated task exists on the host (different intent).
    let unrelated_inputs = TaskIntentInputs {
        objective: zeroclaw_api::taskintent::BoundedText::new("an unrelated objective").unwrap(),
        capability_request: CapabilityRequest {
            capability: Capability::RepositoryImplementation,
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
        context_bundle_ref: zeroclaw_api::taskintent::BoundedText::new("bundle-unrelated").unwrap(),
        source_refs: vec![],
        expiry: None,
        retry_of: None,
    };
    let unrelated_intent =
        crate::tachi_bridge::compose_intent(&unrelated_inputs, &parent_policy(), &context).unwrap();
    let stray = match rig
        .client()
        .submit(&unrelated_intent, &RequestId::new("stray-1").unwrap())
        .await
        .unwrap()
    {
        SubmitReceipt::Admitted { task_ref, .. } => task_ref,
        other => panic!("stray submit failed: {other:?}"),
    };
    let err = supervisor
        .attach_implementation_task(stray.clone())
        .await
        .expect_err("unrelated task must be refused");
    assert!(
        matches!(
            err,
            super::SupervisorFlowError::AttachedTaskDigestMismatch { .. }
        ),
        "wrong error: {err}"
    );
    // The session is still in Planning (nothing attached).
    assert_eq!(supervisor.phase(), super::SupervisorPhase::Planning);
}

#[tokio::test]
async fn ambiguous_review_submit_replays_the_same_tuple_never_a_new_id() {
    // Codex round-1 finding 4: a lost submit response for a review task
    // is resolved by REPLAYING the exact same (intent, request_id) tuple
    // — the session retains it as pending, and the next call replays it
    // verbatim. No second task is ever minted for one submission. The
    // injector drops the first SUBMIT_RECONCILE_ATTEMPTS responses (the
    // whole first reconciling call), then passes through.
    use parking_lot::Mutex as TestMutex;
    struct DropFirstSubmits {
        inner: Arc<InMemoryTachiTaskBridge>,
        remaining: TestMutex<usize>,
    }
    #[async_trait::async_trait]
    impl TachiTaskBridge for DropFirstSubmits {
        async fn submit(
            &self,
            intent: &zeroclaw_api::taskintent::TaskIntentV1,
            request_id: &RequestId,
        ) -> Result<SubmitReceipt, crate::tachi_bridge::SubmitTransportError> {
            let should_drop = {
                let mut remaining = self.remaining.lock();
                if *remaining > 0 {
                    *remaining -= 1;
                    true
                } else {
                    false
                }
            };
            if should_drop {
                // The host commit happens inside; the client never sees it.
                let _ = self.inner.submit(intent, request_id).await;
                return Err(crate::tachi_bridge::SubmitTransportError);
            }
            self.inner.submit(intent, request_id).await
        }
        async fn get(
            &self,
            task_ref: &zeroclaw_api::taskintent::TaskRef,
        ) -> Result<crate::tachi_bridge::TaskSnapshotView, crate::tachi_bridge::BridgeQueryError>
        {
            self.inner.get(task_ref).await
        }
        async fn watch(
            &self,
            task_ref: &zeroclaw_api::taskintent::TaskRef,
            after_seq: u64,
            limit: usize,
        ) -> Result<crate::tachi_bridge::TaskEventPageView, crate::tachi_bridge::BridgeQueryError>
        {
            self.inner.watch(task_ref, after_seq, limit).await
        }
        async fn collect(
            &self,
            task_ref: &zeroclaw_api::taskintent::TaskRef,
            result_revision: Option<u64>,
        ) -> Result<crate::tachi_bridge::ResultProjectionView, crate::tachi_bridge::BridgeQueryError>
        {
            self.inner.collect(task_ref, result_revision).await
        }
        async fn intervene(
            &self,
            task_ref: &zeroclaw_api::taskintent::TaskRef,
            intervention: &zeroclaw_api::taskintent::InterventionV1,
            requester: &zeroclaw_api::taskintent::RequesterRef,
            request_id: &RequestId,
            expected_task_revision: Option<u64>,
        ) -> Result<
            zeroclaw_api::taskintent::InterventionReceipt,
            zeroclaw_api::taskintent::InterventionError,
        > {
            self.inner
                .intervene(
                    task_ref,
                    intervention,
                    requester,
                    request_id,
                    expected_task_revision,
                )
                .await
        }
        async fn request_stop(
            &self,
            task_ref: &zeroclaw_api::taskintent::TaskRef,
            mode: zeroclaw_api::taskintent::StopMode,
            requester: &zeroclaw_api::taskintent::RequesterRef,
            request_id: &RequestId,
            expected_task_revision: Option<u64>,
        ) -> Result<
            zeroclaw_api::taskintent::StopReceipt,
            zeroclaw_api::taskintent::InterventionError,
        > {
            self.inner
                .request_stop(
                    task_ref,
                    mode,
                    requester,
                    request_id,
                    expected_task_revision,
                )
                .await
        }
    }
    let inner = Arc::new(InMemoryTachiTaskBridge::new());
    let dropping = Arc::new(DropFirstSubmits {
        inner: Arc::clone(&inner),
        remaining: TestMutex::new(crate::tachi_bridge::client::SUBMIT_RECONCILE_ATTEMPTS),
    });
    // Phase 1: plan + parent submit go through the plain host (no drop).
    let plain_rig = Rig {
        bridge: Arc::clone(&inner),
    };
    let mut supervisor = session(&plain_rig);
    let task = parent_submits_planned_task(&mut supervisor, &plain_rig).await;
    let _ = task;

    // Swap the session onto the dropping transport by replaying the flow
    // with a second session sharing the SAME host but wrapped client.
    let dropping_client = TachiBridgeClient::new(Arc::clone(&dropping) as Arc<dyn TachiTaskBridge>);
    let registry = registry();
    let mut supervisor2 = SupervisorSessionV1::from_admitted_profile(
        &registry,
        &supervisor_vref(&registry),
        &LineageRef::new_root(ParentRunRef::from_opaque("root-ambiguous")),
        dropping_client,
        requester(),
        None,
    )
    .expect("admits");
    let action = supervisor2
        .plan_implementation_task(&implementation_inputs(), &parent_policy())
        .expect("plan");
    let payload = action.task_intent_request.clone().expect("payload");
    match plain_rig
        .client()
        .submit(&payload.intent, &payload.request_id)
        .await
        .unwrap()
    {
        SubmitReceipt::Admitted { task_ref, .. } => {
            supervisor2
                .attach_implementation_task(task_ref.clone())
                .await
                .expect("attach");
            inner.observe_outcome(
                &task_ref,
                serde_json::from_value(serde_json::json!("attempt:test-0000")).unwrap(),
                "done",
                Some("artifact:impl-r0".into()),
                vec!["evidence:impl-r0".into()],
                false,
                true,
                "vendor=codex; model=codex; basis=attested",
            );
            let _ = supervisor2
                .collect_result(&task_ref)
                .await
                .expect("implementation result observed");
        }
        other => panic!("submit failed: {other:?}"),
    }
    let tasks_before = inner.task_count();
    let bindings_before = inner.binding_count();

    // First review submit: the transport DROPS the response after the
    // host committed — surfaced typed; the tuple is retained as pending.
    let err = supervisor2
        .request_independent_review(IC::FreshContextSameHarness, "review under ambiguity")
        .await
        .expect_err("ambiguous submit surfaces a typed refusal");
    assert!(
        matches!(err, super::SupervisorFlowError::ReviewSubmitRefused { .. }),
        "wrong error: {err}"
    );
    // The host DID commit exactly one binding for it.
    assert_eq!(inner.binding_count(), bindings_before + 1);
    assert_eq!(inner.task_count(), tasks_before + 1);

    // The retry REPLAYS the same tuple: the host returns the SAME
    // TaskRef (replayed admission), no second binding, no second task.
    let replayed = supervisor2
        .request_independent_review(
            IC::FreshContextSameHarness,
            "any args — the pending tuple governs",
        )
        .await
        .expect("replay reconciles to the one task");
    assert_eq!(inner.binding_count(), bindings_before + 1);
    assert_eq!(inner.task_count(), tasks_before + 1);
    // The replayed record is bound to the one committed task (the task
    // minted by the dropped commit itself, tasks_before + 1).
    assert_eq!(
        replayed.review_task().as_wire(),
        format!("task:inmem-{:08x}", tasks_before + 1)
    );
    // And a fresh review after resolution works normally.
    let _second = supervisor2
        .request_independent_review(IC::FreshContextSameHarness, "a genuinely new review")
        .await
        .expect("new review after resolution");
    assert_eq!(inner.binding_count(), bindings_before + 2);
    assert_eq!(inner.task_count(), tasks_before + 2);
}

#[tokio::test]
async fn cross_vendor_satisfaction_requires_vendor_corroboration() {
    // Codex round-1 finding 1: the recorded class label alone never
    // suffices — a fresh_context_cross_vendor requirement additionally
    // requires BOTH observed provenances with DIFFERENT vendors. A
    // same-vendor review (whatever its label) fails the gate.
    let rig = Rig::new();
    let mut supervisor = session(&rig);
    let task = parent_submits_planned_task(&mut supervisor, &rig).await;
    let record = supervisor
        .request_independent_review(IC::FreshContextCrossVendor, "review")
        .await
        .expect("review created (label claim)");

    // SAME vendor on both sides (codex implementing, codex reviewing).
    rig.bridge.observe_outcome(
        &task,
        attempt(1),
        "done",
        Some("artifact:i".into()),
        vec!["evidence:i".into()],
        true,
        true,
        "vendor=codex; model=codex; basis=attested",
    );
    rig.bridge.observe_outcome(
        record.review_task(),
        attempt(2),
        "done",
        Some("artifact:r".into()),
        vec!["evidence:r".into()],
        true,
        false,
        "vendor=codex; model=codex; basis=attested",
    );
    let impl_projection = supervisor
        .collect_result(&task)
        .await
        .expect("collect impl");
    let _observation = supervisor
        .observe_review_result(record.review_task())
        .await
        .expect("observed (self-collected)");
    assert!(
        !supervisor.latest_review_satisfies(IC::FreshContextCrossVendor),
        "same-vendor review must NOT corroborate a cross-vendor requirement"
    );
    // The same review DOES satisfy the weaker fresh-context same-harness
    // requirement (lineage + completion, no vendor constraint).
    assert!(supervisor.latest_review_satisfies(IC::FreshContextSameHarness));
    let _ = impl_projection;

    // With a DIFFERENT-vendor review, the cross-vendor gate opens.
    let record2 = supervisor
        .request_independent_review(IC::FreshContextCrossVendor, "cross review")
        .await
        .expect("second review");
    rig.bridge.observe_outcome(
        record2.review_task(),
        attempt(3),
        "done",
        Some("artifact:r2".into()),
        vec!["evidence:r2".into()],
        true,
        false,
        "vendor=glm; model=glm; basis=attested",
    );
    let _observation2 = supervisor
        .observe_review_result(record2.review_task())
        .await
        .expect("observed 2 (self-collected)");
    assert!(supervisor.latest_review_satisfies(IC::FreshContextCrossVendor));
}

#[tokio::test]
async fn review_observations_are_self_collected_only() {
    // Codex round-3 finding 1: the review gate opens ONLY through the
    // session's own collect — there is no caller-supplied projection
    // surface at all, so a caller-constructed ResultProjectionView
    // cannot complete a review (the type being publicly constructible
    // no longer matters: nothing accepts one).
    let rig = Rig::new();
    let mut supervisor = session(&rig);
    let task = parent_submits_planned_task(&mut supervisor, &rig).await;
    let review = supervisor
        .request_independent_review(IC::FreshContextSameHarness, "review")
        .await
        .expect("review");

    // The implementation task completes; the review does not.
    rig.bridge.observe_outcome(
        &task,
        attempt(1),
        "done",
        Some("artifact:i".into()),
        vec!["evidence:i".into()],
        true,
        true,
        "vendor=glm; model=glm; basis=attested",
    );
    let _ = supervisor
        .collect_result(&task)
        .await
        .expect("collect impl");
    // Gate still closed (no review observation).
    assert!(!supervisor.latest_review_satisfies(IC::FreshContextSameHarness));
    // The only way to open it is the session's own collect of the
    // review task's result.
    rig.bridge.observe_outcome(
        review.review_task(),
        attempt(2),
        "done",
        Some("artifact:r".into()),
        vec!["evidence:r".into()],
        true,
        false,
        "vendor=glm; model=glm; basis=attested",
    );
    let _observation = supervisor
        .observe_review_result(review.review_task())
        .await
        .expect("observed (self-collected)");
    assert!(supervisor.latest_review_satisfies(IC::FreshContextSameHarness));
    // An unknown review task is refused typed.
    let stray: TaskRef = serde_json::from_value(serde_json::json!("task:stray")).unwrap();
    assert!(matches!(
        supervisor.observe_review_result(&stray).await,
        Err(super::SupervisorFlowError::UnknownReviewTask { .. })
    ));
}

#[tokio::test]
async fn human_review_class_cannot_be_minted_by_this_surface() {
    // Codex round-2 finding 3: the supervisor submits machine-executed
    // review tasks (capability reasoning_review); a HumanReview class
    // can never be minted through it — the class is refused at request
    // time, and a forged HumanReview record satisfies nothing.
    let rig = Rig::new();
    let mut supervisor = session(&rig);
    parent_submits_planned_task(&mut supervisor, &rig).await;
    let err = supervisor
        .request_independent_review(IC::HumanReview, "review")
        .await
        .expect_err("human review class refused");
    assert!(
        matches!(
            err,
            super::SupervisorFlowError::ClassNotRequestableHere { .. }
        ),
        "wrong error: {err}"
    );
    // And even a forged record with a completed observation cannot
    // corroborate human participation from provenance.
    let forged = super::ReviewLineageRecord::forge_for_test(
        serde_json::from_value(serde_json::json!("task:r1")).unwrap(),
        serde_json::from_value(serde_json::json!("task:i1")).unwrap(),
        IC::HumanReview,
        "bundle-r".into(),
        "bundle-i".into(),
        "rid".into(),
        super::ResultObservation {
            result_revision: 1,
            provenance_vendor: "human".into(),
            provenance_model: String::new(),
            terminal_classification: "done".into(),
        },
        Some(super::ResultObservation {
            result_revision: 1,
            provenance_vendor: "codex".into(),
            provenance_model: "codex".into(),
            terminal_classification: "done".into(),
        }),
    );
    assert!(!forged.satisfies(IC::HumanReview));
}

#[tokio::test]
async fn duplicated_provenance_keys_and_case_tricks_cannot_forge_corroboration() {
    // Codex round-2 finding 2: a duplicated vendor key makes the whole
    // provenance malformed (fails closed), and case differences do not
    // create vendor distinctness.
    let rig = Rig::new();
    let mut supervisor = session(&rig);
    let task = parent_submits_planned_task(&mut supervisor, &rig).await;
    let review = supervisor
        .request_independent_review(IC::FreshContextCrossVendor, "review")
        .await
        .expect("review");
    rig.bridge.observe_outcome(
        &task,
        attempt(1),
        "done",
        Some("artifact:i".into()),
        vec!["evidence:i".into()],
        true,
        true,
        "vendor=codex; model=codex; basis=attested",
    );
    // Forged review provenance: duplicated vendor keys.
    rig.bridge.observe_outcome(
        review.review_task(),
        attempt(2),
        "done",
        Some("artifact:r".into()),
        vec!["evidence:r".into()],
        true,
        false,
        "vendor=codex; vendor=glm; model=x; basis=attested",
    );
    let _ = supervisor
        .collect_result(&task)
        .await
        .expect("collect impl");
    let _observation = supervisor
        .observe_review_result(review.review_task())
        .await
        .expect("observed (self-collected)");
    assert!(
        !supervisor.latest_review_satisfies(IC::FreshContextCrossVendor),
        "duplicated-key provenance must not corroborate"
    );
    // Case tricks: same vendor in different cases is the same vendor
    // (cannot fabricate cross-vendor), and different cases of the same
    // vendor do not fabricate cross-model either.
    let case_review = supervisor
        .request_independent_review(IC::FreshContextCrossVendor, "case review")
        .await
        .expect("review 2");
    rig.bridge.observe_outcome(
        case_review.review_task(),
        attempt(3),
        "done",
        Some("artifact:r2".into()),
        vec!["evidence:r2".into()],
        true,
        false,
        "vendor=CODEX; model=gpt; basis=attested",
    );
    let _case_observation = supervisor
        .observe_review_result(case_review.review_task())
        .await
        .expect("observed 2 (self-collected)");
    assert!(
        !supervisor.latest_review_satisfies(IC::FreshContextCrossVendor),
        "case-only vendor difference is not cross-vendor"
    );
}

#[tokio::test]
async fn corroborated_stronger_review_satisfies_weaker_requirement() {
    // Codex round-2 finding 4: corroboration branches on the RECORD's
    // class, then the frozen lattice decides adequacy — a corroborated
    // cross-vendor review satisfies the weaker cross-model-same-vendor
    // requirement, exactly as the lattice says.
    let rig = Rig::new();
    let mut supervisor = session(&rig);
    let task = parent_submits_planned_task(&mut supervisor, &rig).await;
    let review = supervisor
        .request_independent_review(IC::FreshContextCrossVendor, "cross review")
        .await
        .expect("review");
    rig.bridge.observe_outcome(
        &task,
        attempt(1),
        "done",
        Some("artifact:i".into()),
        vec!["evidence:i".into()],
        true,
        true,
        "vendor=codex; model=codex; basis=attested",
    );
    rig.bridge.observe_outcome(
        review.review_task(),
        attempt(2),
        "done",
        Some("artifact:r".into()),
        vec!["evidence:r".into()],
        true,
        false,
        "vendor=glm; model=glm; basis=attested",
    );
    let _ = supervisor
        .collect_result(&task)
        .await
        .expect("collect impl");
    let _observation = supervisor
        .observe_review_result(review.review_task())
        .await
        .expect("observed (self-collected)");
    assert!(supervisor.latest_review_satisfies(IC::FreshContextCrossVendor));
    assert!(
        supervisor.latest_review_satisfies(IC::FreshContextCrossModelSameVendor),
        "a corroborated stronger review satisfies the weaker requirement (lattice)"
    );
    assert!(supervisor.latest_review_satisfies(IC::FreshContextSameHarness));
}

#[tokio::test]
async fn typed_reconciliation_unknown_retains_the_replay_tuple() {
    // Codex round-2 finding 5: a TYPED ReconciliationUnknown receipt is
    // as ambiguous as a lost response — the exact (intent, request_id)
    // tuple is retained and replayed; no new id is minted.
    use parking_lot::Mutex as TestMutex;
    struct UnknownOnce {
        inner: Arc<InMemoryTachiTaskBridge>,
        fired: TestMutex<bool>,
    }
    #[async_trait::async_trait]
    impl TachiTaskBridge for UnknownOnce {
        async fn submit(
            &self,
            intent: &zeroclaw_api::taskintent::TaskIntentV1,
            request_id: &RequestId,
        ) -> Result<SubmitReceipt, crate::tachi_bridge::SubmitTransportError> {
            let fire = {
                let mut fired = self.fired.lock();
                if *fired {
                    false
                } else {
                    *fired = true;
                    true
                }
            };
            if fire {
                // The host commits; the client observes typed
                // ambiguity instead of the admission.
                let _ = self.inner.submit(intent, request_id).await;
                let digest = intent.canonical_digest();
                return Ok(SubmitReceipt::ReconciliationUnknown { digest });
            }
            self.inner.submit(intent, request_id).await
        }
        async fn get(
            &self,
            task_ref: &zeroclaw_api::taskintent::TaskRef,
        ) -> Result<crate::tachi_bridge::TaskSnapshotView, crate::tachi_bridge::BridgeQueryError>
        {
            self.inner.get(task_ref).await
        }
        async fn watch(
            &self,
            task_ref: &zeroclaw_api::taskintent::TaskRef,
            after_seq: u64,
            limit: usize,
        ) -> Result<crate::tachi_bridge::TaskEventPageView, crate::tachi_bridge::BridgeQueryError>
        {
            self.inner.watch(task_ref, after_seq, limit).await
        }
        async fn collect(
            &self,
            task_ref: &zeroclaw_api::taskintent::TaskRef,
            result_revision: Option<u64>,
        ) -> Result<crate::tachi_bridge::ResultProjectionView, crate::tachi_bridge::BridgeQueryError>
        {
            self.inner.collect(task_ref, result_revision).await
        }
        async fn intervene(
            &self,
            task_ref: &zeroclaw_api::taskintent::TaskRef,
            intervention: &zeroclaw_api::taskintent::InterventionV1,
            requester: &zeroclaw_api::taskintent::RequesterRef,
            request_id: &RequestId,
            expected_task_revision: Option<u64>,
        ) -> Result<
            zeroclaw_api::taskintent::InterventionReceipt,
            zeroclaw_api::taskintent::InterventionError,
        > {
            self.inner
                .intervene(
                    task_ref,
                    intervention,
                    requester,
                    request_id,
                    expected_task_revision,
                )
                .await
        }
        async fn request_stop(
            &self,
            task_ref: &zeroclaw_api::taskintent::TaskRef,
            mode: zeroclaw_api::taskintent::StopMode,
            requester: &zeroclaw_api::taskintent::RequesterRef,
            request_id: &RequestId,
            expected_task_revision: Option<u64>,
        ) -> Result<
            zeroclaw_api::taskintent::StopReceipt,
            zeroclaw_api::taskintent::InterventionError,
        > {
            self.inner
                .request_stop(
                    task_ref,
                    mode,
                    requester,
                    request_id,
                    expected_task_revision,
                )
                .await
        }
    }
    let inner = Arc::new(InMemoryTachiTaskBridge::new());
    let plain = Rig {
        bridge: Arc::clone(&inner),
    };
    let client = TachiBridgeClient::new(Arc::new(UnknownOnce {
        inner: Arc::clone(&inner),
        fired: TestMutex::new(false),
    }) as Arc<dyn TachiTaskBridge>);
    let registry = registry();
    let mut supervisor = SupervisorSessionV1::from_admitted_profile(
        &registry,
        &supervisor_vref(&registry),
        &LineageRef::new_root(ParentRunRef::from_opaque("root-typed-unknown")),
        client,
        requester(),
        None,
    )
    .expect("admits");
    let action = supervisor
        .plan_implementation_task(&implementation_inputs(), &parent_policy())
        .expect("plan");
    let payload = action.task_intent_request.clone().expect("payload");
    match plain
        .client()
        .submit(&payload.intent, &payload.request_id)
        .await
        .unwrap()
    {
        SubmitReceipt::Admitted { task_ref, .. } => {
            supervisor
                .attach_implementation_task(task_ref.clone())
                .await
                .expect("attach");
            inner.observe_outcome(
                &task_ref,
                serde_json::from_value(serde_json::json!("attempt:test-0000")).unwrap(),
                "done",
                Some("artifact:impl-r0".into()),
                vec!["evidence:impl-r0".into()],
                false,
                true,
                "vendor=codex; model=codex; basis=attested",
            );
            let _ = supervisor
                .collect_result(&task_ref)
                .await
                .expect("implementation result observed");
        }
        other => panic!("submit failed: {other:?}"),
    }
    let bindings_before = inner.binding_count();
    let tasks_before = inner.task_count();
    let err = supervisor
        .request_independent_review(IC::FreshContextSameHarness, "typed ambiguity probe")
        .await
        .expect_err("typed ambiguity surfaces");
    assert!(
        matches!(
            err,
            super::SupervisorFlowError::ReviewSubmitRefused {
                receipt: SubmitReceipt::ReconciliationUnknown { .. }
            }
        ),
        "wrong error: {err}"
    );
    // The host committed exactly one binding + task.
    assert_eq!(inner.binding_count(), bindings_before + 1);
    assert_eq!(inner.task_count(), tasks_before + 1);
    // The replay returns the SAME task (idempotent), no second anything.
    let replayed = supervisor
        .request_independent_review(IC::FreshContextSameHarness, "retry")
        .await
        .expect("typed replay reconciles");
    assert_eq!(inner.binding_count(), bindings_before + 1);
    assert_eq!(inner.task_count(), tasks_before + 1);
    assert_eq!(
        replayed.review_task().as_wire(),
        format!("task:inmem-{:08x}", tasks_before + 1)
    );
}

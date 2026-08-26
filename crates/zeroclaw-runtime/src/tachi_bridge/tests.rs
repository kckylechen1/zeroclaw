//! Stage-A suites for the Tachi bridge client vertical (V2b).
//!
//! Each suite names the ticket DoD row / contract clause it proves.
//! Stage-B (the live end-to-end run) lives outside this file — see the
//! PR body evidence section and the leaf ledger on the tracker.

use std::collections::BTreeSet;
use std::sync::Arc;

use zeroclaw_api::taskintent::{
    ArtifactClass, ArtifactExpectation, BoundedText, Capability, CapabilityRequest,
    EvaluationRequirement, IndependenceClass, ParentRunRef, PrivacyClass, RequestId, RequesterRef,
    RoutingPreference, SourceKind, SourceRef, SubAgentRunRef, TaskConstraint, WorkspaceSourceRef,
};

use super::client::{
    BridgeQueryError, ProjectedAdjudicationState, ProjectedDeliveryState, ProjectedExecutionState,
    SubmitReceipt, TachiBridgeClient, TachiTaskBridge,
};
use super::compose::{
    ComposeRejection, ForbiddenCategory, RequesterBridgePolicy, StructuralIntentContext,
    TaskIntentInputs, compose_intent,
};
use super::in_memory::{AmbiguousSubmitOnce, InMemoryTachiTaskBridge, UnavailableTachiTaskBridge};

// ─────────────────────────────────────────────────────────────────────────
// Fixtures
// ─────────────────────────────────────────────────────────────────────────

fn repository_implementation_policy() -> RequesterBridgePolicy {
    RequesterBridgePolicy {
        admitted_capabilities: BTreeSet::from([Capability::RepositoryImplementation]),
        workspace_source: Some(WorkspaceSourceRef {
            repo: BoundedText::new("kckylechen1/zeroclaw").expect("bounded"),
            git_ref: Some(BoundedText::new("master").expect("bounded")),
        }),
        routing_preference: Some(RoutingPreference::PreferTachiManaged),
        approval_requirement: zeroclaw_api::taskintent::ApprovalRequirement::NotRequired,
        privacy_class: PrivacyClass::Internal,
    }
}

fn acceptance_inputs(objective: &str) -> TaskIntentInputs {
    TaskIntentInputs {
        objective: BoundedText::new(objective).expect("bounded"),
        capability_request: CapabilityRequest {
            capability: Capability::RepositoryImplementation,
        },
        constraints: vec![TaskConstraint {
            description: BoundedText::new("no new ledgers; refs, not relay prose")
                .expect("bounded"),
        }],
        expected_artifacts: vec![
            ArtifactExpectation {
                artifact_class: ArtifactClass::Diff,
                description: BoundedText::new("repository diff implementing the objective")
                    .expect("bounded"),
                required: true,
            },
            ArtifactExpectation {
                artifact_class: ArtifactClass::VerificationLog,
                description: BoundedText::new("verification ran and passed").expect("bounded"),
                required: true,
            },
        ],
        evaluation_requirement: EvaluationRequirement {
            independence: IndependenceClass::FreshContextCrossVendor,
        },
    }
}

fn structural_context(bundle: &str) -> StructuralIntentContext {
    StructuralIntentContext {
        requester: RequesterRef::claim("zeroclaw-parent-v2b").expect("bounded"),
        parent_ref: Some(ParentRunRef::own("run-1").expect("bounded lineage id")),
        supervisor_ref: None,
        context_bundle_ref: BoundedText::new(bundle).expect("bounded"),
        source_refs: vec![SourceRef {
            kind: SourceKind::Issue,
            locator: BoundedText::new("kckylechen1/zeroclaw#234").expect("bounded"),
        }],
        expiry: None,
        retry_of: None,
    }
}

fn compose(objective: &str) -> Result<zeroclaw_api::taskintent::TaskIntentV1, ComposeRejection> {
    compose_intent(
        &acceptance_inputs(objective),
        &repository_implementation_policy(),
        &structural_context("bundle-7f3a"),
    )
}

fn request_id(n: u64) -> RequestId {
    RequestId::new(format!("req-{n}")).expect("bounded")
}

async fn admit(
    client: &TachiBridgeClient,
    objective: &str,
    n: u64,
) -> zeroclaw_api::taskintent::TaskRef {
    let intent = compose(objective).expect("clean intent");
    match client.submit(&intent, &request_id(n)).await {
        Ok(SubmitReceipt::Admitted { task_ref, .. }) => task_ref,
        other => panic!("expected Admitted, got {other:?}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Row 2/3: five-value surface, policy-filled authority, golden digest
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn compose_fills_authority_bearing_fields_from_policy_only() {
    // Row 2: the authority-bearing subset is set from the requester's own
    // admitted policy — never from task input or bundle content.
    let intent = compose("implement the watershed slice").expect("clean intent");
    let policy = repository_implementation_policy();
    assert_eq!(
        intent.capability_request.capability,
        Capability::RepositoryImplementation
    );
    assert_eq!(intent.workspace_source, policy.workspace_source);
    assert_eq!(intent.routing_preference, policy.routing_preference);
    assert_eq!(intent.approval_requirement, policy.approval_requirement);
    assert_eq!(intent.privacy_class, policy.privacy_class);
    // The five task-specific values land verbatim.
    assert_eq!(intent.objective.as_str(), "implement the watershed slice");
    assert_eq!(intent.constraints.len(), 1);
    assert_eq!(intent.expected_artifacts.len(), 2);
    assert_eq!(
        intent.evaluation_requirement.independence,
        IndependenceClass::FreshContextCrossVendor
    );
    // Structural fields come from context, not policy or input.
    assert_eq!(intent.requester.to_string(), "zeroclaw-parent-v2b");
    assert_eq!(
        intent.parent_ref.as_ref().map(ParentRunRef::as_wire),
        Some("parent:run-1")
    );
    assert!(intent.retry_of.is_none());
}

#[test]
fn compose_rejects_capability_outside_requester_authority() {
    // TB-5 intersection law, encode-side pre-flight: a capability the
    // requester's policy does not admit is refused before any transport.
    let mut inputs = acceptance_inputs("investigate instead");
    inputs.capability_request = CapabilityRequest {
        capability: Capability::ReadOnlyInvestigation,
    };
    let rejection = compose_intent(
        &inputs,
        &repository_implementation_policy(),
        &structural_context("bundle-7f3a"),
    )
    .unwrap_err();
    assert_eq!(rejection, ComposeRejection::CapabilityNotAdmitted);
}

#[test]
fn differing_bundle_content_yields_identical_admission_decision() {
    // Row 5 / TB-4 seam law: an intent whose ONLY difference is
    // ContextBundle content yields an IDENTICAL admission decision on
    // every authority-bearing field.
    let a = compose_intent(
        &acceptance_inputs("same objective"),
        &repository_implementation_policy(),
        &structural_context("bundle-aaa"),
    )
    .expect("admits");
    let b = compose_intent(
        &acceptance_inputs("same objective"),
        &repository_implementation_policy(),
        &structural_context("bundle-zzz"),
    )
    .expect("admits");
    // Identical admission decision (both admit), and the authority-bearing
    // fields are byte-identical.
    assert_eq!(a.capability_request, b.capability_request);
    assert_eq!(a.workspace_source, b.workspace_source);
    assert_eq!(a.routing_preference, b.routing_preference);
    assert_eq!(a.approval_requirement, b.approval_requirement);
    // ...while the digests differ (content difference is content).
    assert_ne!(a.canonical_digest(), b.canonical_digest());
}

// ─────────────────────────────────────────────────────────────────────────
// Row 4: per-category forbidden-content rejection (encode side)
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn forbidden_payloads_are_rejected_over_every_text_bearing_field() {
    // Row 4 / TB-4: per-category payloads fail compose with a typed
    // rejection naming the category and field, over every text-bearing
    // field (objective, constraint text, artifact description).
    let cases: &[(ForbiddenCategory, &str, &str)] = &[
        (
            ForbiddenCategory::Credential,
            "push using ghp_0123456789abcdef",
            "objective",
        ),
        (
            ForbiddenCategory::Command,
            "ssh host 'cargo test'",
            "objective",
        ),
        (
            ForbiddenCategory::WorktreePath,
            "run in /Users/dev/repo/worktrees/wt-1",
            "objective",
        ),
        (
            ForbiddenCategory::PrivateDyad,
            "see private dyad notes for details",
            "objective",
        ),
        (
            ForbiddenCategory::CallerMintedRef,
            "continue task:abc123 after the restart",
            "objective",
        ),
        (
            ForbiddenCategory::Command,
            "tmux attach -t main",
            "constraint",
        ),
        (
            ForbiddenCategory::Credential,
            "token sk-ant-aaaa",
            "artifact",
        ),
        (
            ForbiddenCategory::Command,
            "codex exec --full-auto",
            "constraint",
        ),
    ];
    for (category, payload, placement) in cases {
        let mut inputs = acceptance_inputs("clean objective");
        match *placement {
            "constraint" => {
                inputs.constraints = vec![TaskConstraint {
                    description: BoundedText::new(*payload).expect("bounded"),
                }];
            }
            "artifact" => {
                inputs.expected_artifacts = vec![ArtifactExpectation {
                    artifact_class: ArtifactClass::Report,
                    description: BoundedText::new(*payload).expect("bounded"),
                    required: true,
                }];
            }
            _ => {
                inputs.objective = BoundedText::new(*payload).expect("bounded");
            }
        }
        let rejection = compose_intent(
            &inputs,
            &repository_implementation_policy(),
            &structural_context("bundle-7f3a"),
        )
        .unwrap_err();
        let ComposeRejection::ForbiddenContent { category: hit, .. } = rejection else {
            panic!("expected ForbiddenContent for {payload:?}, got {rejection:?}");
        };
        assert_eq!(&hit, category, "payload {payload:?} placement {placement}");
    }
}

#[test]
fn watershed_dimensions_are_rejected_as_prose_anywhere_in_text() {
    // Row 4 discrimination list, PROSE form (codex round finding): the
    // banned dimensions must be rejected even when the text is not
    // shaped like a command or a path — a vendor name as backend prose,
    // a worktree as a relative path, a cwd/tmux/sandbox mention, or a
    // CLI flag as a standalone token. Each case mutates ONLY the
    // objective of an otherwise clean intent.
    let cases: &[(&str, ForbiddenCategory)] = &[
        // Vendor/model names as prose (TB-5).
        (
            "Use Anthropic Claude as the backend",
            ForbiddenCategory::ExecutionDetail,
        ),
        (
            "route this to glm-4 for speed",
            ForbiddenCategory::ExecutionDetail,
        ),
        (
            "prefer deepseek over the default",
            ForbiddenCategory::ExecutionDetail,
        ),
        (
            "Use Zhipu as the backend",
            ForbiddenCategory::ExecutionDetail,
        ),
        (
            "invoke with (--full-auto) enabled",
            ForbiddenCategory::ExecutionDetail,
        ),
        // Worktree/placement vocabulary (TB-4).
        (
            "do the work in a worktree ../feature-v2b",
            ForbiddenCategory::ExecutionDetail,
        ),
        (
            "clone into wt-2 then compare against ./main",
            ForbiddenCategory::ExecutionDetail,
        ),
        // cwd (TB-1 dimension).
        (
            "set cwd to the repository root first",
            ForbiddenCategory::ExecutionDetail,
        ),
        (
            "run from the working directory of the repo",
            ForbiddenCategory::ExecutionDetail,
        ),
        // tmux/SSH as prose (TB-4).
        (
            "keep a tmux session alive during the run",
            ForbiddenCategory::ExecutionDetail,
        ),
        (
            "tunnel over ssh for the build",
            ForbiddenCategory::ExecutionDetail,
        ),
        // Sandbox flags (TB-4).
        (
            "disable the sandbox for this one",
            ForbiddenCategory::ExecutionDetail,
        ),
        // CLI flags as standalone tokens (TB-4).
        (
            "pass --full-auto to the tool",
            ForbiddenCategory::ExecutionDetail,
        ),
        ("invoke with -rf once", ForbiddenCategory::ExecutionDetail),
    ];
    for (payload, expected) in cases {
        let mut inputs = acceptance_inputs("clean objective");
        inputs.objective = BoundedText::new(*payload).expect("bounded");
        let rejection = compose_intent(
            &inputs,
            &repository_implementation_policy(),
            &structural_context("bundle-7f3a"),
        )
        .unwrap_err();
        let ComposeRejection::ForbiddenContent { category: hit, .. } = rejection else {
            panic!("expected ForbiddenContent for {payload:?}, got {rejection:?}");
        };
        assert_eq!(&hit, expected, "payload {payload:?}");
    }
    // Discrimination control: ordinary implementation prose still passes.
    let clean = compose_intent(
        &acceptance_inputs("add a regression test for the digest contract"),
        &repository_implementation_policy(),
        &structural_context("bundle-7f3a"),
    )
    .expect("clean objective composes");
    assert_eq!(
        clean.objective.as_str(),
        "add a regression test for the digest contract"
    );
}

/// Transport wrapper that COUNTS port.submit calls — the discrimination
/// instrument for the client fail-closed law: a client-side rejection
/// must mean ZERO transport calls, independent of what the host would
/// have rejected on its own (zero host state alone was
/// non-discriminating because the host scans too).
struct SubmitSpy {
    inner: Arc<dyn TachiTaskBridge>,
    submit_calls: std::sync::atomic::AtomicUsize,
}

impl SubmitSpy {
    fn new(inner: Arc<dyn TachiTaskBridge>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            submit_calls: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn submit_calls(&self) -> usize {
        self.submit_calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl TachiTaskBridge for SubmitSpy {
    async fn submit(
        &self,
        intent: &zeroclaw_api::taskintent::TaskIntentV1,
        request_id: &RequestId,
    ) -> Result<SubmitReceipt, super::client::SubmitTransportError> {
        self.submit_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.submit(intent, request_id).await
    }

    async fn get(
        &self,
        task_ref: &zeroclaw_api::taskintent::TaskRef,
    ) -> Result<super::client::TaskSnapshotView, super::client::BridgeQueryError> {
        self.inner.get(task_ref).await
    }

    async fn watch(
        &self,
        task_ref: &zeroclaw_api::taskintent::TaskRef,
        after_seq: u64,
        limit: usize,
    ) -> Result<super::client::TaskEventPageView, super::client::BridgeQueryError> {
        self.inner.watch(task_ref, after_seq, limit).await
    }

    async fn collect(
        &self,
        task_ref: &zeroclaw_api::taskintent::TaskRef,
        result_revision: Option<u64>,
    ) -> Result<super::client::ResultProjectionView, super::client::BridgeQueryError> {
        self.inner.collect(task_ref, result_revision).await
    }
}

#[test]
fn client_submit_fails_closed_on_a_raw_constructed_forbidden_intent() {
    // The client must not be a bypass — a
    // programmatically constructed intent that never went through
    // compose still hits the encode-side admission scan before any
    // transport is touched (fail closed locally, never fail open).
    // The proof is a SPY on the port, not host state —
    // the host double would have rejected the payload on its own, so
    // only a zero submit-call count discriminates the CLIENT law.
    tokio_rt().block_on(async {
        let spy = SubmitSpy::new(Arc::new(InMemoryTachiTaskBridge::new()));
        let client = TachiBridgeClient::new(spy.clone());
        let mut intent = compose("clean").expect("clean base");
        intent.objective = BoundedText::new("ssh build-host 'cargo build'").expect("bounded");
        let receipt = client.submit(&intent, &request_id(1)).await.expect("typed");
        let SubmitReceipt::Rejected { reason } = receipt else {
            panic!("raw forbidden intent must be rejected client-side: {receipt:?}")
        };
        assert!(reason.contains("cli/shell command text"), "got: {reason}");
        assert_eq!(spy.submit_calls(), 0, "transport never touched");
        // The reconciling path enforces the same fail-closed law.
        let again = client
            .submit_reconciling(&intent, &request_id(1))
            .await
            .expect("typed");
        assert!(matches!(again, SubmitReceipt::Rejected { .. }));
        assert_eq!(spy.submit_calls(), 0, "transport still never touched");
    });
}

#[test]
fn client_rejects_forbidden_content_in_requester_authored_refs() {
    // The mirrored host law scans BoundedText
    // fields only; the CLIENT layer additionally fail-closes on the ref
    // values ZeroClaw itself authors — a lineage ref or requester claim
    // carrying credential/command/caller-minted content never reaches a
    // transport. Proven with the spy (zero port.submit calls).
    tokio_rt().block_on(async move {
        for (field, mutate) in [
            (
                "parent_ref",
                Box::new(|intent: &mut zeroclaw_api::taskintent::TaskIntentV1| {
                    intent.parent_ref =
                        Some(ParentRunRef::own("ghp_0123456789abcdef").expect("bounded"));
                }) as Box<dyn Fn(&mut zeroclaw_api::taskintent::TaskIntentV1)>,
            ),
            (
                "parent_ref",
                Box::new(|intent: &mut zeroclaw_api::taskintent::TaskIntentV1| {
                    // A caller-minted task id smuggled into a lineage body.
                    intent.parent_ref = Some(ParentRunRef::own("task:forged").expect("bounded"));
                }),
            ),
            (
                "supervisor_ref",
                Box::new(|intent: &mut zeroclaw_api::taskintent::TaskIntentV1| {
                    intent.supervisor_ref =
                        Some(SubAgentRunRef::own("ssh build-host").expect("bounded"));
                }),
            ),
            (
                "requester",
                Box::new(|intent: &mut zeroclaw_api::taskintent::TaskIntentV1| {
                    intent.requester = RequesterRef::claim("codex --full-auto").expect("bounded");
                }),
            ),
        ] {
            let spy = SubmitSpy::new(Arc::new(InMemoryTachiTaskBridge::new()));
            let client = TachiBridgeClient::new(spy.clone());
            let mut intent = compose("clean").expect("clean base");
            mutate(&mut intent);
            let receipt = client.submit(&intent, &request_id(1)).await.expect("typed");
            let SubmitReceipt::Rejected { reason } = receipt else {
                panic!("forbidden {field} must be rejected client-side: {receipt:?}");
            };
            assert!(
                reason.contains("intent rejected:"),
                "typed rejection naming the category, got: {reason}"
            );
            assert_eq!(spy.submit_calls(), 0, "{field}: transport never touched");
        }
        // The reconciling path enforces the same ref law (mutation-
        // discriminated: deleting the scan from submit_reconciling alone
        // must turn this red).
        let spy = SubmitSpy::new(Arc::new(InMemoryTachiTaskBridge::new()));
        let client = TachiBridgeClient::new(spy.clone());
        let mut intent = compose("clean").expect("clean");
        intent.supervisor_ref = Some(SubAgentRunRef::own("tmux attach -t x").expect("bounded"));
        let receipt = client
            .submit_reconciling(&intent, &request_id(1))
            .await
            .expect("typed");
        assert!(
            matches!(receipt, SubmitReceipt::Rejected { .. }),
            "{receipt:?}"
        );
        assert_eq!(
            spy.submit_calls(),
            0,
            "reconciling path: transport untouched"
        );
        // A forbidden BODY inside a decoded retry_of ref (the task:
        // namespace itself is legitimate for this field; the body is
        // still content-scanned).
        let spy = SubmitSpy::new(Arc::new(InMemoryTachiTaskBridge::new()));
        let client = TachiBridgeClient::new(spy.clone());
        let mut intent = compose("clean").expect("clean");
        intent.retry_of = Some(
            serde_json::from_value(serde_json::Value::String(
                "task:ghp_0123456789abcdef".to_string(),
            ))
            .expect("wire-shaped"),
        );
        let receipt = client.submit(&intent, &request_id(1)).await.expect("typed");
        assert!(
            matches!(receipt, SubmitReceipt::Rejected { .. }),
            "{receipt:?}"
        );
        assert_eq!(
            spy.submit_calls(),
            0,
            "forbidden retry_of body: transport untouched"
        );
        // A clean retry lineage (a decoded prior TaskRef) still submits.
        let host = Arc::new(InMemoryTachiTaskBridge::new());
        let client = TachiBridgeClient::new(host.clone());
        let first = compose("first submission").expect("clean");
        let SubmitReceipt::Admitted { task_ref, .. } =
            client.submit(&first, &request_id(1)).await.expect("ok")
        else {
            unreachable!()
        };
        let mut retry = compose("deliberate retry").expect("clean");
        retry.retry_of = Some(task_ref.clone());
        let receipt = client.submit(&retry, &request_id(2)).await.expect("typed");
        assert!(
            matches!(receipt, SubmitReceipt::Admitted { .. }),
            "clean retry lineage must pass the ref scan: {receipt:?}"
        );
    });
}

// ─────────────────────────────────────────────────────────────────────────
// Row 7: TB-7 idempotency client behavior (in-process law)
// ─────────────────────────────────────────────────────────────────────────

fn tokio_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

#[test]
fn same_tuple_and_digest_replays_to_the_same_task_ref() {
    tokio_rt().block_on(async {
        let host = Arc::new(InMemoryTachiTaskBridge::new());
        let client = TachiBridgeClient::new(host.clone());
        let intent = compose("one real repository task").expect("clean");
        let first = client.submit(&intent, &request_id(1)).await.expect("ok");
        let SubmitReceipt::Admitted {
            task_ref,
            replayed: false,
        } = first
        else {
            panic!("first submit admits: {first:?}");
        };
        // Owner vertical test 1: same (requester, request_id) + same
        // digest CANNOT double-start.
        let second = client.submit(&intent, &request_id(1)).await.expect("ok");
        let SubmitReceipt::Admitted {
            task_ref: replayed_ref,
            replayed: true,
        } = second
        else {
            panic!("duplicate replays: {second:?}")
        };
        assert_eq!(task_ref, replayed_ref);
        assert_eq!(host.task_count(), 1, "no second worker may start");
        assert_eq!(host.binding_count(), 1);
    });
}

#[test]
fn same_tuple_with_different_digest_is_a_typed_conflict_with_zero_spawns() {
    tokio_rt().block_on(async {
        let host = Arc::new(InMemoryTachiTaskBridge::new());
        let client = TachiBridgeClient::new(host.clone());
        let a = compose("objective A").expect("clean");
        let b = compose("objective B").expect("clean");
        assert!(matches!(
            client.submit(&a, &request_id(1)).await,
            Ok(SubmitReceipt::Admitted { .. })
        ));
        let tasks_before = host.task_count();
        // Owner vertical test 2: same request_id, different digest.
        let outcome = client.submit(&b, &request_id(1)).await.expect("ok");
        let SubmitReceipt::RequestIdConflict {
            bound_digest,
            submitted_digest,
        } = outcome
        else {
            panic!("expected RequestIdConflict, got {outcome:?}");
        };
        assert_ne!(bound_digest, submitted_digest);
        assert_eq!(host.task_count(), tasks_before, "zero new execution");
    });
}

#[test]
fn ambiguous_submit_replays_the_same_request_id_and_reconciles_one_task() {
    // TB-7 rule 4 (owner vertical test 1b): after an ambiguous submit —
    // the response was lost AFTER the host committed — ZeroClaw replays
    // the SAME request id; it never invents a new one, and the replay
    // reconciles to exactly one task.
    tokio_rt().block_on(async {
        let host = Arc::new(InMemoryTachiTaskBridge::new());
        let flaky = Arc::new(AmbiguousSubmitOnce::new(host.clone()));
        let client = TachiBridgeClient::new(flaky);
        let intent = compose("ambiguous then reconciled").expect("clean");
        let outcome = client
            .submit_reconciling(&intent, &request_id(7))
            .await
            .expect("reconciles within the bounded attempts");
        let SubmitReceipt::Admitted {
            task_ref,
            replayed: true,
        } = outcome
        else {
            panic!("replay reconciles: {outcome:?}");
        };
        // Exactly one tuple, exactly one task — the replay reused the id.
        assert_eq!(host.binding_count(), 1);
        assert_eq!(host.task_count(), 1);
        // And the reconciled task is observable.
        let snapshot = client.get(&task_ref).await.expect("exists");
        assert_eq!(snapshot.task_ref, task_ref);
    });
}

// ─────────────────────────────────────────────────────────────────────────
// Row 8/9: snapshot mapping tables (TB-8/TB-16) and watch backfill (TB-9)
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn snapshot_lifecycle_state_derives_from_the_three_mapping_tables() {
    tokio_rt().block_on(async {
        let host = Arc::new(InMemoryTachiTaskBridge::new());
        let client = TachiBridgeClient::new(host.clone());
        let task_ref = admit(&client, "mapping-table task", 1).await;
        // Before any execution fact: queued / unreviewed / not_ready.
        let snapshot = client.get(&task_ref).await.expect("snapshot");
        assert_eq!(snapshot.execution.label(), "queued");
        assert_eq!(snapshot.adjudication.label(), "unreviewed");
        assert_eq!(snapshot.delivery.label(), "not_ready");
        assert_eq!(snapshot.execution.to_string(), "exec:queued");
        // Cross-dimension law (TB-16): an adjudication transition never
        // changes the projected execution state, and a delivery-ready
        // projection never changes adjudication.
        host.ingest_execution(&task_ref, "running");
        let running = client.get(&task_ref).await.expect("snapshot");
        assert_eq!(running.execution.label(), "running");
        assert_eq!(running.adjudication.label(), "unreviewed");
        assert_eq!(running.delivery.label(), "not_ready");
        host.ingest_adjudication(&task_ref, "accepted");
        let adjudicated = client.get(&task_ref).await.expect("snapshot");
        assert_eq!(adjudicated.adjudication.label(), "accepted");
        assert_eq!(adjudicated.execution.label(), "running");
        // Unknown dimension labels are not projectable (no local enum can
        // mint a state outside the tables).
        assert!(ProjectedExecutionState::project("done").is_none());
        assert!(ProjectedAdjudicationState::project("queued").is_none());
        assert!(ProjectedDeliveryState::project("running").is_none());
    });
}

#[test]
fn watch_backfill_replays_exactly_the_missed_events() {
    // Row 9 / TB-9 + owner vertical test 3: reconnect from the last-seen
    // cursor replays exactly the missed events; duplicates are
    // suppressed; watching never creates tasks.
    tokio_rt().block_on(async {
        let host = Arc::new(InMemoryTachiTaskBridge::new());
        let client = TachiBridgeClient::new(host.clone());
        let task_ref = admit(&client, "watch backfill task", 1).await;
        host.ingest_execution(&task_ref, "running");
        host.ingest_execution(&task_ref, "submitted");
        // First watch: full backfill from 0.
        let page = client.watch_new_events(&task_ref, 10).await.expect("page");
        assert_eq!(page.events.len(), 3, "submitted + 2 execution facts");
        let cursor = client.cursor(&task_ref);
        assert_eq!(cursor, 3);
        // More events land while "disconnected".
        host.ingest_execution(&task_ref, "completed");
        // Reconnect from the last-seen cursor: exactly the missed events.
        let replay = client.watch_new_events(&task_ref, 10).await.expect("page");
        assert_eq!(replay.events.len(), 1);
        assert_eq!(replay.events[0].seq, 4);
        // Deterministic duplicate suppression at the WATCH layer (TB-9):
        // replaying a watch from the SAME cursor never re-delivers an
        // event the cursor already covers, and event ids are unique per
        // canonical fact (occurrence-unique), so a page can never carry
        // the same (seq, event_id) twice.
        let same_cursor = client.watch_new_events(&task_ref, 10).await.expect("page");
        assert!(
            same_cursor.events.is_empty(),
            "nothing new: the cursor already covers seq 1..=4"
        );
        let mut seen: Vec<(u64, String)> = Vec::new();
        let full = host.watch(&task_ref, 0, 100).await.expect("page");
        assert!(!full.has_more);
        seen.extend(full.events.iter().map(|e| (e.seq, e.event_id.clone())));
        seen.sort();
        seen.dedup();
        assert_eq!(
            seen.len(),
            4,
            "no duplicate (seq, event_id) pairs: {seen:?}"
        );
        // 8-field binding present on every event.
        for event in page.events {
            assert!(!event.event_id.is_empty());
            assert!(!event.source.is_empty());
            assert!(!event.source_revision.is_empty());
            assert!(!event.occurred_at.is_empty());
            assert!(!event.recorded_at.is_empty());
            assert!(
                event.payload_digest.len() == 64
                    && event.payload_digest.chars().all(|c| c.is_ascii_hexdigit()),
                "payload digest must be SHA-256 lower hex: {}",
                event.payload_digest
            );
            assert!(!event.visibility.is_empty());
            assert!(!event.kind.is_empty());
        }
        // Watching never creates tasks.
        assert_eq!(host.task_count(), 1);
        // Paging: limit respects has_more (direct port call).
        let paged = host.watch(&task_ref, 0, 2).await.expect("page");
        assert!(paged.has_more);
        assert_eq!(paged.events.len(), 2);
    });
}

#[test]
fn repeated_transitions_are_distinct_facts_and_fold_in_order() {
    // Host-double discipline: each ingest is a DISTINCT
    // canonical fact — `running → failed → running` must fold to
    // `running` (not collapse to `failed`), and a second
    // outcome/adjudication cycle must leave the second outcome with the
    // second adjudication's state (not a stale one).
    tokio_rt().block_on(async {
        let host = Arc::new(InMemoryTachiTaskBridge::new());
        let client = TachiBridgeClient::new(host.clone());
        let task_ref = admit(&client, "repeated transitions", 1).await;
        host.ingest_execution(&task_ref, "running");
        host.ingest_execution(&task_ref, "failed");
        host.ingest_execution(&task_ref, "running");
        let snapshot = client.get(&task_ref).await.expect("snapshot");
        assert_eq!(snapshot.execution.label(), "running");
        // Repeated labels are DISTINCT facts: their event ids differ
        // (asserted on the id strings themselves - seq uniqueness alone
        // cannot prove this).
        let page = host.watch(&task_ref, 0, 100).await.expect("page");
        let running_ids: Vec<&str> = page
            .events
            .iter()
            .filter(|event| event.event_id.starts_with("exec-running-"))
            .map(|event| event.event_id.as_str())
            .collect();
        assert_eq!(running_ids.len(), 2, "two running facts: {running_ids:?}");
        assert_ne!(running_ids[0], running_ids[1]);
        let mut all_ids: Vec<&str> = page.events.iter().map(|e| e.event_id.as_str()).collect();
        all_ids.sort_unstable();
        all_ids.dedup();
        assert_eq!(all_ids.len(), page.events.len(), "event ids are unique");
        // Outcome → accepted → new outcome: the latest revision carries
        // the latest outcome with an adjudication that applies to IT.
        let attempt: zeroclaw_api::taskintent::AttemptRef =
            serde_json::from_value(serde_json::Value::String("attempt:inmem-01".to_string()))
                .expect("wire-shaped attempt ref");
        host.observe_outcome(
            &task_ref,
            attempt.clone(),
            "success",
            Some("artifact:rev-1".to_string()),
            vec!["artifact:rev-1".to_string()],
            true,
            true,
            "vendor=x;basis=observed",
        );
        host.ingest_adjudication(&task_ref, "accepted");
        host.observe_outcome(
            &task_ref,
            attempt,
            "success",
            Some("artifact:rev-2".to_string()),
            vec!["artifact:rev-2".to_string()],
            true,
            true,
            "vendor=x;basis=observed",
        );
        // No adjudication after the second outcome yet: it is unreviewed.
        let latest = client.collect_latest(&task_ref).await.expect("latest");
        assert_eq!(latest.result_revision, 3);
        assert_eq!(latest.adjudication.label(), "unreviewed");
        assert_eq!(
            latest.canonical_artifact_ref.as_deref(),
            Some("artifact:rev-2")
        );
        // Pinning the first revision still yields the first outcome with
        // its own adjudication state at the time.
        let first = client.collect_pinned(&task_ref, 1).await.expect("pinned");
        assert_eq!(
            first.canonical_artifact_ref.as_deref(),
            Some("artifact:rev-1")
        );
    });
}

#[test]
#[should_panic(expected = "admitted tasks only")]
fn ingesting_execution_for_an_unadmitted_task_ref_is_refused() {
    let host = InMemoryTachiTaskBridge::new();
    let fabricated: zeroclaw_api::taskintent::TaskRef =
        serde_json::from_value(serde_json::Value::String("task:inmem-00000001".to_string()))
            .expect("wire-shaped fabricated ref");
    host.ingest_execution(&fabricated, "running");
}

#[test]
#[should_panic(expected = "admitted tasks only")]
fn ingesting_adjudication_for_an_unadmitted_task_ref_is_refused() {
    let host = InMemoryTachiTaskBridge::new();
    let fabricated: zeroclaw_api::taskintent::TaskRef =
        serde_json::from_value(serde_json::Value::String("task:inmem-00000001".to_string()))
            .expect("wire-shaped fabricated ref");
    host.ingest_adjudication(&fabricated, "accepted");
}

#[test]
#[should_panic(expected = "admitted tasks only")]
fn observing_an_outcome_for_an_unadmitted_task_ref_is_refused() {
    // Host-double discipline: the ingest drivers accept
    // only refs the double itself admitted via submit. A fabricated
    // future ref (`task:inmem-00000001` is predictable) cannot pre-seed
    // a result log before admission.
    let host = InMemoryTachiTaskBridge::new();
    let fabricated: zeroclaw_api::taskintent::TaskRef =
        serde_json::from_value(serde_json::Value::String("task:inmem-00000001".to_string()))
            .expect("wire-shaped fabricated ref");
    let attempt: zeroclaw_api::taskintent::AttemptRef =
        serde_json::from_value(serde_json::Value::String("attempt:inmem-01".to_string()))
            .expect("wire-shaped attempt ref");
    host.observe_outcome(
        &fabricated,
        attempt,
        "success",
        None,
        Vec::new(),
        false,
        false,
        "vendor=attacker;basis=fabricated",
    );
}

#[test]
fn payload_digest_is_the_canonical_json_sha256_of_the_payload() {
    // The digest contract is content, not identity: recompute the
    // expected SHA-256 over the canonical JSON of a known payload and
    // assert the event carries exactly that (lowercase hex).
    tokio_rt().block_on(async {
        let host = Arc::new(InMemoryTachiTaskBridge::new());
        let client = TachiBridgeClient::new(host.clone());
        let task_ref = admit(&client, "digest contract task", 1).await;
        host.ingest_execution(&task_ref, "running");
        let page = host.watch(&task_ref, 0, 100).await.expect("page");
        let event = page
            .events
            .iter()
            .find(|e| e.kind == "execution")
            .expect("execution event");
        let expected_payload = serde_json::json!({"kind": "execution", "label": "running"});
        let canonical = zeroclaw_api::taskintent::canonical_json(&expected_payload).to_string();
        use sha2::Digest as _;
        let expected = sha2::Sha256::digest(canonical.as_bytes());
        let hex: String = expected.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(event.payload_digest, hex);
        // And content sensitivity: a different payload yields a different digest.
        host.ingest_execution(&task_ref, "failed");
        let page2 = host.watch(&task_ref, 0, 100).await.expect("page");
        let failed = page2
            .events
            .iter()
            .find(|e| e.kind == "execution" && e.payload_digest != event.payload_digest)
            .expect("failed fact carries a different payload digest");
        assert_ne!(failed.payload_digest, event.payload_digest);
    });
}

#[test]
fn watch_cursor_never_regresses_on_a_stale_out_of_order_response() {
    // A scripted transport returns an OLDER page after a newer one (the
    // slow-response race): the cursor must keep the higher sequence.
    struct ScriptedWatch {
        pages: std::sync::Mutex<
            std::collections::VecDeque<
                Result<super::client::TaskEventPageView, super::client::BridgeQueryError>,
            >,
        >,
    }

    #[async_trait::async_trait]
    impl TachiTaskBridge for ScriptedWatch {
        async fn submit(
            &self,
            _intent: &zeroclaw_api::taskintent::TaskIntentV1,
            _request_id: &RequestId,
        ) -> Result<SubmitReceipt, super::client::SubmitTransportError> {
            Err(super::client::SubmitTransportError)
        }

        async fn get(
            &self,
            _task_ref: &zeroclaw_api::taskintent::TaskRef,
        ) -> Result<super::client::TaskSnapshotView, super::client::BridgeQueryError> {
            Err(super::client::BridgeQueryError::Unavailable)
        }

        async fn watch(
            &self,
            task_ref: &zeroclaw_api::taskintent::TaskRef,
            _after_seq: u64,
            _limit: usize,
        ) -> Result<super::client::TaskEventPageView, super::client::BridgeQueryError> {
            self.pages
                .lock()
                .expect("pages lock")
                .pop_front()
                .unwrap_or(Err(super::client::BridgeQueryError::Unavailable))
                .map(|mut page| {
                    page.task_ref = task_ref.clone();
                    page
                })
        }

        async fn collect(
            &self,
            _task_ref: &zeroclaw_api::taskintent::TaskRef,
            _result_revision: Option<u64>,
        ) -> Result<super::client::ResultProjectionView, super::client::BridgeQueryError> {
            Err(super::client::BridgeQueryError::Unavailable)
        }
    }

    fn page_with(
        seqs: &[u64],
    ) -> Result<super::client::TaskEventPageView, super::client::BridgeQueryError> {
        Ok(super::client::TaskEventPageView {
            task_ref: serde_json::from_value(serde_json::Value::String("task:x".to_string()))
                .expect("wire-shaped"),
            events: seqs
                .iter()
                .map(|seq| super::client::TaskEventView {
                    seq: *seq,
                    event_id: format!("evt-{seq}"),
                    source: "bridge".to_string(),
                    source_revision: seq.to_string(),
                    occurred_at: "t".to_string(),
                    recorded_at: "t".to_string(),
                    payload_digest: "0".repeat(64),
                    visibility: "internal".to_string(),
                    kind: "execution".to_string(),
                })
                .collect(),
            has_more: false,
        })
    }

    tokio_rt().block_on(async {
        let scripted = Arc::new(ScriptedWatch {
            pages: std::sync::Mutex::new(
                [page_with(&[1, 2, 3]), page_with(&[1])]
                    .into_iter()
                    .collect(),
            ),
        });
        let client = TachiBridgeClient::new(scripted);
        let task_ref: zeroclaw_api::taskintent::TaskRef =
            serde_json::from_value(serde_json::Value::String("task:x".to_string()))
                .expect("wire-shaped");
        // Newer page first: cursor advances to 3.
        client.watch_new_events(&task_ref, 10).await.expect("page");
        assert_eq!(client.cursor(&task_ref), 3);
        // Stale slower response with an older page: cursor stays at 3.
        client.watch_new_events(&task_ref, 10).await.expect("page");
        assert_eq!(client.cursor(&task_ref), 3, "cursor must not regress");
    });
}

// ─────────────────────────────────────────────────────────────────────────
// Row 10: collect is artifact-first (TB-13)
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn worker_success_without_required_artifact_is_not_contract_success() {
    // Owner vertical test 7: a worker `success` without the required
    // artifact/evidence does not satisfy the contract, regardless of
    // worker prose.
    tokio_rt().block_on(async {
        let host = Arc::new(InMemoryTachiTaskBridge::new());
        let client = TachiBridgeClient::new(host.clone());
        let task_ref = admit(&client, "artifact-first task", 1).await;
        let attempt: zeroclaw_api::taskintent::AttemptRef =
            serde_json::from_value(serde_json::Value::String("attempt:inmem-01".to_string()))
                .expect("wire-shaped attempt ref");
        // Worker reports success with NO diff and NO verification.
        host.observe_outcome(
            &task_ref,
            attempt,
            "success",
            None,
            Vec::new(),
            false,
            false,
            "vendor=unknown;basis=reported",
        );
        let result = client.collect_latest(&task_ref).await.expect("projection");
        assert_eq!(result.terminal_classification, "success");
        assert_eq!(
            result.contract_violations.len(),
            2,
            "diff + verification_log both missing"
        );
        assert!(
            result
                .contract_violations
                .iter()
                .any(|v| v.artifact_class == "diff")
        );
        assert!(
            result
                .contract_violations
                .iter()
                .any(|v| v.artifact_class == "verification_log")
        );
        // Same worker prose WITH the required evidence: no violations.
        let task_ref2 = admit(&client, "artifact-first task with evidence", 2).await;
        let attempt2: zeroclaw_api::taskintent::AttemptRef =
            serde_json::from_value(serde_json::Value::String("attempt:inmem-02".to_string()))
                .expect("wire-shaped attempt ref");
        host.observe_outcome(
            &task_ref2,
            attempt2,
            "success",
            Some("artifact:diff-1".to_string()),
            vec!["artifact:diff-1".to_string(), "evidence:log-1".to_string()],
            true,
            true,
            "vendor=x;basis=observed",
        );
        let ok = client.collect_latest(&task_ref2).await.expect("projection");
        assert!(ok.contract_violations.is_empty());
        assert_eq!(ok.verification.evidence_ref_count, 2);
    });
}

#[test]
fn result_revisions_are_newer_wins_and_pinned_exact_or_typed_not_found() {
    tokio_rt().block_on(async {
        let host = Arc::new(InMemoryTachiTaskBridge::new());
        let client = TachiBridgeClient::new(host.clone());
        let task_ref = admit(&client, "revisioned task", 1).await;
        let attempt: zeroclaw_api::taskintent::AttemptRef =
            serde_json::from_value(serde_json::Value::String("attempt:inmem-01".to_string()))
                .expect("wire-shaped attempt ref");
        host.observe_outcome(
            &task_ref,
            attempt.clone(),
            "success",
            Some("artifact:rev-1".to_string()),
            vec!["artifact:rev-1".to_string()],
            true,
            true,
            "vendor=x;basis=observed",
        );
        host.observe_outcome(
            &task_ref,
            attempt,
            "success",
            Some("artifact:rev-2".to_string()),
            vec!["artifact:rev-2".to_string()],
            true,
            true,
            "vendor=x;basis=observed",
        );
        // Default: newer wins.
        let latest = client.collect_latest(&task_ref).await.expect("latest");
        assert_eq!(latest.result_revision, 2);
        assert_eq!(
            latest.canonical_artifact_ref.as_deref(),
            Some("artifact:rev-2")
        );
        // Pinned: exact revision.
        let pinned = client.collect_pinned(&task_ref, 1).await.expect("pinned");
        assert_eq!(pinned.result_revision, 1);
        assert_eq!(
            pinned.canonical_artifact_ref.as_deref(),
            Some("artifact:rev-1")
        );
        // Bogus pin: typed not_found.
        assert_eq!(
            client.collect_pinned(&task_ref, 99).await.unwrap_err(),
            BridgeQueryError::ResultRevisionNotFound
        );
        // No result yet: typed NotReady.
        let task_ref2 = admit(&client, "no result task", 2).await;
        assert_eq!(
            client.collect_latest(&task_ref2).await.unwrap_err(),
            BridgeQueryError::NotReady
        );
    });
}

// ─────────────────────────────────────────────────────────────────────────
// Row 12: TB-20 outage fails closed; positive control stays structural
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn tachi_outage_fails_closed_with_zero_local_execution() {
    // Row 12 / TB-20: with the transport down, the repository-work
    // request returns typed `Unavailable`; the client has no local
    // execution path (proven structurally below and by the module source
    // scan in `module_source_scans_hold`).
    tokio_rt().block_on(async {
        let client = TachiBridgeClient::new(Arc::new(UnavailableTachiTaskBridge));
        let intent = compose("repository work during outage").expect("clean");
        assert_eq!(
            client.submit(&intent, &request_id(1)).await,
            Ok(SubmitReceipt::Unavailable)
        );
        assert_eq!(
            client.submit_reconciling(&intent, &request_id(1)).await,
            Ok(SubmitReceipt::Unavailable)
        );
        // get/watch/collect also fail typed, never silently succeed.
        let bogus = serde_json::from_value(serde_json::Value::String("task:none".to_string()))
            .expect("wire-shaped ref");
        assert_eq!(
            client.get(&bogus).await.unwrap_err(),
            BridgeQueryError::Unavailable
        );
        assert_eq!(
            client.watch_new_events(&bogus, 10).await.unwrap_err(),
            BridgeQueryError::Unavailable
        );
        assert_eq!(
            client.collect_latest(&bogus).await.unwrap_err(),
            BridgeQueryError::Unavailable
        );
    });
}

#[test]
fn module_source_scans_hold() {
    // Row 12 structural halves + TB-23 item 4:
    // (a) the bridge client module contains no process/command execution
    //     capability (TB-20: there is nothing to fall back TO);
    // (b) it imports nothing from zeroclaw-eval;
    // (c) the local-chat/agent path does not reference the bridge (the
    //     positive control: ordinary local chat cannot depend on Tachi
    //     availability because nothing in the agent path names it).
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let module_files = [
        "tachi_bridge/mod.rs",
        "tachi_bridge/compose.rs",
        "tachi_bridge/client.rs",
        "tachi_bridge/in_memory.rs",
    ];
    for file in module_files {
        let source = std::fs::read_to_string(format!("{manifest_dir}/src/{file}"))
            .unwrap_or_else(|error| panic!("read {file}: {error}"));
        for banned in [
            "std::process",
            "tokio::process",
            "process::Command",
            "zeroclaw_eval",
        ] {
            assert!(
                !source.contains(banned),
                "{file} must not contain {banned} (TB-20/TB-23)"
            );
        }
    }
    // (c) scan the agent module tree for references to the bridge.
    let agent_dir = format!("{manifest_dir}/src/agent");
    let mut scanned = 0;
    let mut stack: Vec<std::path::PathBuf> = vec![std::path::PathBuf::from(agent_dir)];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .unwrap_or_else(|error| panic!("read dir {}: {error}", dir.display()));
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                scanned += 1;
                let source = std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
                assert!(
                    !source.contains("tachi_bridge"),
                    "{} must not reference the tachi bridge (TB-20 positive control)",
                    path.display()
                );
            }
        }
    }
    assert!(
        scanned > 10,
        "agent tree scan must actually cover sources ({scanned})"
    );
}

#[test]
fn client_surface_is_exactly_submit_get_watch_collect() {
    // Owner scope limit: no intervene/request_stop client surface. The
    // port trait's method set is asserted textually so a future op
    // addition shows up as a deliberate diff against this pin.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let source = std::fs::read_to_string(format!("{manifest_dir}/src/tachi_bridge/client.rs"))
        .expect("client source");
    let trait_body = source
        .split("pub trait TachiTaskBridge")
        .nth(1)
        .expect("trait present")
        .split('}')
        .next()
        .expect("trait body");
    for op in [
        "async fn submit",
        "async fn get",
        "async fn watch",
        "async fn collect",
    ] {
        assert!(trait_body.contains(op), "port must expose {op}");
    }
    for banned in ["intervene", "request_stop", "fn spawn", "fn cancel"] {
        assert!(
            !trait_body.contains(banned),
            "port must not expose {banned} (owner scope: V3 leaf)"
        );
    }
}

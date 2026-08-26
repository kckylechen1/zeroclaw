//! SA-26/TB-22 discrimination, supervisor half (vertical V3): the
//! supervisor session path creates NO durable Task/Attempt rows in
//! `control_plane.db` or any other ZeroClaw DB — supervision state,
//! judgments, and reports are run-scoped, and durable work truth stays
//! Tachi-side through the bridge port.
//!
//! This test lives in its own integration binary on purpose (mirroring
//! `subagent_v1_no_durable_writes.rs`): it installs the process-global
//! control plane, which must not leak into any other test's process.

use std::sync::Arc;

use zeroclaw_api::subagent_v1::LineageRef;
use zeroclaw_api::taskintent::RequesterRef;
use zeroclaw_runtime::subagent_v1::SubAgentProfileRegistry;
use zeroclaw_runtime::supervisor_v1::SupervisorSessionV1;
use zeroclaw_runtime::tachi_bridge::TachiBridgeClient;

/// A port implementation that is always down (TB-20): the supervisor's
/// no-durable-writes property must hold even when every bridge op fails
/// typed — there is no fallback surface that could write anything.
struct DownBridge;

#[async_trait::async_trait]
impl zeroclaw_runtime::tachi_bridge::TachiTaskBridge for DownBridge {
    async fn submit(
        &self,
        _intent: &zeroclaw_api::taskintent::TaskIntentV1,
        _request_id: &zeroclaw_api::taskintent::RequestId,
    ) -> Result<
        zeroclaw_runtime::tachi_bridge::SubmitReceipt,
        zeroclaw_runtime::tachi_bridge::SubmitTransportError,
    > {
        Ok(zeroclaw_runtime::tachi_bridge::SubmitReceipt::Unavailable)
    }

    async fn get(
        &self,
        _task_ref: &zeroclaw_api::taskintent::TaskRef,
    ) -> Result<
        zeroclaw_runtime::tachi_bridge::TaskSnapshotView,
        zeroclaw_runtime::tachi_bridge::BridgeQueryError,
    > {
        Err(zeroclaw_runtime::tachi_bridge::BridgeQueryError::Unavailable)
    }

    async fn watch(
        &self,
        _task_ref: &zeroclaw_api::taskintent::TaskRef,
        _after_seq: u64,
        _limit: usize,
    ) -> Result<
        zeroclaw_runtime::tachi_bridge::TaskEventPageView,
        zeroclaw_runtime::tachi_bridge::BridgeQueryError,
    > {
        Err(zeroclaw_runtime::tachi_bridge::BridgeQueryError::Unavailable)
    }

    async fn collect(
        &self,
        _task_ref: &zeroclaw_api::taskintent::TaskRef,
        _result_revision: Option<u64>,
    ) -> Result<
        zeroclaw_runtime::tachi_bridge::ResultProjectionView,
        zeroclaw_runtime::tachi_bridge::BridgeQueryError,
    > {
        Err(zeroclaw_runtime::tachi_bridge::BridgeQueryError::Unavailable)
    }

    async fn intervene(
        &self,
        _task_ref: &zeroclaw_api::taskintent::TaskRef,
        _intervention: &zeroclaw_api::taskintent::InterventionV1,
        _requester: &RequesterRef,
        _request_id: &zeroclaw_api::taskintent::RequestId,
        _expected_task_revision: Option<u64>,
    ) -> Result<
        zeroclaw_api::taskintent::InterventionReceipt,
        zeroclaw_api::taskintent::InterventionError,
    > {
        Err(zeroclaw_api::taskintent::InterventionError::Unavailable)
    }

    async fn request_stop(
        &self,
        _task_ref: &zeroclaw_api::taskintent::TaskRef,
        _mode: zeroclaw_api::taskintent::StopMode,
        _requester: &RequesterRef,
        _request_id: &zeroclaw_api::taskintent::RequestId,
        _expected_task_revision: Option<u64>,
    ) -> Result<zeroclaw_api::taskintent::StopReceipt, zeroclaw_api::taskintent::InterventionError>
    {
        Err(zeroclaw_api::taskintent::InterventionError::Unavailable)
    }
}

#[tokio::test]
async fn supervisor_session_writes_no_control_plane_rows() {
    // Install a REAL in-memory control-plane store so the supervisor
    // path runs with a live control plane it could write to — and prove
    // it never does.
    let store = Arc::new(
        zeroclaw_runtime::control_plane::task_store_sqlite::SqliteTaskStore::new_in_memory()
            .expect("in-memory store"),
    );
    let handle = zeroclaw_runtime::control_plane::boot::ControlPlaneHandle {
        store: Arc::clone(&store)
            as Arc<dyn zeroclaw_runtime::control_plane::task_registry::TaskRegistry>,
        boot_id: "test-boot-supervisor".into(),
        sqlite_store: Arc::clone(&store),
        commands: None,
    };
    assert!(
        zeroclaw_runtime::control_plane::init_control_plane(handle),
        "this binary owns the global install; a prior install means test pollution"
    );

    let mut registry = SubAgentProfileRegistry::default();
    let vref = registry
        .admit(SubAgentProfileRegistry::default_supervisor_profile())
        .expect("default supervisor profile admits");
    let requester: RequesterRef =
        serde_json::from_value(serde_json::json!("requester:supervisor-durability"))
            .expect("requester ref");
    let client = TachiBridgeClient::new(Arc::new(DownBridge));
    let mut supervisor = SupervisorSessionV1::from_admitted_profile(
        &registry,
        &vref,
        &LineageRef::new_root(zeroclaw_api::subagent_v1::ParentRunRef::from_opaque(
            "root-durability",
        )),
        client,
        requester,
        None,
    )
    .expect("supervisor session admits");

    // Drive every supervisor surface against the DOWN bridge: every op
    // fails typed (fail closed, TB-20) — and none of them can fall back
    // to writing local durable state, because no such surface exists.
    let inputs = zeroclaw_runtime::tachi_bridge::TaskIntentInputs {
        objective: zeroclaw_api::taskintent::BoundedText::new("durability probe objective")
            .expect("bounded"),
        capability_request: zeroclaw_api::taskintent::CapabilityRequest {
            capability: zeroclaw_api::taskintent::Capability::RepositoryImplementation,
        },
        constraints: vec![],
        expected_artifacts: vec![],
        evaluation_requirement: zeroclaw_api::taskintent::EvaluationRequirement {
            independence: zeroclaw_api::taskintent::IndependenceClass::FreshContextCrossVendor,
        },
    };
    let policy = zeroclaw_runtime::tachi_bridge::RequesterBridgePolicy {
        admitted_capabilities: std::collections::BTreeSet::from([
            zeroclaw_api::taskintent::Capability::RepositoryImplementation,
        ]),
        workspace_source: None,
        routing_preference: None,
        approval_requirement: zeroclaw_api::taskintent::ApprovalRequirement::NotRequired,
        privacy_class: zeroclaw_api::taskintent::PrivacyClass::Internal,
    };
    let action = supervisor
        .plan_implementation_task(&inputs, &policy)
        .expect("planning is local and typed — it writes nothing");
    let payload = action.task_intent_request.expect("typed payload");
    // The parent's submit against the down bridge returns typed
    // Unavailable (no local execution fallback anywhere).
    let parent_client = TachiBridgeClient::new(Arc::new(DownBridge));
    match parent_client
        .submit(&payload.intent, &payload.request_id)
        .await
    {
        Ok(zeroclaw_runtime::tachi_bridge::SubmitReceipt::Unavailable) => {}
        other => panic!("down bridge must surface typed Unavailable, got {other:?}"),
    }
    let stray: zeroclaw_api::taskintent::TaskRef =
        serde_json::from_value(serde_json::json!("task:down-bridge-never-minted-one"))
            .expect("wire-shaped ref");
    // Attach (receipt-bound digest verification) against the DOWN
    // bridge: fails typed — the session cannot supervise a task whose
    // intent digest it cannot verify. Then exercise the remaining
    // surfaces: all typed failures, none durable.
    assert!(
        supervisor
            .attach_implementation_task(stray.clone())
            .await
            .is_err(),
        "attach verification against a down bridge fails typed"
    );
    assert!(supervisor.observe(&stray).await.is_err());
    assert!(supervisor.collect_result(&stray).await.is_err());
    assert!(
        supervisor
            .request_independent_review(
                zeroclaw_api::taskintent::IndependenceClass::FreshContextCrossVendor,
                "durability probe review",
            )
            .await
            .is_err(),
        "review submit against a down bridge fails typed (ReviewSubmitRefused)"
    );
    assert!(supervisor.propose_judgment(&stray).await.is_err());
    let report = supervisor.conclude();
    assert_eq!(
        report.status,
        zeroclaw_api::subagent_v1::SubAgentTerminalFact::Completed
    );

    // Zero durable rows AT ALL — one count over the whole table, so a
    // row under ANY agent name fails.
    let total = store.count_all().expect("count-all query");
    assert_eq!(
        total, 0,
        "the supervisor path must leave no control-plane rows under any name"
    );
}

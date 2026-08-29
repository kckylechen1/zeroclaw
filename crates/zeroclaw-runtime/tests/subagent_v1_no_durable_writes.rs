//! SA-26 discrimination, executable half (V1 leaf, durability row): the new V1
//! local SubAgent path creates NO durable Task/Attempt rows in
//! `control_plane.db` or any other ZeroClaw DB.
//!
//! This test lives in its own integration binary on purpose: it installs
//! the process-global control plane (`control_plane::init_control_plane`
//! is a `OnceLock`), which must not leak into any other test's process.

use std::sync::Arc;

use async_trait::async_trait;
use zeroclaw_api::tool::Tool;
use zeroclaw_config::schema::{AliasedAgentConfig, Config, RiskProfileConfig};
use zeroclaw_runtime::subagent_v1::{
    BoundedModelRequest, BoundedModelResponse, ModelAccessResolver, ReasoningSubagentTool,
};

struct StubResolver;

#[async_trait]
impl ModelAccessResolver for StubResolver {
    async fn complete(
        &self,
        _request: BoundedModelRequest,
    ) -> anyhow::Result<BoundedModelResponse> {
        Ok(BoundedModelResponse {
            text: serde_json::json!({
                "summary": "analysis complete",
                "findings": [],
                "uncertainty": [],
                "recommendations": [],
                "requested_parent_actions": [],
                "proposed_candidates": []
            })
            .to_string(),
            tokens_in: 5,
            tokens_out: 5,
        })
    }

    fn provider_ref(&self) -> &str {
        "stub.test"
    }
}

#[tokio::test]
async fn v1_path_writes_no_control_plane_rows() {
    // Install a REAL in-memory control-plane store so the v1 path is
    // running with a live control plane it could write to — and prove
    // it never does.
    let store = Arc::new(
        zeroclaw_runtime::control_plane::task_store_sqlite::SqliteTaskStore::new_in_memory()
            .expect("in-memory store"),
    );
    let handle = zeroclaw_runtime::control_plane::boot::ControlPlaneHandle {
        store: Arc::clone(&store)
            as Arc<dyn zeroclaw_runtime::control_plane::task_registry::TaskRegistry>,
        boot_id: "test-boot".into(),
        sqlite_store: Arc::clone(&store),
        commands: None,
    };
    assert!(
        zeroclaw_runtime::control_plane::init_control_plane(handle),
        "this binary owns the global install; a prior install means test pollution"
    );

    let mut config = Config::default();
    let risk = RiskProfileConfig::default();
    config.risk_profiles.insert("default".to_string(), risk);
    config.agents.insert(
        "parent-agent".to_string(),
        AliasedAgentConfig {
            risk_profile: "default".into(),
            ..AliasedAgentConfig::default()
        },
    );

    let tool = ReasoningSubagentTool::new(
        Arc::new(config),
        "parent-agent",
        Arc::new(zeroclaw_config::policy::SecurityPolicy::default()),
    )
    .with_model_resolver(Arc::new(StubResolver));

    let result = tool
        .execute(serde_json::json!({
            "objective": "Summarize the stability envelope."
        }))
        .await
        .expect("tool executes");
    assert!(result.success, "child must complete: {:?}", result.error);
    assert!(
        result.output.to_string().contains("subagent-v1-"),
        "output must carry the minted run ref: {result:?}"
    );

    // Zero durable rows AT ALL — one count over the whole table, so a
    // row under ANY agent name (not just names we guessed) fails.
    let total = store.count_all().expect("count-all query");
    assert_eq!(
        total, 0,
        "the v1 path must leave no control-plane rows under any name"
    );
    for agent in ["parent-agent", "default-reasoning-v1", "subagent-v1"] {
        let rows = store.count_by_agent(agent).expect("count query");
        assert_eq!(
            rows, 0,
            "the v1 path must leave no control-plane rows under {agent}"
        );
    }
}

#[tokio::test]
async fn nested_usage_fields_reject_unknowns_on_the_wire() {
    // Wire strictness at every nesting level: an extra field inside
    // `usage` (or `profile_ref`) fails to deserialize instead of being
    // silently ignored.
    let report_json = serde_json::json!({
        "run_ref": "subagent-v1-x",
        "profile_ref": {"profile_id": "p", "revision": 1, "digest": "d"},
        "context_bundle_ref": "b",
        "status": "completed",
        "summary": "s",
        "usage": {
            "elapsed_ms": 1, "tokens_in": 1, "tokens_out": 1, "actions": 1,
            "hidden_extra": true
        }
    });
    let parsed: Result<zeroclaw_api::subagent_v1::SubAgentReportV1, _> =
        serde_json::from_value(report_json);
    assert!(parsed.is_err(), "nested usage extras must be rejected");
}

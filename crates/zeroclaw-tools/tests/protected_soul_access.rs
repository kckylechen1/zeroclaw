use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;
use zeroclaw_api::companion::AgentIdentityId;
use zeroclaw_api::tool::Tool;
use zeroclaw_config::policy::SecurityPolicy;
use zeroclaw_config::schema::SearchMode;
use zeroclaw_memory::embeddings::EmbeddingProvider;
use zeroclaw_memory::soul::{CarrierContext, IdentityRegistry, SoulService};
use zeroclaw_memory::{
    AgentScopedMemory, Memory, MemoryCategory, RetrievalConfig, RetrievalPipeline, SqliteMemory,
};
use zeroclaw_tools::memory_export::MemoryExportTool;
use zeroclaw_tools::memory_forget::MemoryForgetTool;
use zeroclaw_tools::memory_recall::MemoryRecallTool;
use zeroclaw_tools::memory_store::MemoryStoreTool;

struct ConstantEmbedding;

#[async_trait]
impl EmbeddingProvider for ConstantEmbedding {
    fn name(&self) -> &str {
        "protected-boundary-test"
    }

    fn dimensions(&self) -> usize {
        2
    }

    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect())
    }
}

#[tokio::test]
async fn model_generic_tools_exclude_soul_while_dedicated_access_survives() {
    let tmp = TempDir::new().unwrap();
    let raw = Arc::new(
        SqliteMemory::with_embedder(
            "sqlite",
            tmp.path(),
            Arc::new(ConstantEmbedding),
            0.7,
            0.3,
            1000,
            None,
            SearchMode::Hybrid,
        )
        .unwrap(),
    );
    let own = raw.ensure_agent_uuid("own").await.unwrap();
    let sibling = raw.ensure_agent_uuid("sibling").await.unwrap();
    let own_soul_key = format!("soul::{own}::disposition");
    let own_candidate_key = format!("soul::{own}::candidate::pending");
    let sibling_candidate_key = format!("soul::{sibling}::candidate::pending");

    for (key, content, agent) in [
        (&own_soul_key, "private disposition", own.as_str()),
        (&own_candidate_key, "private own candidate", own.as_str()),
        (
            &sibling_candidate_key,
            "private sibling candidate",
            sibling.as_str(),
        ),
    ] {
        raw.store_with_agent(
            key,
            content,
            MemoryCategory::Custom("soul".to_string()),
            None,
            Some("soul"),
            None,
            Some(agent),
        )
        .await
        .unwrap();
    }
    for (key, content, agent) in [
        ("own-ambient", "ordinary own memory", own.as_str()),
        (
            "sibling-ambient",
            "ordinary sibling memory",
            sibling.as_str(),
        ),
    ] {
        raw.store_with_agent(
            key,
            content,
            MemoryCategory::Core,
            None,
            None,
            None,
            Some(agent),
        )
        .await
        .unwrap();
    }

    let scoped: Arc<dyn Memory> = Arc::new(RetrievalPipeline::new(
        Arc::new(AgentScopedMemory::new(
            raw.clone(),
            &own,
            vec![sibling.clone()],
        )),
        RetrievalConfig::default(),
    ));
    let security = Arc::new(SecurityPolicy::default());

    for query in ["private candidate", "*"] {
        let result = MemoryRecallTool::new(scoped.clone())
            .execute(json!({"query": query, "limit": 20}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("ordinary own memory"));
        assert!(result.output.contains("ordinary sibling memory"));
        assert!(!result.output.contains("private disposition"));
        assert!(!result.output.contains("private own candidate"));
        assert!(!result.output.contains("private sibling candidate"));
    }

    assert!(scoped.get(&own_soul_key).await.unwrap().is_none());
    assert!(
        scoped
            .list(None, None)
            .await
            .unwrap()
            .iter()
            .all(|entry| entry.namespace != "soul")
    );

    let export = MemoryExportTool::new(scoped.clone())
        .execute(json!({"namespace": "soul"}))
        .await
        .unwrap();
    assert!(export.success);
    assert_eq!(export.output.as_str(), "[]");

    let store = MemoryStoreTool::new(scoped.clone(), security.clone())
        .execute(json!({"key": own_candidate_key, "content": "overwrite"}))
        .await
        .unwrap();
    assert!(!store.success);
    let forget = MemoryForgetTool::new(scoped, security)
        .execute(json!({"key": own_candidate_key}))
        .await
        .unwrap();
    assert!(!forget.success);

    let identity = AgentIdentityId::from_opaque(own.clone());
    let registry = Arc::new(IdentityRegistry::new());
    registry.admit(&identity, "test admission").unwrap();
    let dedicated = SoulService::new(registry, raw.clone() as Arc<dyn Memory>).unwrap();
    assert_eq!(
        dedicated
            .get(&identity, "disposition", &CarrierContext::default())
            .await
            .unwrap()
            .unwrap()
            .content,
        "private disposition"
    );
    assert!(
        raw.get_for_agent(&own_candidate_key, &own)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        raw.get_for_agent(&sibling_candidate_key, &sibling)
            .await
            .unwrap()
            .is_some()
    );
}

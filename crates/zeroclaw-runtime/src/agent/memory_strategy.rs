use std::sync::Arc;
use zeroclaw_api::memory_traits::{Memory, MemoryStrategy};
use zeroclaw_api::model_provider::ModelProvider;

pub struct DefaultMemoryStrategy {
    memory: Arc<dyn Memory>,
    memory_config: zeroclaw_config::schema::MemoryConfig,
    workspace_dir: std::path::PathBuf,
}

impl DefaultMemoryStrategy {
    pub fn new(
        memory: Arc<dyn Memory>,
        memory_config: zeroclaw_config::schema::MemoryConfig,
        workspace_dir: impl Into<std::path::PathBuf>,
    ) -> Self {
        Self {
            memory,
            memory_config,
            workspace_dir: workspace_dir.into(),
        }
    }

    /// Convenience constructor that takes the live `MemoryConfig` so
    /// `run_governance` uses the operator's actual settings (archive
    /// windows, hygiene toggle, etc.) rather than hardcoded defaults.
    pub fn with_config(
        memory: Arc<dyn Memory>,
        memory_config: zeroclaw_config::schema::MemoryConfig,
        workspace_dir: impl Into<std::path::PathBuf>,
    ) -> Self {
        Self::new(memory, memory_config, workspace_dir)
    }
}

#[async_trait::async_trait]
impl MemoryStrategy for DefaultMemoryStrategy {
    async fn consolidate_turn(
        &self,
        user_message: &str,
        assistant_response: &str,
        provider: &dyn ModelProvider,
        model: &str,
        temperature: Option<f64>,
    ) -> anyhow::Result<()> {
        zeroclaw_memory::consolidation::consolidate_turn(
            provider,
            model,
            temperature,
            self.memory.as_ref(),
            &self.memory_config,
            user_message,
            assistant_response,
        )
        .await?;
        // Optional LLM enrichment (tachi overrides; others no-op). Cadence is
        // the 12h enrichment state file — not per-turn. If consolidate_turn is
        // never called, enrichment never runs (no speculative config key).
        if let Err(e) = zeroclaw_memory::run_llm_enrichment_if_due(
            self.memory.as_ref(),
            &self.workspace_dir,
            provider,
            model,
        )
        .await
        {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "memory llm enrichment skipped"
            );
        }
        Ok(())
    }

    async fn run_governance(&self) -> anyhow::Result<()> {
        // Delegate to the existing hygiene routine.
        // Phase 1: `hygiene::run_if_due` returns `Result<()>`.
        // A structured report will be wired in a follow-up when hygiene
        // exposes per-action counters.
        zeroclaw_memory::hygiene::run_if_due(&self.memory_config, &self.workspace_dir)?;
        // Tachi / memcore light-sleep on the live `Memory` handle (trait
        // default is no-op). The factory hygiene cadence in
        // `create_memory_with_storage_and_routes` is the primary live path;
        // this keeps the strategy surface honest without a second DB open.
        self.memory.run_light_sleep_governance()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use zeroclaw_api::attribution::{
        Attributable, MemoryKind, ModelProviderKind, ProviderKind, Role,
    };
    use zeroclaw_api::memory_traits::{MemoryCategory, MemoryEntry};
    use zeroclaw_api::model_provider::ModelProvider;

    /// Minimal Memory stub that counts enrichment calls.
    struct CountingMemory {
        enrichment_calls: AtomicUsize,
    }

    impl Attributable for CountingMemory {
        fn role(&self) -> Role {
            Role::Memory(MemoryKind::None)
        }
        fn alias(&self) -> &str {
            "counting"
        }
    }

    #[async_trait]
    impl Memory for CountingMemory {
        fn name(&self) -> &str {
            "counting"
        }
        async fn store(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn recall(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
            _since: Option<&str>,
            _until: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(vec![])
        }
        async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(None)
        }
        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(vec![])
        }
        async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn forget_for_agent(&self, _key: &str, _agent_id: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn count(&self) -> anyhow::Result<usize> {
            Ok(0)
        }
        async fn health_check(&self) -> bool {
            true
        }
        async fn store_with_agent(
            &self,
            key: &str,
            content: &str,
            category: MemoryCategory,
            session_id: Option<&str>,
            _namespace: Option<&str>,
            _importance: Option<f64>,
            _agent_id: Option<&str>,
        ) -> anyhow::Result<()> {
            self.store(key, content, category, session_id).await
        }
        async fn recall_for_agents(
            &self,
            _allowed_agent_ids: &[&str],
            query: &str,
            limit: usize,
            session_id: Option<&str>,
            since: Option<&str>,
            until: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            self.recall(query, limit, session_id, since, until).await
        }
        async fn run_llm_enrichment(
            &self,
            _provider: &dyn ModelProvider,
            _model: &str,
        ) -> anyhow::Result<usize> {
            self.enrichment_calls.fetch_add(1, Ordering::SeqCst);
            Ok(0)
        }
    }

    struct StubProvider;

    impl Attributable for StubProvider {
        fn role(&self) -> Role {
            Role::Provider(ProviderKind::Model(ModelProviderKind::Custom))
        }
        fn alias(&self) -> &str {
            "StubProvider"
        }
    }

    #[async_trait]
    impl ModelProvider for StubProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            // Minimal valid consolidation JSON so consolidate_turn succeeds.
            Ok(r#"{"history_entry":"noop turn","memory_update":null}"#.into())
        }
    }

    #[tokio::test]
    async fn consolidate_turn_runs_enrichment_when_due_then_skips() {
        let tmp = TempDir::new().unwrap();
        let mem = Arc::new(CountingMemory {
            enrichment_calls: AtomicUsize::new(0),
        });
        let strategy = DefaultMemoryStrategy::new(
            mem.clone(),
            zeroclaw_config::schema::MemoryConfig::default(),
            tmp.path(),
        );
        let provider = StubProvider;

        strategy
            .consolidate_turn("hi", "hello", &provider, "m", None)
            .await
            .unwrap();
        assert_eq!(mem.enrichment_calls.load(Ordering::SeqCst), 1);

        strategy
            .consolidate_turn("hi2", "hello2", &provider, "m", None)
            .await
            .unwrap();
        assert_eq!(
            mem.enrichment_calls.load(Ordering::SeqCst),
            1,
            "second turn within cadence must not re-run enrichment"
        );
    }
}

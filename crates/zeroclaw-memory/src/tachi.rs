//! Tachi / memcore memory backend (feature `tachi`).

use super::embeddings::EmbeddingProvider;
use super::traits::{
    ExportFilter, Memory, MemoryCategory, MemoryEntry, MemoryStats, StoreOptions,
    is_recent_recall_query,
};
use anyhow::Context;
use async_trait::async_trait;
use chrono::Local;
use memcore::{HybridWeights, MemoryStore, SearchOptions};
use parking_lot::{Mutex, RwLock};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

const METADATA_ZC_KEY: &str = "zeroclaw_key";
const METADATA_ZC_CATEGORY: &str = "zeroclaw_category";
const METADATA_SESSION_ID: &str = "session_id";
const METADATA_TENANT_ID: &str = "tenant_id";
const METADATA_PINNED: &str = "pinned";
const METADATA_KIND: &str = "kind";
const DEFAULT_AGENT: &str = "default";
const LIST_LIMIT: usize = 10_000;

#[derive(Clone)]
pub struct TachiMemory {
    alias: String,
    db_path: PathBuf,
    store: Arc<Mutex<MemoryStore>>,
    embedder: Arc<RwLock<Arc<dyn EmbeddingProvider>>>,
    vector_weight: f32,
    keyword_weight: f32,
}

impl TachiMemory {
    pub fn new(alias: &str, workspace_dir: &Path) -> anyhow::Result<Self> {
        Self::with_embedder(
            alias,
            workspace_dir,
            Arc::new(super::embeddings::NoopEmbedding),
            0.7,
            0.3,
        )
    }

    pub fn with_embedder(
        alias: &str,
        workspace_dir: &Path,
        embedder: Arc<dyn EmbeddingProvider>,
        vector_weight: f32,
        keyword_weight: f32,
    ) -> anyhow::Result<Self> {
        let db_path = workspace_dir.join("memory").join("tachi.db");
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let store = MemoryStore::open(
            db_path
                .to_str()
                .context("tachi memory db path is not valid UTF-8")?,
        )
        .map_err(|e| anyhow::Error::msg(format!("failed to open tachi memory db: {e}")))?;
        Ok(Self {
            alias: alias.to_string(),
            db_path,
            store: Arc::new(Mutex::new(store)),
            embedder: Arc::new(RwLock::new(embedder)),
            vector_weight,
            keyword_weight,
        })
    }

    fn hybrid_weights(&self) -> HybridWeights {
        let semantic = f64::from(self.vector_weight);
        let fts = f64::from(self.keyword_weight);
        let remainder = (1.0 - semantic - fts).max(0.0);
        HybridWeights {
            semantic,
            fts,
            symbolic: remainder * 0.5,
            decay: remainder * 0.5,
            use_rrf: true,
        }
    }

    fn category_to_segment(category: &MemoryCategory) -> String {
        match category {
            MemoryCategory::Core => "core".into(),
            MemoryCategory::Daily => "daily".into(),
            MemoryCategory::Conversation => "conversation".into(),
            MemoryCategory::Custom(name) => Self::sanitize_path_segment(name),
        }
    }

    fn category_to_memcore(category: &MemoryCategory) -> String {
        match category {
            MemoryCategory::Core => "fact".into(),
            MemoryCategory::Daily => "experience".into(),
            MemoryCategory::Conversation => "experience".into(),
            MemoryCategory::Custom(_) => "other".into(),
        }
    }

    fn segment_to_category(segment: &str, metadata: &serde_json::Value) -> MemoryCategory {
        if let Some(stored) = metadata.get(METADATA_ZC_CATEGORY).and_then(|v| v.as_str()) {
            return match stored {
                "core" => MemoryCategory::Core,
                "daily" => MemoryCategory::Daily,
                "conversation" => MemoryCategory::Conversation,
                other => MemoryCategory::Custom(other.to_string()),
            };
        }
        match segment {
            "core" => MemoryCategory::Core,
            "daily" => MemoryCategory::Daily,
            "conversation" => MemoryCategory::Conversation,
            other => MemoryCategory::Custom(other.to_string()),
        }
    }

    fn sanitize_path_segment(value: &str) -> String {
        value
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                    ch
                } else {
                    '_'
                }
            })
            .collect()
    }

    fn agent_segment(agent_id: Option<&str>) -> String {
        Self::sanitize_path_segment(agent_id.unwrap_or(DEFAULT_AGENT))
    }

    fn namespace_segment(namespace: Option<&str>) -> String {
        Self::sanitize_path_segment(namespace.unwrap_or("default"))
    }

    fn storage_path(
        agent_id: Option<&str>,
        namespace: Option<&str>,
        category: &MemoryCategory,
        key: &str,
    ) -> String {
        format!(
            "/agents/{}/{}/{}/{}",
            Self::agent_segment(agent_id),
            Self::namespace_segment(namespace),
            Self::category_to_segment(category),
            Self::sanitize_path_segment(key)
        )
    }

    fn path_namespace(path: &str) -> Option<String> {
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        if parts.len() >= 3 && parts[0] == "agents" {
            Some(parts[2].to_string())
        } else {
            None
        }
    }

    fn path_agent_id(path: &str) -> Option<String> {
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        if parts.len() >= 2 && parts[0] == "agents" {
            Some(parts[1].to_string())
        } else {
            None
        }
    }

    fn path_category_segment(path: &str) -> Option<&str> {
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        if parts.len() >= 4 && parts[0] == "agents" {
            Some(parts[3])
        } else {
            None
        }
    }

    fn path_key(path: &str) -> String {
        path.rsplit('/').next().unwrap_or(path).to_string()
    }

    fn build_metadata(
        key: &str,
        category: &MemoryCategory,
        session_id: Option<&str>,
        options: &StoreOptions,
    ) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert(
            METADATA_ZC_KEY.to_string(),
            serde_json::Value::String(key.to_string()),
        );
        obj.insert(
            METADATA_ZC_CATEGORY.to_string(),
            serde_json::Value::String(category.to_string()),
        );
        if let Some(sid) = session_id {
            obj.insert(
                METADATA_SESSION_ID.to_string(),
                serde_json::Value::String(sid.to_string()),
            );
        }
        if let Some(tenant) = options.tenant_id.as_deref() {
            obj.insert(
                METADATA_TENANT_ID.to_string(),
                serde_json::Value::String(tenant.to_string()),
            );
        }
        if options.pinned {
            obj.insert(METADATA_PINNED.to_string(), serde_json::Value::Bool(true));
        }
        if let Some(kind) = &options.kind
            && let Ok(value) = serde_json::to_value(kind)
        {
            obj.insert(METADATA_KIND.to_string(), value);
        }
        serde_json::Value::Object(obj)
    }

    async fn compute_embedding(&self, text: &str) -> Option<Vec<f32>> {
        let embedder = self.embedder.read().clone();
        if embedder.dimensions() == 0 {
            return None;
        }
        embedder.embed_one(text).await.ok()
    }

    fn memcore_to_zeroclaw(entry: memcore::MemoryEntry, score: Option<f64>) -> MemoryEntry {
        let metadata = entry.metadata.clone();
        let key = metadata
            .get(METADATA_ZC_KEY)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| Self::path_key(&entry.path));
        let category_segment = Self::path_category_segment(&entry.path).unwrap_or("core");
        let category = Self::segment_to_category(category_segment, &metadata);
        let session_id = metadata
            .get(METADATA_SESSION_ID)
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let tenant_id = metadata
            .get(METADATA_TENANT_ID)
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let pinned = metadata
            .get(METADATA_PINNED)
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let kind = metadata
            .get(METADATA_KIND)
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        let agent_id = Self::path_agent_id(&entry.path);
        MemoryEntry {
            id: entry.id,
            key,
            content: entry.text,
            category,
            timestamp: entry.timestamp,
            session_id,
            score,
            namespace: Self::path_namespace(&entry.path).unwrap_or_else(|| entry.scope.clone()),
            importance: Some(entry.importance),
            superseded_by: None,
            kind,
            pinned,
            tenant_id,
            agent_alias: agent_id.clone(),
            agent_id,
        }
    }

    fn entry_in_window(timestamp: &str, since: Option<&str>, until: Option<&str>) -> bool {
        if let Some(s) = since
            && timestamp < s
        {
            return false;
        }
        if let Some(u) = until
            && timestamp > u
        {
            return false;
        }
        true
    }

    fn filter_entries(
        entries: Vec<MemoryEntry>,
        session_id: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
        namespace: Option<&str>,
        allowed_agents: Option<&[&str]>,
        limit: usize,
    ) -> Vec<MemoryEntry> {
        entries
            .into_iter()
            .filter(|entry| {
                if let Some(sid) = session_id
                    && entry.session_id.as_deref() != Some(sid)
                {
                    return false;
                }
                if !Self::entry_in_window(&entry.timestamp, since, until) {
                    return false;
                }
                if let Some(ns) = namespace
                    && entry.namespace != ns
                {
                    return false;
                }
                if let Some(allowed) = allowed_agents {
                    let agent = entry.agent_id.as_deref().unwrap_or(DEFAULT_AGENT);
                    if !allowed.contains(&agent) {
                        return false;
                    }
                }
                true
            })
            .take(limit)
            .collect()
    }

    async fn store_internal(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        namespace: Option<&str>,
        importance: Option<f64>,
        agent_id: Option<&str>,
        options: StoreOptions,
    ) -> anyhow::Result<()> {
        let path = Self::storage_path(agent_id, namespace, &category, key);
        let embedding = self.compute_embedding(content).await;
        let now = Local::now().to_rfc3339();
        let importance = importance.unwrap_or(0.7);
        let scope = match namespace {
            Some("default") | None => "general".to_string(),
            Some("user") => "user".to_string(),
            Some(ns) if ns.starts_with("user:") => "user".to_string(),
            _ => "project".to_string(),
        };
        let metadata = Self::build_metadata(key, &category, session_id, &options);

        let store = self.store.clone();
        let path_c = path.clone();
        let content_c = content.to_string();
        let category_c = Self::category_to_memcore(&category);
        let embedding_c = embedding.clone();
        let scope_c = scope;

        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let mut store = store.lock();
            let existing_id = store
                .list_by_path(&path_c, 8, false)
                .ok()
                .and_then(|rows| rows.into_iter().find(|e| e.path == path_c).map(|e| e.id));

            let id = existing_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            let entry = memcore::MemoryEntry {
                id,
                path: path_c,
                summary: content_c.chars().take(100).collect(),
                text: content_c,
                importance,
                timestamp: now.clone(),
                valid_from: now,
                valid_until: None,
                category: category_c,
                topic: String::new(),
                keywords: Vec::new(),
                persons: Vec::new(),
                entities: Vec::new(),
                location: String::new(),
                source: "zeroclaw".into(),
                scope: scope_c,
                archived: false,
                access_count: 0,
                last_access: None,
                revision: 1,
                vector: embedding_c,
                retention_policy: None,
                domain: None,
                metadata,
                recall_count: 0,
                query_diversity: 0,
                tier: memcore::types::default_tier(),
            };
            store
                .upsert(&entry)
                .map_err(|e| anyhow::Error::msg(format!("tachi upsert failed: {e}")))
        })
        .await??;
        Ok(())
    }

    async fn list_all(&self) -> anyhow::Result<Vec<MemoryEntry>> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<MemoryEntry>> {
            let store = store.lock();
            let rows = store
                .list_by_path("/agents", LIST_LIMIT, false)
                .map_err(|e| anyhow::Error::msg(format!("tachi list failed: {e}")))?;
            Ok(rows
                .into_iter()
                .map(|row| Self::memcore_to_zeroclaw(row, None))
                .collect())
        })
        .await?
    }

    async fn recall_internal(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
        namespace: Option<&str>,
        allowed_agents: Option<&[&str]>,
        path_prefix: Option<String>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        if is_recent_recall_query(query) {
            let mut entries = self.list_all().await?;
            entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
            entries = Self::filter_entries(
                entries,
                session_id,
                since,
                until,
                namespace,
                allowed_agents,
                limit,
            );
            for entry in &mut entries {
                entry.score = Some(1.0);
            }
            return Ok(entries);
        }

        let query_vec = self.compute_embedding(query).await;
        let store = self.store.clone();
        let query_c = query.to_string();
        let weights = self.hybrid_weights();
        let path_prefix_c = path_prefix.clone();

        let mut results =
            tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<MemoryEntry>> {
                let store = store.lock();
                let vec_available = store.vec_available;
                let opts = SearchOptions {
                    top_k: limit.max(6),
                    candidates_per_channel: limit.max(20),
                    weights,
                    path_prefix: path_prefix_c,
                    query_vec,
                    vec_available,
                    record_access: true,
                    ..Default::default()
                };
                let hits = store
                    .search(&query_c, Some(opts))
                    .map_err(|e| anyhow::Error::msg(format!("tachi search failed: {e}")))?;
                Ok(hits
                    .into_iter()
                    .map(|hit| Self::memcore_to_zeroclaw(hit.entry, Some(hit.score.final_score)))
                    .collect())
            })
            .await??;

        results = Self::filter_entries(
            results,
            session_id,
            since,
            until,
            namespace,
            allowed_agents,
            limit,
        );
        Ok(results)
    }

    fn swap_embedder(&self, embedder: Arc<dyn EmbeddingProvider>) {
        *self.embedder.write() = embedder;
    }
}

#[async_trait]
impl Memory for TachiMemory {
    fn name(&self) -> &str {
        &self.alias
    }

    fn refresh_embedder(
        &self,
        model_provider: &str,
        api_key: Option<&str>,
        model: &str,
        dimensions: usize,
    ) {
        let embedder: Arc<dyn EmbeddingProvider> =
            Arc::from(super::embeddings::create_embedding_provider(
                model_provider,
                api_key,
                model,
                dimensions,
            ));
        self.swap_embedder(embedder);
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.store_with_agent(key, content, category, session_id, None, None, None)
            .await
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.recall_internal(query, limit, session_id, since, until, None, None, None)
            .await
    }

    async fn recall_namespaced(
        &self,
        namespace: &str,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.recall_internal(
            query,
            limit,
            session_id,
            since,
            until,
            Some(namespace),
            None,
            None,
        )
        .await
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        let entries = self.list_all().await?;
        Ok(entries.into_iter().find(|e| e.key == key))
    }

    async fn get_for_agent(
        &self,
        key: &str,
        agent_id: &str,
    ) -> anyhow::Result<Option<MemoryEntry>> {
        let prefix = format!("/agents/{}/", Self::sanitize_path_segment(agent_id));
        let key_seg = Self::sanitize_path_segment(key);
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Option<MemoryEntry>> {
            let store = store.lock();
            let hit = store
                .list_by_path(&prefix, LIST_LIMIT, false)
                .map_err(|e| anyhow::Error::msg(format!("{e}")))?
                .into_iter()
                .find(|row| Self::path_key(&row.path) == key_seg);
            Ok(hit.map(|row| Self::memcore_to_zeroclaw(row, None)))
        })
        .await?
    }

    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let mut entries = self.list_all().await?;
        if let Some(cat) = category {
            entries.retain(|e| &e.category == cat);
        }
        if let Some(sid) = session_id {
            entries.retain(|e| e.session_id.as_deref() == Some(sid));
        }
        Ok(entries)
    }

    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        let entries = self.list_all().await?;
        let ids: Vec<String> = entries
            .into_iter()
            .filter(|e| e.key == key)
            .map(|e| e.id)
            .collect();
        if ids.is_empty() {
            return Ok(false);
        }
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
            let mut store = store.lock();
            let mut deleted = false;
            for id in ids {
                if store
                    .delete(&id)
                    .map_err(|e| anyhow::Error::msg(format!("{e}")))?
                {
                    deleted = true;
                }
            }
            Ok(deleted)
        })
        .await?
    }

    async fn forget_for_agent(&self, key: &str, agent_id: &str) -> anyhow::Result<bool> {
        let store = self.store.clone();
        let agent_id = agent_id.to_string();
        let key = key.to_string();
        tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
            let mut store = store.lock();
            let prefix = format!("/agents/{}/", Self::sanitize_path_segment(&agent_id));
            let rows = store
                .list_by_path(&prefix, LIST_LIMIT, false)
                .map_err(|e| anyhow::Error::msg(format!("{e}")))?;
            let ids: Vec<String> = rows
                .into_iter()
                .filter(|row| Self::path_key(&row.path) == Self::sanitize_path_segment(&key))
                .map(|row| row.id)
                .collect();
            if ids.is_empty() {
                return Ok(false);
            }
            let mut deleted = false;
            for id in ids {
                if store
                    .delete(&id)
                    .map_err(|e| anyhow::Error::msg(format!("{e}")))?
                {
                    deleted = true;
                }
            }
            Ok(deleted)
        })
        .await?
    }

    async fn purge_namespace(&self, namespace: &str) -> anyhow::Result<usize> {
        let entries = self.list_all().await?;
        let ids: Vec<String> = entries
            .into_iter()
            .filter(|e| e.namespace == namespace)
            .map(|e| e.id)
            .collect();
        self.delete_ids(ids).await
    }

    async fn purge_agent(&self, agent_alias: &str) -> anyhow::Result<usize> {
        let prefix = format!("/agents/{}/", Self::sanitize_path_segment(agent_alias));
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
            let mut store = store.lock();
            let rows = store
                .list_by_path(&prefix, LIST_LIMIT, false)
                .map_err(|e| anyhow::Error::msg(format!("{e}")))?;
            let ids: Vec<String> = rows.into_iter().map(|row| row.id).collect();
            let mut deleted = 0usize;
            for id in ids {
                if store
                    .delete(&id)
                    .map_err(|e| anyhow::Error::msg(format!("{e}")))?
                {
                    deleted += 1;
                }
            }
            Ok(deleted)
        })
        .await?
    }

    async fn count_agent(&self, agent_alias: &str) -> anyhow::Result<usize> {
        let prefix = format!("/agents/{}/", Self::sanitize_path_segment(agent_alias));
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
            let store = store.lock();
            let rows = store
                .list_by_path(&prefix, LIST_LIMIT, false)
                .map_err(|e| anyhow::Error::msg(format!("{e}")))?;
            Ok(rows.len())
        })
        .await?
    }

    async fn count(&self) -> anyhow::Result<usize> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
            let store = store.lock();
            let stats = store
                .stats(false)
                .map_err(|e| anyhow::Error::msg(format!("{e}")))?;
            Ok(stats.total as usize)
        })
        .await?
    }

    async fn health_check(&self) -> bool {
        if !self.db_path.exists() {
            return false;
        }
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let store = store.lock();
            store.quick_check().unwrap_or(false)
        })
        .await
        .unwrap_or(false)
    }

    async fn store_with_metadata(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        namespace: Option<&str>,
        importance: Option<f64>,
    ) -> anyhow::Result<()> {
        self.store_with_agent(
            key, content, category, session_id, namespace, importance, None,
        )
        .await
    }

    async fn store_with_options(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        options: StoreOptions,
    ) -> anyhow::Result<()> {
        let namespace = options.namespace.clone();
        self.store_internal(
            key,
            content,
            category,
            session_id,
            namespace.as_deref(),
            options.importance,
            None,
            options,
        )
        .await
    }

    async fn store_with_agent(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        namespace: Option<&str>,
        importance: Option<f64>,
        agent_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.store_internal(
            key,
            content,
            category,
            session_id,
            namespace,
            importance,
            agent_id,
            StoreOptions::default(),
        )
        .await
    }

    async fn recall_for_agents(
        &self,
        allowed_agent_ids: &[&str],
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        if allowed_agent_ids.is_empty() {
            return self.recall(query, limit, session_id, since, until).await;
        }
        if allowed_agent_ids.len() == 1 {
            let prefix = format!(
                "/agents/{}/",
                Self::sanitize_path_segment(allowed_agent_ids[0])
            );
            return self
                .recall_internal(
                    query,
                    limit,
                    session_id,
                    since,
                    until,
                    None,
                    Some(allowed_agent_ids),
                    Some(prefix),
                )
                .await;
        }
        self.recall_internal(
            query,
            limit,
            session_id,
            since,
            until,
            None,
            Some(allowed_agent_ids),
            Some("/agents".into()),
        )
        .await
    }

    async fn export(&self, filter: &ExportFilter) -> anyhow::Result<Vec<MemoryEntry>> {
        let mut entries = self
            .list(filter.category.as_ref(), filter.session_id.as_deref())
            .await?;
        entries.retain(|e| {
            if let Some(ref ns) = filter.namespace
                && e.namespace != *ns
            {
                return false;
            }
            if let Some(ref since) = filter.since
                && e.timestamp.as_str() < since.as_str()
            {
                return false;
            }
            if let Some(ref until) = filter.until
                && e.timestamp.as_str() > until.as_str()
            {
                return false;
            }
            true
        });
        Ok(entries)
    }

    async fn stats(&self) -> anyhow::Result<MemoryStats> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<MemoryStats> {
            let store = store.lock();
            let stats = store
                .stats(false)
                .map_err(|e| anyhow::Error::msg(format!("{e}")))?;
            let by_category: Vec<(String, u64)> = stats.by_category.into_iter().collect();
            Ok(MemoryStats {
                total_rows: stats.total,
                by_category,
                superseded_rows: 0,
                pinned_rows: 0,
                bytes: 0,
            })
        })
        .await?
    }
}

impl TachiMemory {
    async fn delete_ids(&self, ids: Vec<String>) -> anyhow::Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
            let mut store = store.lock();
            let mut deleted = 0usize;
            for id in ids {
                if store
                    .delete(&id)
                    .map_err(|e| anyhow::Error::msg(format!("{e}")))?
                {
                    deleted += 1;
                }
            }
            Ok(deleted)
        })
        .await?
    }
}

impl ::zeroclaw_api::attribution::Attributable for TachiMemory {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        ::zeroclaw_api::attribution::Role::Memory(::zeroclaw_api::attribution::MemoryKind::Plugin)
    }

    fn alias(&self) -> &str {
        &self.alias
    }
}

#[cfg(all(test, feature = "tachi"))]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_tachi() -> (TempDir, TachiMemory) {
        let tmp = TempDir::new().unwrap();
        let mem = TachiMemory::new("tachi", tmp.path()).unwrap();
        (tmp, mem)
    }

    #[tokio::test]
    async fn store_recall_roundtrip_fts_with_noop_embedder() {
        let (_tmp, mem) = temp_tachi();
        mem.store(
            "lang_pref",
            "The operator prefers concise Rust answers",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();

        let hits = mem.recall("Rust", 5, None, None, None).await.unwrap();
        assert!(
            hits.iter().any(|e| e.key == "lang_pref"),
            "expected FTS recall for keyword 'Rust', got: {hits:?}"
        );
    }

    #[tokio::test]
    async fn agent_scoping_isolates_recall_and_forget() {
        let (_tmp, mem) = temp_tachi();
        let alpha = mem.ensure_agent_uuid("alpha").await.unwrap();
        let beta = mem.ensure_agent_uuid("beta").await.unwrap();

        mem.store_with_agent(
            "note",
            "alpha-only secret token",
            MemoryCategory::Core,
            None,
            None,
            None,
            Some(&alpha),
        )
        .await
        .unwrap();
        mem.store_with_agent(
            "note",
            "beta-only secret token",
            MemoryCategory::Core,
            None,
            None,
            None,
            Some(&beta),
        )
        .await
        .unwrap();

        let alpha_hits = mem
            .recall_for_agents(&[&alpha], "alpha-only", 5, None, None, None)
            .await
            .unwrap();
        assert_eq!(alpha_hits.len(), 1);
        assert!(alpha_hits[0].content.contains("alpha-only"));

        let beta_hits = mem
            .recall_for_agents(&[&beta], "beta-only", 5, None, None, None)
            .await
            .unwrap();
        assert_eq!(beta_hits.len(), 1);
        assert!(beta_hits[0].content.contains("beta-only"));

        assert!(mem.forget_for_agent("note", &alpha).await.unwrap());
        let after = mem
            .recall_for_agents(&[&alpha], "alpha-only", 5, None, None, None)
            .await
            .unwrap();
        assert!(after.is_empty());
        let beta_still = mem
            .recall_for_agents(&[&beta], "beta-only", 5, None, None, None)
            .await
            .unwrap();
        assert_eq!(beta_still.len(), 1);
    }

    #[tokio::test]
    async fn namespace_isolation_via_recall_namespaced() {
        let (_tmp, mem) = temp_tachi();
        mem.store_with_agent(
            "hyperion_cfg",
            "hyperion namespace marker",
            MemoryCategory::Core,
            None,
            Some("hyperion"),
            None,
            None,
        )
        .await
        .unwrap();
        mem.store_with_agent(
            "default_cfg",
            "default namespace marker",
            MemoryCategory::Core,
            None,
            Some("default"),
            None,
            None,
        )
        .await
        .unwrap();

        let hyperion = mem
            .recall_namespaced("hyperion", "*", 5, None, None, None)
            .await
            .unwrap();
        assert_eq!(hyperion.len(), 1);
        assert!(hyperion[0].content.contains("hyperion namespace"));

        let default_ns = mem
            .recall_namespaced("default", "*", 5, None, None, None)
            .await
            .unwrap();
        assert_eq!(default_ns.len(), 1);
        assert!(default_ns[0].content.contains("default namespace"));
    }

    #[tokio::test]
    async fn category_mapping_roundtrip() {
        let (_tmp, mem) = temp_tachi();
        mem.store(
            "daily_note",
            "Shipped tachi backend scaffold",
            MemoryCategory::Daily,
            None,
        )
        .await
        .unwrap();

        let entry = mem.get("daily_note").await.unwrap().expect("stored row");
        assert_eq!(entry.category, MemoryCategory::Daily);

        let listed = mem.list(Some(&MemoryCategory::Daily), None).await.unwrap();
        assert!(listed.iter().any(|e| e.key == "daily_note"));
    }
}

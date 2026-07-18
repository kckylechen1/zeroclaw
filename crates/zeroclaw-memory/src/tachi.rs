//! Tachi / memcore memory backend (feature `tachi`).
//!
//! # Licensing
//!
//! `memcore` is licensed under **AGPL-3.0**. Any distributed binary built with
//! the `memory-tachi` / `tachi` feature is therefore AGPL-affected. Keep that
//! feature off for releases that must remain under ZeroClaw's own license terms
//! alone.
//!
//! # Hyperion deployments
//!
//! Hyperion trading memory must keep using the hapi memory-server MCP on
//! `:6888` (`hapi_save` / `hapi_search` / `hapi_memory`). Do **not** set
//! `memory.backend = "tachi"` for trading memory — this backend is for
//! ZeroClaw-native agent stores (e.g. RomanBath), not the Hyperion memory
//! contract in `AGENTS.md`.
//!
//! # Known limitations (Phase 2)
//!
//! - `reindex` and `supersede` are not implemented (trait defaults).
//! - Tier lifecycle / consolidation / GC stay in memcore; this scaffold does
//!   not drive them.
//! - Embedding-identity reconciliation (`auto_reindex_on_identity_change`) is
//!   sqlite-specific and not mirrored here.
//! - Hygiene / snapshot / auto-hydrate are not wired for this backend.

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
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

const METADATA_ZC_KEY: &str = "zeroclaw_key";
const METADATA_ZC_CATEGORY: &str = "zeroclaw_category";
const METADATA_ZC_NAMESPACE: &str = "zeroclaw_namespace";
const METADATA_ZC_AGENT: &str = "zeroclaw_agent";
const METADATA_SESSION_ID: &str = "session_id";
const METADATA_TENANT_ID: &str = "tenant_id";
const METADATA_PINNED: &str = "pinned";
const METADATA_KIND: &str = "kind";
const DEFAULT_AGENT: &str = "default";
const DEFAULT_NAMESPACE: &str = "default";
/// Initial page size when growing a prefix scan to completeness.
const DEFAULT_LIST_PAGE_SIZE: usize = 512;
const MAX_LIST_GROW: usize = 1_048_576;

#[derive(Clone)]
pub struct TachiMemory {
    alias: String,
    db_path: PathBuf,
    store: Arc<Mutex<MemoryStore>>,
    embedder: Arc<RwLock<Arc<dyn EmbeddingProvider>>>,
    vector_weight: f32,
    keyword_weight: f32,
    /// Page size seed for complete prefix scans (overridable in tests).
    list_page_size: usize,
    /// Upper bound for growing prefix scans (overridable in tests).
    max_list_grow: usize,
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
        Self::with_embedder_and_page_size(
            alias,
            workspace_dir,
            embedder,
            vector_weight,
            keyword_weight,
            DEFAULT_LIST_PAGE_SIZE,
            MAX_LIST_GROW,
        )
    }

    /// Like [`Self::with_embedder`], but with injectable list page-size / grow
    /// caps for regression tests.
    pub fn with_embedder_and_page_size(
        alias: &str,
        workspace_dir: &Path,
        embedder: Arc<dyn EmbeddingProvider>,
        vector_weight: f32,
        keyword_weight: f32,
        list_page_size: usize,
        max_list_grow: usize,
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
        let list_page_size = list_page_size.max(1);
        Ok(Self {
            alias: alias.to_string(),
            db_path,
            store: Arc::new(Mutex::new(store)),
            embedder: Arc::new(RwLock::new(embedder)),
            vector_weight,
            keyword_weight,
            list_page_size,
            max_list_grow: max_list_grow.max(list_page_size),
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
            MemoryCategory::Custom(name) => Self::encode_path_segment(name),
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

    fn segment_to_category(_segment: &str, metadata: &serde_json::Value) -> MemoryCategory {
        if let Some(stored) = metadata.get(METADATA_ZC_CATEGORY).and_then(|v| v.as_str()) {
            return match stored {
                "core" => MemoryCategory::Core,
                "daily" => MemoryCategory::Daily,
                "conversation" => MemoryCategory::Conversation,
                other => MemoryCategory::Custom(other.to_string()),
            };
        }
        MemoryCategory::Core
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

    /// Injective path segment: always `{sanitized}__{hash}` where `hash` is the
    /// first 8 **bytes** (16 hex chars, 64 bits) of SHA-256 of the original, so
    /// a literal key that looks like an encoded form cannot collide with the
    /// encoding of a different key. Logical identity lives in metadata.
    fn encode_path_segment(value: &str) -> String {
        let sanitized = Self::sanitize_path_segment(value);
        let hash = Self::short_hash(value);
        if sanitized.is_empty() {
            format!("x__{hash}")
        } else {
            format!("{sanitized}__{hash}")
        }
    }

    fn short_hash(value: &str) -> String {
        let digest = Sha256::digest(value.as_bytes());
        format!(
            "{:016x}",
            u64::from_be_bytes(
                digest[..8]
                    .try_into()
                    .expect("SHA-256 always produces >= 8 bytes")
            )
        )
    }

    fn agent_segment(agent_id: Option<&str>) -> String {
        Self::encode_path_segment(agent_id.unwrap_or(DEFAULT_AGENT))
    }

    fn namespace_segment(namespace: Option<&str>) -> String {
        Self::encode_path_segment(namespace.unwrap_or(DEFAULT_NAMESPACE))
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
            Self::encode_path_segment(key)
        )
    }

    fn build_metadata(
        key: &str,
        category: &MemoryCategory,
        session_id: Option<&str>,
        namespace: Option<&str>,
        agent_id: Option<&str>,
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
        obj.insert(
            METADATA_ZC_NAMESPACE.to_string(),
            serde_json::Value::String(namespace.unwrap_or(DEFAULT_NAMESPACE).to_string()),
        );
        obj.insert(
            METADATA_ZC_AGENT.to_string(),
            serde_json::Value::String(agent_id.unwrap_or(DEFAULT_AGENT).to_string()),
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

    fn metadata_str(metadata: &serde_json::Value, key: &str) -> Option<String> {
        metadata
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
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
        let key = Self::metadata_str(&metadata, METADATA_ZC_KEY).unwrap_or_else(|| {
            entry
                .path
                .rsplit('/')
                .next()
                .unwrap_or(&entry.path)
                .to_string()
        });
        let category = Self::segment_to_category("", &metadata);
        let session_id = Self::metadata_str(&metadata, METADATA_SESSION_ID);
        let tenant_id = Self::metadata_str(&metadata, METADATA_TENANT_ID);
        let pinned = metadata
            .get(METADATA_PINNED)
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let kind = metadata
            .get(METADATA_KIND)
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        let agent_id = Self::metadata_str(&metadata, METADATA_ZC_AGENT);
        let namespace = Self::metadata_str(&metadata, METADATA_ZC_NAMESPACE)
            .unwrap_or_else(|| DEFAULT_NAMESPACE.to_string());
        MemoryEntry {
            id: entry.id,
            key,
            content: entry.text,
            category,
            timestamp: entry.timestamp,
            session_id,
            score,
            namespace,
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

    /// Grow `list_by_path` until the page is not full. If the page is still
    /// full at `max_grow`, fail loud — silent truncation would leave rows
    /// surviving purge/delete.
    fn list_prefix_complete(
        store: &MemoryStore,
        prefix: &str,
        page_size: usize,
        max_grow: usize,
    ) -> anyhow::Result<Vec<memcore::MemoryEntry>> {
        let mut limit = page_size.max(1);
        let max_grow = max_grow.max(limit);
        loop {
            let rows = store
                .list_by_path(prefix, limit, false)
                .map_err(|e| anyhow::Error::msg(format!("tachi list failed: {e}")))?;
            if rows.len() < limit {
                return Ok(rows);
            }
            if limit >= max_grow {
                return Err(anyhow::Error::msg(format!(
                    "tachi list truncated: prefix={prefix} returned {limit} rows at max_list_grow={max_grow}"
                )));
            }
            limit = limit.saturating_mul(2).min(max_grow);
        }
    }

    fn list_prefix_recent(
        store: &MemoryStore,
        prefix: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<memcore::MemoryEntry>> {
        store
            .list_by_path_recent(prefix, limit.max(1), false)
            .map_err(|e| anyhow::Error::msg(format!("tachi list_recent failed: {e}")))
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
        let metadata =
            Self::build_metadata(key, &category, session_id, namespace, agent_id, &options);

        let store = self.store.clone();
        let path_c = path.clone();
        let content_c = content.to_string();
        let category_c = Self::category_to_memcore(&category);
        let embedding_c = embedding.clone();
        let scope_c = scope;
        let page_size = self.list_page_size;
        let max_grow = self.max_list_grow;
        let key_owned = key.to_string();
        // Sqlite upsert identity is `(agent_id, key)` — see
        // `sqlite.rs` `ON CONFLICT(agent_id, key)`. Namespace/category update
        // in place; they are not part of the identity tuple.
        let agent_owned = agent_id.unwrap_or(DEFAULT_AGENT).to_string();

        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let mut store = store.lock();
            // Exact-path hit first; fall back to sqlite identity (agent, key)
            // so category/namespace changes update the same row.
            let exact_hit = store
                .list_by_path(&path_c, 8, false)
                .ok()
                .and_then(|rows| rows.into_iter().find(|e| e.path == path_c).map(|e| e.id));
            let existing_id = match exact_hit {
                Some(id) => Some(id),
                None => {
                    let agent_prefix =
                        format!("/agents/{}/", Self::agent_segment(Some(&agent_owned)));
                    // Propagate truncation errors: `.ok()` here would treat an
                    // incomplete scan as "no existing row" and fork identity
                    // with a fresh UUID.
                    Self::list_prefix_complete(&store, &agent_prefix, page_size, max_grow)?
                        .into_iter()
                        .find(|e| {
                            Self::metadata_str(&e.metadata, METADATA_ZC_KEY).as_deref()
                                == Some(key_owned.as_str())
                                && Self::metadata_str(&e.metadata, METADATA_ZC_AGENT).as_deref()
                                    == Some(agent_owned.as_str())
                        })
                        .map(|e| e.id)
                }
            };

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
        let page_size = self.list_page_size;
        let max_grow = self.max_list_grow;
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<MemoryEntry>> {
            let store = store.lock();
            let rows = Self::list_prefix_complete(&store, "/agents", page_size, max_grow)?;
            Ok(rows
                .into_iter()
                .map(|row| Self::memcore_to_zeroclaw(row, None))
                .collect())
        })
        .await?
    }

    async fn find_rows_by_key(
        &self,
        key: &str,
        agent_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let store = self.store.clone();
        let page_size = self.list_page_size;
        let max_grow = self.max_list_grow;
        let key = key.to_string();
        let prefix = match agent_id {
            Some(aid) => format!("/agents/{}/", Self::agent_segment(Some(aid))),
            None => "/agents".to_string(),
        };
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<MemoryEntry>> {
            let store = store.lock();
            let rows = Self::list_prefix_complete(&store, &prefix, page_size, max_grow)?;
            Ok(rows
                .into_iter()
                .filter(|row| {
                    Self::metadata_str(&row.metadata, METADATA_ZC_KEY).as_deref()
                        == Some(key.as_str())
                })
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
        let prefix = path_prefix.unwrap_or_else(|| "/agents".to_string());
        if is_recent_recall_query(query) {
            let store = self.store.clone();
            // Over-fetch so post-filters still fill `limit`.
            let fetch = limit.saturating_mul(4).max(limit).max(16);
            let prefix_c = prefix.clone();
            let mut entries =
                tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<MemoryEntry>> {
                    let store = store.lock();
                    let rows = Self::list_prefix_recent(&store, &prefix_c, fetch)?;
                    Ok(rows
                        .into_iter()
                        .map(|row| Self::memcore_to_zeroclaw(row, Some(1.0)))
                        .collect())
                })
                .await??;
            entries = Self::filter_entries(
                entries,
                session_id,
                since,
                until,
                namespace,
                allowed_agents,
                limit,
            );
            return Ok(entries);
        }

        let query_vec = self.compute_embedding(query).await;
        let store = self.store.clone();
        let query_c = query.to_string();
        let weights = self.hybrid_weights();
        let path_prefix_c = Some(prefix);

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

    async fn list_agent_entries(&self, agent_alias: &str) -> anyhow::Result<Vec<MemoryEntry>> {
        let store = self.store.clone();
        let page_size = self.list_page_size;
        let max_grow = self.max_list_grow;
        let prefix = format!("/agents/{}/", Self::agent_segment(Some(agent_alias)));
        let agent_alias = agent_alias.to_string();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<MemoryEntry>> {
            let store = store.lock();
            let rows = Self::list_prefix_complete(&store, &prefix, page_size, max_grow)?;
            Ok(rows
                .into_iter()
                .filter(|row| {
                    Self::metadata_str(&row.metadata, METADATA_ZC_AGENT).as_deref()
                        == Some(agent_alias.as_str())
                        || Self::agent_segment(Some(&agent_alias))
                            == row
                                .path
                                .trim_start_matches('/')
                                .split('/')
                                .nth(1)
                                .unwrap_or("")
                })
                .map(|row| Self::memcore_to_zeroclaw(row, None))
                .collect())
        })
        .await?
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
        let mut rows = self.find_rows_by_key(key, None).await?;
        Ok(rows.pop())
    }

    async fn get_for_agent(
        &self,
        key: &str,
        agent_id: &str,
    ) -> anyhow::Result<Option<MemoryEntry>> {
        let mut rows = self.find_rows_by_key(key, Some(agent_id)).await?;
        Ok(rows.pop())
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
        let rows = self.find_rows_by_key(key, None).await?;
        let ids: Vec<String> = rows.into_iter().map(|e| e.id).collect();
        Ok(self.delete_ids(ids).await? > 0)
    }

    async fn forget_for_agent(&self, key: &str, agent_id: &str) -> anyhow::Result<bool> {
        let rows = self.find_rows_by_key(key, Some(agent_id)).await?;
        let ids: Vec<String> = rows.into_iter().map(|e| e.id).collect();
        Ok(self.delete_ids(ids).await? > 0)
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

    async fn purge_session(&self, session_id: &str) -> anyhow::Result<usize> {
        let entries = self.list_all().await?;
        let ids: Vec<String> = entries
            .into_iter()
            .filter(|e| e.session_id.as_deref() == Some(session_id))
            .map(|e| e.id)
            .collect();
        self.delete_ids(ids).await
    }

    async fn purge_session_for_agent(
        &self,
        session_id: &str,
        agent_id: &str,
    ) -> anyhow::Result<usize> {
        let entries = self.list_all().await?;
        let ids: Vec<String> = entries
            .into_iter()
            .filter(|e| {
                e.session_id.as_deref() == Some(session_id)
                    && e.agent_id.as_deref() == Some(agent_id)
            })
            .map(|e| e.id)
            .collect();
        self.delete_ids(ids).await
    }

    async fn export_agent(&self, agent_alias: &str) -> anyhow::Result<Vec<MemoryEntry>> {
        let mut entries = self.list_agent_entries(agent_alias).await?;
        entries.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
        Ok(entries)
    }

    async fn purge_agent(&self, agent_alias: &str) -> anyhow::Result<usize> {
        let entries = self.list_agent_entries(agent_alias).await?;
        let ids: Vec<String> = entries.into_iter().map(|e| e.id).collect();
        self.delete_ids(ids).await
    }

    async fn rename_agent(&self, from: &str, to: &str) -> anyhow::Result<usize> {
        if from == to {
            return Ok(0);
        }
        let store = self.store.clone();
        let page_size = self.list_page_size;
        let max_grow = self.max_list_grow;
        let from = from.to_string();
        let to = to.to_string();
        let from_seg = Self::agent_segment(Some(&from));
        let to_seg = Self::agent_segment(Some(&to));
        tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
            let mut store = store.lock();
            let prefix = format!("/agents/{from_seg}/");
            let rows = Self::list_prefix_complete(&store, &prefix, page_size, max_grow)?;
            let mut rewritten = 0usize;
            for mut row in rows {
                let agent_meta = Self::metadata_str(&row.metadata, METADATA_ZC_AGENT);
                if agent_meta.as_deref() != Some(from.as_str()) && !row.path.starts_with(&prefix) {
                    continue;
                }
                let rest = row
                    .path
                    .strip_prefix(&format!("/agents/{from_seg}"))
                    .unwrap_or("");
                row.path = format!("/agents/{to_seg}{rest}");
                if let Some(obj) = row.metadata.as_object_mut() {
                    obj.insert(
                        METADATA_ZC_AGENT.to_string(),
                        serde_json::Value::String(to.clone()),
                    );
                }
                store
                    .upsert(&row)
                    .map_err(|e| anyhow::Error::msg(format!("tachi rename upsert failed: {e}")))?;
                rewritten += 1;
            }
            Ok(rewritten)
        })
        .await?
    }

    async fn count_agent(&self, agent_alias: &str) -> anyhow::Result<usize> {
        Ok(self.list_agent_entries(agent_alias).await?.len())
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
                Self::agent_segment(Some(allowed_agent_ids[0]))
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
        let page_size = self.list_page_size;
        let max_grow = self.max_list_grow;
        tokio::task::spawn_blocking(move || -> anyhow::Result<MemoryStats> {
            let store = store.lock();
            let stats = store
                .stats(false)
                .map_err(|e| anyhow::Error::msg(format!("{e}")))?;
            let by_category: Vec<(String, u64)> = stats.by_category.into_iter().collect();
            let rows = Self::list_prefix_complete(&store, "/agents", page_size, max_grow)?;
            let mut pinned_rows = 0u64;
            let mut superseded_rows = 0u64;
            let mut bytes = 0u64;
            for row in &rows {
                if row
                    .metadata
                    .get(METADATA_PINNED)
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    pinned_rows += 1;
                }
                // memcore soft-hides superseded rows from default list; count
                // only what we can see in metadata if present.
                if row
                    .metadata
                    .get("superseded_by")
                    .and_then(|v| v.as_str())
                    .is_some()
                {
                    superseded_rows += 1;
                }
                bytes += row.text.len() as u64;
            }
            Ok(MemoryStats {
                total_rows: stats.total,
                by_category,
                superseded_rows,
                pinned_rows,
                bytes,
            })
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

    fn temp_tachi_small_page() -> (TempDir, TachiMemory) {
        let tmp = TempDir::new().unwrap();
        let mem = TachiMemory::with_embedder_and_page_size(
            "tachi",
            tmp.path(),
            Arc::new(super::super::embeddings::NoopEmbedding),
            0.7,
            0.3,
            2,
            MAX_LIST_GROW,
        )
        .unwrap();
        (tmp, mem)
    }

    fn encoded_segment_for(value: &str) -> String {
        TachiMemory::encode_path_segment(value)
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

    #[tokio::test]
    async fn user_namespace_store_recall_purge_roundtrip() {
        let (_tmp, mem) = temp_tachi();
        mem.store_with_agent(
            "pref",
            "alice user-namespace fact",
            MemoryCategory::Core,
            None,
            Some("user:alice"),
            None,
            None,
        )
        .await
        .unwrap();

        let hits = mem
            .recall_namespaced("user:alice", "*", 5, None, None, None)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].namespace, "user:alice");
        assert_eq!(hits[0].key, "pref");

        let purged = mem.purge_namespace("user:alice").await.unwrap();
        assert_eq!(purged, 1);
        let after = mem
            .recall_namespaced("user:alice", "*", 5, None, None, None)
            .await
            .unwrap();
        assert!(after.is_empty());
    }

    #[tokio::test]
    async fn key_collision_distinct_identities() {
        let (_tmp, mem) = temp_tachi();
        mem.store("a/b", "slash key content", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("a_b", "underscore key content", MemoryCategory::Core, None)
            .await
            .unwrap();

        let slash = mem.get("a/b").await.unwrap().expect("slash key");
        let under = mem.get("a_b").await.unwrap().expect("underscore key");
        assert_eq!(slash.content, "slash key content");
        assert_eq!(under.content, "underscore key content");
        assert_ne!(slash.id, under.id);
        assert_eq!(mem.count().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn export_agent_then_purge_returns_entries() {
        let (_tmp, mem) = temp_tachi();
        mem.store_with_agent(
            "k1",
            "agent alpha row one",
            MemoryCategory::Core,
            None,
            None,
            None,
            Some("alpha"),
        )
        .await
        .unwrap();
        mem.store_with_agent(
            "k2",
            "agent alpha row two",
            MemoryCategory::Daily,
            None,
            None,
            None,
            Some("alpha"),
        )
        .await
        .unwrap();

        let exported = mem.export_agent("alpha").await.unwrap();
        assert_eq!(exported.len(), 2);
        assert!(exported.iter().any(|e| e.key == "k1"));
        assert!(exported.iter().any(|e| e.key == "k2"));

        let purged = mem.purge_agent("alpha").await.unwrap();
        assert_eq!(purged, 2);
        assert!(mem.export_agent("alpha").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn purge_session_removes_only_that_session() {
        let (_tmp, mem) = temp_tachi();
        mem.store(
            "s1",
            "session one row",
            MemoryCategory::Core,
            Some("sess-a"),
        )
        .await
        .unwrap();
        mem.store(
            "s2",
            "session two row",
            MemoryCategory::Core,
            Some("sess-b"),
        )
        .await
        .unwrap();

        let purged = mem.purge_session("sess-a").await.unwrap();
        assert_eq!(purged, 1);
        assert!(mem.get("s1").await.unwrap().is_none());
        assert!(mem.get("s2").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn rename_agent_moves_entries() {
        let (_tmp, mem) = temp_tachi();
        mem.store_with_agent(
            "note",
            "owned by old alias",
            MemoryCategory::Core,
            None,
            None,
            None,
            Some("old_alias"),
        )
        .await
        .unwrap();

        let moved = mem.rename_agent("old_alias", "new_alias").await.unwrap();
        assert_eq!(moved, 1);

        assert!(
            mem.recall_for_agents(&["old_alias"], "owned", 5, None, None, None)
                .await
                .unwrap()
                .is_empty()
        );
        let hits = mem
            .recall_for_agents(&["new_alias"], "owned", 5, None, None, None)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].agent_id.as_deref(), Some("new_alias"));
        assert_eq!(hits[0].key, "note");
    }

    #[tokio::test]
    async fn get_and_forget_survive_small_list_page_cap() {
        let (_tmp, mem) = temp_tachi_small_page();
        // With page_size=2, a naive single list_by_path call would truncate.
        // Growing scan + metadata key lookup must still find every row.
        for i in 0..5 {
            mem.store(
                &format!("key_{i}"),
                &format!("content number {i} unique"),
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();
        }
        assert_eq!(mem.count().await.unwrap(), 5);

        let hit = mem.get("key_4").await.unwrap().expect("key_4 visible");
        assert!(hit.content.contains("number 4"));

        assert!(mem.forget("key_0").await.unwrap());
        assert!(mem.get("key_0").await.unwrap().is_none());
        assert_eq!(mem.count().await.unwrap(), 4);
        assert!(mem.get("key_3").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn sanitize_collision_slash_vs_colon_remain_distinct() {
        let (_tmp, mem) = temp_tachi();
        // Both sanitize to `a_b` under the old non-injective scheme.
        mem.store("a/b", "slash identity", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("a:b", "colon identity", MemoryCategory::Core, None)
            .await
            .unwrap();

        let slash = mem.get("a/b").await.unwrap().expect("a/b");
        let colon = mem.get("a:b").await.unwrap().expect("a:b");
        assert_eq!(slash.content, "slash identity");
        assert_eq!(colon.content, "colon identity");
        assert_ne!(slash.id, colon.id);
        assert_ne!(
            encoded_segment_for("a/b"),
            encoded_segment_for("a:b"),
            "injective encoding must distinguish sanitize-colliding keys"
        );
        assert_eq!(mem.count().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn encoded_form_literal_collision_remains_distinct() {
        let (_tmp, mem) = temp_tachi();
        mem.store("a/b", "original slash key", MemoryCategory::Core, None)
            .await
            .unwrap();
        // Literal key equal to the encoded segment of `a/b` — must not collide.
        let encoded_of_slash = encoded_segment_for("a/b");
        mem.store(
            &encoded_of_slash,
            "literal encoded-looking key",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();

        let original = mem.get("a/b").await.unwrap().expect("a/b");
        let literal = mem
            .get(&encoded_of_slash)
            .await
            .unwrap()
            .expect("encoded-form literal");
        assert_eq!(original.content, "original slash key");
        assert_eq!(literal.content, "literal encoded-looking key");
        assert_ne!(original.id, literal.id);
        assert_eq!(mem.count().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn upsert_identity_mirrors_sqlite_agent_and_key() {
        // Sqlite: ON CONFLICT(agent_id, key) — namespace/category update in place
        // (`sqlite.rs` store_row_with_metadata). Same agent+key collapses;
        // different agents stay distinct. The pre-fix fallback matched key
        // alone under the agent prefix without verifying agent metadata and
        // could grab the wrong row when paths diverged; matching (agent, key)
        // keeps identity aligned with sqlite.
        let (_tmp, mem) = temp_tachi();

        mem.store_with_agent(
            "shared",
            "ns-alpha core",
            MemoryCategory::Core,
            None,
            Some("ns-alpha"),
            None,
            Some("agent1"),
        )
        .await
        .unwrap();
        // Same agent+key, different namespace+category → one row (last wins).
        mem.store_with_agent(
            "shared",
            "ns-beta daily",
            MemoryCategory::Daily,
            None,
            Some("ns-beta"),
            None,
            Some("agent1"),
        )
        .await
        .unwrap();

        let agent1_rows = mem.export_agent("agent1").await.unwrap();
        assert_eq!(
            agent1_rows.len(),
            1,
            "sqlite identity (agent_id, key) collapses namespace/category updates"
        );
        assert_eq!(agent1_rows[0].content, "ns-beta daily");
        assert_eq!(agent1_rows[0].namespace, "ns-beta");
        assert_eq!(agent1_rows[0].category, MemoryCategory::Daily);

        // Different agent, same key → second row.
        mem.store_with_agent(
            "shared",
            "agent2 copy",
            MemoryCategory::Core,
            None,
            Some("ns-alpha"),
            None,
            Some("agent2"),
        )
        .await
        .unwrap();
        assert_eq!(mem.export_agent("agent1").await.unwrap().len(), 1);
        assert_eq!(mem.export_agent("agent2").await.unwrap().len(), 1);
        assert_eq!(mem.count().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn list_prefix_complete_errors_when_max_grow_full() {
        // Seed with default caps, then reopen with page == grow == 2 so three
        // rows force a full page at the cap.
        let tmp = TempDir::new().unwrap();
        {
            let mem = TachiMemory::new("tachi", tmp.path()).unwrap();
            for i in 0..3 {
                mem.store(
                    &format!("row_{i}"),
                    &format!("payload {i}"),
                    MemoryCategory::Core,
                    None,
                )
                .await
                .unwrap();
            }
        }
        let mem = TachiMemory::with_embedder_and_page_size(
            "tachi",
            tmp.path(),
            Arc::new(super::super::embeddings::NoopEmbedding),
            0.7,
            0.3,
            2,
            2,
        )
        .unwrap();

        // count() uses memcore stats (not the growing scan); list must fail loud.
        assert_eq!(mem.count().await.unwrap(), 3);
        let err = mem
            .list(None, None)
            .await
            .expect_err("full page at max_list_grow must error");
        assert!(
            err.to_string().contains("truncated"),
            "expected truncation error, got: {err}"
        );

        // Storing a NEW key must also fail loud: the (agent, key) fallback scan
        // is incomplete, and treating that as "no existing row" would fork
        // identity with a fresh UUID.
        let err = mem
            .store("row_new", "payload new", MemoryCategory::Core, None)
            .await
            .expect_err("upsert fallback over a truncated scan must error");
        assert!(
            err.to_string().contains("truncated"),
            "expected truncation error, got: {err}"
        );

        // Updating an EXISTING key still works at the cap: the exact-path hit
        // short-circuits before the growing scan.
        mem.store("row_0", "payload updated", MemoryCategory::Core, None)
            .await
            .expect("exact-path update must not require the fallback scan");
    }
}

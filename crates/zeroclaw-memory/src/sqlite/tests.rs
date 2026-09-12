#[cfg(test)]
use super::*;
use tempfile::TempDir;

fn temp_sqlite() -> (TempDir, SqliteMemory) {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::new("test", tmp.path()).unwrap();
    (tmp, mem)
}

#[tokio::test]
async fn sqlite_name() {
    let (_tmp, mem) = temp_sqlite();
    assert_eq!(mem.name(), "sqlite");
}

#[tokio::test]
async fn sqlite_health() {
    let (_tmp, mem) = temp_sqlite();
    assert!(mem.health_check().await);
}

#[tokio::test]
async fn sqlite_store_and_get() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("user_lang", "Prefers Rust", MemoryCategory::Core, None)
        .await
        .unwrap();

    let entry = mem.get("user_lang").await.unwrap();
    assert!(entry.is_some());
    let entry = entry.unwrap();
    assert_eq!(entry.key, "user_lang");
    assert_eq!(entry.content, "Prefers Rust");
    assert_eq!(entry.category, MemoryCategory::Core);
}

#[tokio::test]
async fn sqlite_store_upsert() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("pref", "likes Rust", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.store("pref", "loves Rust", MemoryCategory::Core, None)
        .await
        .unwrap();

    let entry = mem.get("pref").await.unwrap().unwrap();
    assert_eq!(entry.content, "loves Rust");
    assert_eq!(mem.count().await.unwrap(), 1);
}

#[tokio::test]
async fn sqlite_recall_keyword() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("a", "Rust is fast and safe", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.store("b", "Python is interpreted", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.store(
        "c",
        "Rust has zero-cost abstractions",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();

    let results = mem.recall("Rust", 10, None, None, None).await.unwrap();
    assert_eq!(results.len(), 2);
    assert!(
        results
            .iter()
            .all(|r| r.content.to_lowercase().contains("rust"))
    );
}

#[tokio::test]
async fn sqlite_recall_for_agents_does_not_lose_allowed_rows_behind_disallowed_matches() {
    let (_tmp, mem) = temp_sqlite();
    let alpha = mem.ensure_agent_uuid("alpha").await.unwrap();
    let rogue = mem.ensure_agent_uuid("rogue").await.unwrap();

    for idx in 0..12 {
        mem.store_with_agent(
            &format!("rogue-{idx}"),
            "needle disallowed row",
            MemoryCategory::Core,
            None,
            None,
            None,
            Some(&rogue),
        )
        .await
        .unwrap();
    }
    mem.store_with_agent(
        "alpha-allowed",
        "needle allowed row",
        MemoryCategory::Core,
        None,
        None,
        None,
        Some(&alpha),
    )
    .await
    .unwrap();

    let results = mem
        .recall_for_agents(&[alpha.as_str()], "needle", 1, None, None, None)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].key, "alpha-allowed");
}

#[tokio::test]
async fn sqlite_purge_agent_deletes_only_that_agents_rows() {
    let (_tmp, mem) = temp_sqlite();
    let alpha = mem.ensure_agent_uuid("alpha").await.unwrap();
    let rogue = mem.ensure_agent_uuid("rogue").await.unwrap();

    for idx in 0..3 {
        mem.store_with_agent(
            &format!("alpha-{idx}"),
            "alpha row",
            MemoryCategory::Core,
            None,
            None,
            None,
            Some(&alpha),
        )
        .await
        .unwrap();
    }
    for idx in 0..2 {
        mem.store_with_agent(
            &format!("rogue-{idx}"),
            "rogue row",
            MemoryCategory::Core,
            None,
            None,
            None,
            Some(&rogue),
        )
        .await
        .unwrap();
    }
    assert_eq!(mem.count().await.unwrap(), 5);

    // Purge by ALIAS (not UUID). The regression: purge_agent bound the
    // alias straight into the agent_id column, matched zero rows, and
    // returned Ok(0) — so deleting an agent silently kept its memories.
    // The fix resolves alias → id and must delete exactly alpha's rows.
    let purged = mem.purge_agent("alpha").await.unwrap();
    assert_eq!(purged, 3, "purge_agent must delete exactly alpha's rows");
    assert_eq!(mem.count().await.unwrap(), 2, "rogue's rows must survive");

    // Unknown alias → NULL id subselect → deletes nothing, returns 0.
    let purged_ghost = mem.purge_agent("ghost").await.unwrap();
    assert_eq!(purged_ghost, 0);
    assert_eq!(mem.count().await.unwrap(), 2);
}

#[tokio::test]
async fn sqlite_rename_agent_repoints_rows_under_new_alias() {
    let (_tmp, mem) = temp_sqlite();
    let alpha = mem.ensure_agent_uuid("alpha").await.unwrap();
    for idx in 0..3 {
        mem.store_with_agent(
            &format!("alpha-{idx}"),
            "alpha row",
            MemoryCategory::Core,
            None,
            None,
            None,
            Some(&alpha),
        )
        .await
        .unwrap();
    }

    // Rename alpha → beta: memory rows ride the UUID, so this updates exactly
    // one `agents` row and the rows now resolve under the new alias.
    let renamed = mem.rename_agent("alpha", "beta").await.unwrap();
    assert_eq!(renamed, 1, "exactly one agents row re-aliased");

    // The rows now resolve under `beta`, and `alpha` resolves to nothing.
    assert_eq!(mem.export_agent("beta").await.unwrap().len(), 3);
    assert_eq!(mem.export_agent("alpha").await.unwrap().len(), 0);
    assert_eq!(mem.count().await.unwrap(), 3, "no rows lost on rename");

    // Unknown source → nothing updated.
    assert_eq!(mem.rename_agent("ghost", "phantom").await.unwrap(), 0);
}

#[tokio::test]
async fn sqlite_rename_agent_reclaims_orphan_and_refuses_live_collision() {
    let (_tmp, mem) = temp_sqlite();
    let alpha = mem.ensure_agent_uuid("alpha").await.unwrap();
    mem.store_with_agent(
        "a-0",
        "alpha row",
        MemoryCategory::Core,
        None,
        None,
        None,
        Some(&alpha),
    )
    .await
    .unwrap();

    // Simulate a prior delete of `beta`: its memories were purged but the
    // agents row survives (delete never removes it) — an orphan in the
    // UNIQUE alias slot. A bare UPDATE alpha→beta would hit the constraint.
    let _beta = mem.ensure_agent_uuid("beta").await.unwrap();
    assert_eq!(mem.purge_agent("beta").await.unwrap(), 0); // no memories anyway
    // Rename succeeds: the orphan `beta` row is dropped, alpha→beta proceeds.
    assert_eq!(mem.rename_agent("alpha", "beta").await.unwrap(), 1);
    assert_eq!(mem.export_agent("beta").await.unwrap().len(), 1);
    assert_eq!(mem.export_agent("alpha").await.unwrap().len(), 0);

    // Now `beta` has a live memory. Renaming another agent ONTO it must
    // refuse (we won't silently merge two agents' memories).
    let gamma = mem.ensure_agent_uuid("gamma").await.unwrap();
    mem.store_with_agent(
        "g-0",
        "gamma row",
        MemoryCategory::Core,
        None,
        None,
        None,
        Some(&gamma),
    )
    .await
    .unwrap();
    let err = mem.rename_agent("gamma", "beta").await.unwrap_err();
    assert!(
        err.to_string().contains("refusing to merge"),
        "expected merge-refusal, got: {err}"
    );
    // Nothing changed: both still resolve under their own aliases.
    assert_eq!(mem.export_agent("beta").await.unwrap().len(), 1);
    assert_eq!(mem.export_agent("gamma").await.unwrap().len(), 1);
}

#[tokio::test]
async fn sqlite_export_agent_returns_only_that_agents_rows() {
    let (_tmp, mem) = temp_sqlite();
    let alpha = mem.ensure_agent_uuid("alpha").await.unwrap();
    let rogue = mem.ensure_agent_uuid("rogue").await.unwrap();
    for idx in 0..3 {
        mem.store_with_agent(
            &format!("alpha-{idx}"),
            "alpha row",
            MemoryCategory::Core,
            None,
            None,
            None,
            Some(&alpha),
        )
        .await
        .unwrap();
    }
    mem.store_with_agent(
        "rogue-0",
        "rogue row",
        MemoryCategory::Core,
        None,
        None,
        None,
        Some(&rogue),
    )
    .await
    .unwrap();

    let exported = mem.export_agent("alpha").await.unwrap();
    assert_eq!(exported.len(), 3, "export only alpha's rows");
    assert!(exported.iter().all(|e| e.key.starts_with("alpha-")));
    assert_eq!(mem.export_agent("rogue").await.unwrap().len(), 1);
    assert!(mem.export_agent("ghost").await.unwrap().is_empty());
    // export does NOT delete.
    assert_eq!(mem.count().await.unwrap(), 4);
}

#[tokio::test]
async fn sqlite_recall_multi_keyword() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("a", "Rust is fast", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.store("b", "Rust is safe and fast", MemoryCategory::Core, None)
        .await
        .unwrap();

    let results = mem.recall("fast safe", 10, None, None, None).await.unwrap();
    assert!(!results.is_empty());
    // Entry with both keywords should score higher
    assert!(results[0].content.contains("safe") && results[0].content.contains("fast"));
}

#[tokio::test]
async fn sqlite_recall_no_match() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("a", "Rust rocks", MemoryCategory::Core, None)
        .await
        .unwrap();
    let results = mem
        .recall("javascript", 10, None, None, None)
        .await
        .unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn sqlite_forget() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("temp", "temporary data", MemoryCategory::Conversation, None)
        .await
        .unwrap();
    assert_eq!(mem.count().await.unwrap(), 1);

    let removed = mem.forget("temp").await.unwrap();
    assert!(removed);
    assert_eq!(mem.count().await.unwrap(), 0);
}

#[tokio::test]
async fn sqlite_forget_nonexistent() {
    let (_tmp, mem) = temp_sqlite();
    let removed = mem.forget("nope").await.unwrap();
    assert!(!removed);
}

#[tokio::test]
async fn sqlite_list_all() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("a", "one", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.store("b", "two", MemoryCategory::Daily, None)
        .await
        .unwrap();
    mem.store("c", "three", MemoryCategory::Conversation, None)
        .await
        .unwrap();

    let all = mem.list(None, None).await.unwrap();
    assert_eq!(all.len(), 3);
}

#[tokio::test]
async fn sqlite_list_by_category() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("a", "core1", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.store("b", "core2", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.store("c", "daily1", MemoryCategory::Daily, None)
        .await
        .unwrap();

    let core = mem.list(Some(&MemoryCategory::Core), None).await.unwrap();
    assert_eq!(core.len(), 2);

    let daily = mem.list(Some(&MemoryCategory::Daily), None).await.unwrap();
    assert_eq!(daily.len(), 1);
}

#[tokio::test]
async fn sqlite_count_empty() {
    let (_tmp, mem) = temp_sqlite();
    assert_eq!(mem.count().await.unwrap(), 0);
}

#[tokio::test]
async fn sqlite_get_nonexistent() {
    let (_tmp, mem) = temp_sqlite();
    assert!(mem.get("nope").await.unwrap().is_none());
}

#[tokio::test]
async fn sqlite_db_persists() {
    let tmp = TempDir::new().unwrap();

    {
        let mem = SqliteMemory::new("test", tmp.path()).unwrap();
        mem.store("persist", "I survive restarts", MemoryCategory::Core, None)
            .await
            .unwrap();
    }

    // Reopen
    let mem2 = SqliteMemory::new("test", tmp.path()).unwrap();
    let entry = mem2.get("persist").await.unwrap();
    assert!(entry.is_some());
    assert_eq!(entry.unwrap().content, "I survive restarts");
}

#[tokio::test]
async fn sqlite_category_roundtrip() {
    let (_tmp, mem) = temp_sqlite();
    let categories = [
        MemoryCategory::Core,
        MemoryCategory::Daily,
        MemoryCategory::Conversation,
        MemoryCategory::Custom("project".into()),
    ];

    for (i, cat) in categories.iter().enumerate() {
        mem.store(&format!("k{i}"), &format!("v{i}"), cat.clone(), None)
            .await
            .unwrap();
    }

    for (i, cat) in categories.iter().enumerate() {
        let entry = mem.get(&format!("k{i}")).await.unwrap().unwrap();
        assert_eq!(&entry.category, cat);
    }
}

// ── FTS5 search tests ────────────────────────────────────────

#[tokio::test]
async fn fts5_bm25_ranking() {
    let (_tmp, mem) = temp_sqlite();
    mem.store(
        "a",
        "Rust is a systems programming language",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();
    mem.store(
        "b",
        "Python is great for scripting",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();
    mem.store(
        "c",
        "Rust and Rust and Rust everywhere",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();

    let results = mem.recall("Rust", 10, None, None, None).await.unwrap();
    assert!(results.len() >= 2);
    // All results should contain "Rust"
    for r in &results {
        assert!(
            r.content.to_lowercase().contains("rust"),
            "Expected 'rust' in: {}",
            r.content
        );
    }
}

#[tokio::test]
async fn fts5_multi_word_query() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("a", "The quick brown fox jumps", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.store("b", "A lazy dog sleeps", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.store("c", "The quick dog runs fast", MemoryCategory::Core, None)
        .await
        .unwrap();

    let results = mem.recall("quick dog", 10, None, None, None).await.unwrap();
    assert!(!results.is_empty());
    // "The quick dog runs fast" matches both terms
    assert!(results[0].content.contains("quick"));
}

#[tokio::test]
async fn recall_empty_query_returns_recent_entries() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("a", "data", MemoryCategory::Core, None)
        .await
        .unwrap();
    // Empty query = time-only mode: returns recent entries
    let results = mem.recall("", 10, None, None, None).await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].key, "a");
}

#[tokio::test]
async fn recall_whitespace_query_returns_recent_entries() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("a", "data", MemoryCategory::Core, None)
        .await
        .unwrap();
    // Whitespace-only query = time-only mode: returns recent entries
    let results = mem.recall("   ", 10, None, None, None).await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].key, "a");
}

#[tokio::test]
async fn recall_star_query_returns_recent_entries() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("a", "first memory", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.store("b", "second memory", MemoryCategory::Core, None)
        .await
        .unwrap();

    let results = mem.recall("*", 10, None, None, None).await.unwrap();
    assert_eq!(results.len(), 2);
    assert!(results.iter().any(|entry| entry.key == "a"));
    assert!(results.iter().any(|entry| entry.key == "b"));
}

// ── Embedding cache tests ────────────────────────────────────

#[test]
fn content_hash_deterministic() {
    let h1 = SqliteMemory::content_hash("hello world");
    let h2 = SqliteMemory::content_hash("hello world");
    assert_eq!(h1, h2);
}

#[test]
fn content_hash_different_inputs() {
    let h1 = SqliteMemory::content_hash("hello");
    let h2 = SqliteMemory::content_hash("world");
    assert_ne!(h1, h2);
}

// ── Schema tests ─────────────────────────────────────────────

#[tokio::test]
async fn schema_has_fts5_table() {
    let (_tmp, mem) = temp_sqlite();
    let conn = mem.conn.lock();
    // FTS5 table should exist
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='memories_fts'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn schema_has_embedding_cache() {
    let (_tmp, mem) = temp_sqlite();
    let conn = mem.conn.lock();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='embedding_cache'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn schema_memories_has_embedding_column() {
    let (_tmp, mem) = temp_sqlite();
    let conn = mem.conn.lock();
    // Check that embedding column exists by querying it
    let result = conn.execute_batch("SELECT embedding FROM memories LIMIT 0");
    assert!(result.is_ok());
}

// ── FTS5 sync trigger tests ──────────────────────────────────

#[tokio::test]
async fn fts5_syncs_on_insert() {
    let (_tmp, mem) = temp_sqlite();
    mem.store(
        "test_key",
        "unique_searchterm_xyz",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();

    let conn = mem.conn.lock();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH '\"unique_searchterm_xyz\"'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn fts5_syncs_on_delete() {
    let (_tmp, mem) = temp_sqlite();
    mem.store(
        "del_key",
        "deletable_content_abc",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();
    mem.forget("del_key").await.unwrap();

    let conn = mem.conn.lock();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH '\"deletable_content_abc\"'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn fts5_syncs_on_update() {
    let (_tmp, mem) = temp_sqlite();
    mem.store(
        "upd_key",
        "original_content_111",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();
    mem.store("upd_key", "updated_content_222", MemoryCategory::Core, None)
        .await
        .unwrap();

    let conn = mem.conn.lock();
    // Old content should not be findable
    let old: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH '\"original_content_111\"'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old, 0);

    // New content should be findable
    let new: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH '\"updated_content_222\"'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(new, 1);
}

// ── Open timeout tests ────────────────────────────────────────

#[test]
fn open_with_timeout_succeeds_when_fast() {
    let tmp = TempDir::new().unwrap();
    let embedder = Arc::new(super::super::embeddings::NoopEmbedding);
    let mem = SqliteMemory::with_embedder(
        "test",
        tmp.path(),
        embedder,
        0.7,
        0.3,
        1000,
        Some(5),
        SearchMode::default(),
    );
    assert!(
        mem.is_ok(),
        "open with 5s timeout should succeed on fast path"
    );
    assert_eq!(mem.unwrap().name(), "sqlite");
}

#[tokio::test]
async fn open_with_timeout_store_recall_unchanged() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::with_embedder(
        "test",
        tmp.path(),
        Arc::new(super::super::embeddings::NoopEmbedding),
        0.7,
        0.3,
        1000,
        Some(2),
        SearchMode::default(),
    )
    .unwrap();
    mem.store(
        "timeout_key",
        "value with timeout",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();
    let entry = mem.get("timeout_key").await.unwrap().unwrap();
    assert_eq!(entry.content, "value with timeout");
}

// ── Graceful degrade on embedding failure ────────────────────

/// Embedder that advertises a real dimension but always fails to embed,
/// simulating a provider 404/401/outage (e.g. a wrong or revoked embedding
/// key — exactly the live failure that silently dropped 6 days of writes).
struct FailingEmbedding;

#[async_trait::async_trait]
impl super::super::embeddings::EmbeddingProvider for FailingEmbedding {
    fn name(&self) -> &str {
        "failing"
    }
    fn dimensions(&self) -> usize {
        1536
    }
    async fn embed(&self, _texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        anyhow::bail!("Embedding API error 404 Not Found — \"Requested entity was not found.\"")
    }
}

#[tokio::test]
async fn store_degrades_gracefully_when_embedding_fails() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::with_embedder(
        "test",
        tmp.path(),
        Arc::new(FailingEmbedding),
        0.7,
        0.3,
        1000,
        None,
        SearchMode::default(),
    )
    .unwrap();

    // A failing embedder must NOT cost us the write. The row has to persist
    // with a NULL vector rather than the whole store aborting — that abort
    // was the data-loss bug. `reindex` can backfill the vector later.
    mem.store(
        "survives",
        "this content must be retained",
        MemoryCategory::Core,
        None,
    )
    .await
    .expect("store must succeed even when the embedder fails");

    assert_eq!(mem.count().await.unwrap(), 1, "row must be persisted");
    let entry = mem.get("survives").await.unwrap().unwrap();
    assert_eq!(entry.content, "this content must be retained");
}

// ── Embedder hot-swap────────────────────────────────

/// A working embedder double that returns a fixed-length vector (each
/// element = `fill`, so the source embedder is identifiable) and counts its
/// embed calls — makes a swap observable on the real read path, network-free.
struct StubEmbedding {
    dims: usize,
    fill: f32,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl StubEmbedding {
    fn new(dims: usize, fill: f32) -> Self {
        Self {
            dims,
            fill,
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

#[async_trait::async_trait]
impl super::super::embeddings::EmbeddingProvider for StubEmbedding {
    fn name(&self) -> &str {
        "stub"
    }
    fn dimensions(&self) -> usize {
        self.dims
    }
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.calls
            .fetch_add(texts.len(), std::sync::atomic::Ordering::SeqCst);
        Ok(texts.iter().map(|_| vec![self.fill; self.dims]).collect())
    }
}

#[tokio::test]
async fn refresh_embedder_takes_effect_on_live_handle() {
    let (_tmp, mem) = temp_sqlite(); // constructed with NoopEmbedding (dims 0)

    assert!(
        mem.get_or_compute_embedding("hello")
            .await
            .unwrap()
            .is_none(),
        "Noop embedder must short-circuit to no vector"
    );

    mem.swap_embedder(Arc::new(StubEmbedding::new(4, 0.1)));

    let embedding = mem
        .get_or_compute_embedding("hello")
        .await
        .unwrap()
        .expect("swapped-in embedder must now produce a vector");
    assert_eq!(embedding.len(), 4, "vector must come from the new embedder");
}

#[tokio::test]
async fn swap_embedder_invalidates_stale_embedding_cache() {
    let tmp = TempDir::new().unwrap();
    let first = Arc::new(StubEmbedding::new(4, 0.1));
    let first_calls = Arc::clone(&first.calls);
    let mem = SqliteMemory::with_embedder(
        "test",
        tmp.path(),
        first,
        0.7,
        0.3,
        1000,
        None,
        SearchMode::default(),
    )
    .unwrap();

    // Prime the cache with the first provider's vector.
    let v1 = mem.get_or_compute_embedding("same text").await.unwrap();
    assert_eq!(v1.unwrap(), vec![0.1_f32; 4]);
    // Second call for identical content is served from cache (no new embed).
    let _ = mem.get_or_compute_embedding("same text").await.unwrap();
    assert_eq!(
        first_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "identical content must hit the cache, not re-embed"
    );

    // Swap to a different provider (distinct fill so its output is unique).
    let second = Arc::new(StubEmbedding::new(4, 0.9));
    let second_calls = Arc::clone(&second.calls);
    mem.swap_embedder(second);

    // Same content again: must re-embed through the NEW provider, not return
    // the stale cached 0.1 vector.
    let v2 = mem.get_or_compute_embedding("same text").await.unwrap();
    assert_eq!(
        v2.unwrap(),
        vec![0.9_f32; 4],
        "post-swap embed must use the new provider, not the stale cache"
    );
    assert_eq!(
        second_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "cache must have been invalidated so the new provider is called"
    );
}

#[test]
fn refresh_embedder_rebuilds_from_resolved_settings() {
    let (_tmp, mem) = temp_sqlite(); // NoopEmbedding, dims 0
    assert_eq!(mem.embedder_dimensions(), 0);

    Memory::refresh_embedder(
        &mem,
        "openai",
        Some("sk-test"),
        "text-embedding-3-small",
        1536,
    );

    assert_eq!(
        mem.embedder_dimensions(),
        1536,
        "refresh_embedder must install the resolved provider's embedder"
    );
}

// --- Durable-global recall across sessions (vector scope) ---

/// Marker token routed to its own embedding axis by [`KeyedEmbedding`].
const KEYED_MARKER: &str = "orbital";

/// Deterministic content-keyed embedder: texts containing
/// [`KEYED_MARKER`] map to one axis, everything else to an orthogonal
/// axis, so vector-stage relevance is controllable without a network.
struct KeyedEmbedding;

#[async_trait::async_trait]
impl super::super::embeddings::EmbeddingProvider for KeyedEmbedding {
    fn name(&self) -> &str {
        "keyed"
    }
    fn dimensions(&self) -> usize {
        4
    }
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|text| {
                if text.contains(KEYED_MARKER) {
                    vec![1.0, 0.0, 0.0, 0.0]
                } else {
                    vec![0.0, 1.0, 0.0, 0.0]
                }
            })
            .collect())
    }
}

fn temp_sqlite_keyed() -> (TempDir, SqliteMemory) {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::with_embedder(
        "test",
        tmp.path(),
        Arc::new(KeyedEmbedding),
        0.7,
        0.3,
        1000,
        None,
        SearchMode::default(),
    )
    .unwrap();
    (tmp, mem)
}

/// Deterministic test embedder for a keyword-only result alongside an
/// unrelated weak vector-only result.
struct MissingModalityEmbedding;

#[async_trait::async_trait]
impl super::super::embeddings::EmbeddingProvider for MissingModalityEmbedding {
    fn name(&self) -> &str {
        "missing-modality"
    }
    fn dimensions(&self) -> usize {
        2
    }
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|text| {
                if *text == "needle query-axis" {
                    vec![1.0, 0.0]
                } else if text.contains("weak-vector") {
                    vec![0.1, 0.994_987_4]
                } else {
                    vec![0.0, 1.0]
                }
            })
            .collect())
    }
}

fn temp_sqlite_missing_modality() -> (TempDir, SqliteMemory) {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::with_embedder(
        "test",
        tmp.path(),
        Arc::new(MissingModalityEmbedding),
        0.7,
        0.3,
        1000,
        None,
        SearchMode::default(),
    )
    .unwrap();
    (tmp, mem)
}

/// Repro shape from the 2026-07-09 injection-scope finding: with
/// embeddings live, a session-scoped recall (what per-turn injection
/// issues) must surface a global core fact written outside the session,
/// while other sessions' bound rows stay excluded.
#[tokio::test]
async fn session_scoped_recall_includes_durable_global_rows_when_vector_live() {
    let (_tmp, mem) = temp_sqlite_keyed();
    mem.store(
        "vault_fact",
        "the orbital vault passphrase is quokka-vellum",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();
    mem.store(
        "daily_note",
        "orbital vault rotation happens daily",
        MemoryCategory::Daily,
        None,
    )
    .await
    .unwrap();
    mem.store(
        "other_chat",
        "we discussed the orbital vault in another chat",
        MemoryCategory::Conversation,
        Some("other-session"),
    )
    .await
    .unwrap();
    mem.store(
        "bound_core",
        "orbital vault detail bound to its origin session",
        MemoryCategory::Core,
        Some("other-session"),
    )
    .await
    .unwrap();
    mem.store(
        "custom_global",
        "orbital vault note in a custom bucket",
        MemoryCategory::Custom("notes".into()),
        None,
    )
    .await
    .unwrap();
    mem.store(
        "this_chat",
        "current chat about the orbital vault",
        MemoryCategory::Conversation,
        Some("sess-1"),
    )
    .await
    .unwrap();

    let hits = mem
        .recall("orbital vault", 10, Some("sess-1"), None, None)
        .await
        .unwrap();
    let keys: Vec<&str> = hits.iter().map(|e| e.key.as_str()).collect();
    assert!(
        keys.contains(&"vault_fact"),
        "global core row must reach session-scoped vector recall, got {keys:?}"
    );
    assert!(
        keys.contains(&"daily_note"),
        "global daily row must reach session-scoped vector recall, got {keys:?}"
    );
    assert!(
        keys.contains(&"this_chat"),
        "current-session rows must keep working, got {keys:?}"
    );
    assert!(
        !keys.contains(&"other_chat"),
        "other sessions' conversation rows must stay excluded, got {keys:?}"
    );
    assert!(
        !keys.contains(&"bound_core"),
        "session-bound core rows must stay session-scoped, got {keys:?}"
    );
    assert!(
        !keys.contains(&"custom_global"),
        "custom categories are outside the durable-global carve-out, got {keys:?}"
    );
}

/// The vector stage's SQL predicate itself: a session filter admits
/// session-NULL core/daily rows and nothing else beyond the session.
#[tokio::test]
async fn vector_search_session_filter_admits_durable_global_rows_only() {
    let (_tmp, mem) = temp_sqlite_keyed();
    for (key, category, session) in [
        ("global_core", MemoryCategory::Core, None),
        ("global_daily", MemoryCategory::Daily, None),
        ("bound_core", MemoryCategory::Core, Some("other-session")),
        ("session_row", MemoryCategory::Conversation, Some("sess-1")),
        (
            "global_custom",
            MemoryCategory::Custom("notes".into()),
            None,
        ),
    ] {
        mem.store(key, "orbital telemetry", category, session)
            .await
            .unwrap();
    }
    let mut id_to_key = std::collections::HashMap::new();
    for key in [
        "global_core",
        "global_daily",
        "bound_core",
        "session_row",
        "global_custom",
    ] {
        let entry = mem.get(key).await.unwrap().unwrap();
        id_to_key.insert(entry.id, key);
    }
    let query_embedding = mem
        .get_or_compute_embedding("orbital telemetry")
        .await
        .unwrap()
        .unwrap();

    let conn = mem.conn.lock();
    let hits =
        SqliteMemory::vector_search(&conn, &query_embedding, 10, None, Some("sess-1")).unwrap();
    let mut keys: Vec<&str> = hits
        .iter()
        .map(|(id, _)| *id_to_key.get(id).unwrap())
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["global_core", "global_daily", "session_row"],
        "session filter must admit exactly the session's rows plus session-NULL core/daily"
    );
}

/// With the stock Noop embedder (vector stage never runs), session-scoped
/// recall keeps the strict legacy filter (global core rows stay out) and
/// batch-max normalizes keyword scores onto [0, 1] so downstream relevance
/// thresholding and the injection rerank stage see one calibrated scale.
#[tokio::test]
async fn noop_embedder_session_recall_keeps_strict_filter_and_normalizes_scores() {
    let (_tmp, mem) = temp_sqlite();
    mem.store(
        "vault_fact",
        "the orbital vault passphrase is quokka-vellum",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();
    mem.store(
        "this_chat",
        "current chat about the orbital vault",
        MemoryCategory::Conversation,
        Some("sess-1"),
    )
    .await
    .unwrap();
    mem.store(
        "this_chat_2",
        "second orbital note: the vault door code rotated again in the orbital bay",
        MemoryCategory::Conversation,
        Some("sess-1"),
    )
    .await
    .unwrap();

    let hits = mem
        .recall("orbital vault", 10, Some("sess-1"), None, None)
        .await
        .unwrap();
    assert_eq!(
        hits.len(),
        2,
        "strict session filter must hold on the BM25-only path"
    );
    let mut keys: Vec<&str> = hits.iter().map(|e| e.key.as_str()).collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["this_chat", "this_chat_2"]);

    // Multi-entry normalization: BM25-only scores are batch-max normalized
    // onto [0, 1] (dividing each raw negated BM25 by the batch maximum) so
    // downstream relevance thresholding and the injection rerank stage see
    // one calibrated scale. The recall FTS batch and the probe below both
    // request limit*2 = 20, so they share the batch maximum.
    let raw = {
        let conn = mem.conn.lock();
        SqliteMemory::fts5_search(&conn, "orbital vault", 20).unwrap()
    };
    assert!(
        hits.len() > 1,
        "normalization assertion needs multiple surviving entries"
    );
    let max_raw = raw.iter().map(|(_, score)| *score).fold(0.0_f32, f32::max);
    assert!(
        max_raw > 0.0,
        "the batch maximum BM25 magnitude must be positive"
    );
    for hit in &hits {
        let (_, raw_score) = raw
            .iter()
            .find(|(id, _)| *id == hit.id)
            .expect("recalled row must come from the FTS stage");
        let got = hit.score.expect("BM25-only recall carries a score");
        let expected = f64::from(*raw_score / max_raw);
        assert!(
            (got - expected).abs() < 1e-6,
            "BM25-only scores are batch-max normalized for {}: got {got}, expected {expected}",
            hit.key
        );
        assert!(
            (0.0..=1.0).contains(&got),
            "normalized score is within [0, 1] for {}: {got}",
            hit.key
        );
    }
}

/// Threshold-scale seam: when the vector stage is live but returns
/// nothing (query vector orthogonal to every stored row), FTS-only
/// survivors must be scored on the [0, 1] axis the downstream
/// cosine-tuned relevance floor expects, not raw BM25.
#[tokio::test]
async fn fts_only_survivors_are_normalized_when_vector_stage_is_live() {
    let (_tmp, mem) = temp_sqlite_keyed();
    // No KEYED_MARKER in the stored rows: their embeddings sit on the
    // other axis, so cosine similarity with the query is 0 and the
    // vector stage yields nothing.
    mem.store(
        "kw_one",
        "vault passphrase quokka vellum",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();
    mem.store(
        "kw_two",
        "vault door maintenance log",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();

    let hits = mem
        .recall("orbital vault passphrase", 10, None, None, None)
        .await
        .unwrap();
    assert_eq!(
        hits.len(),
        2,
        "both rows must survive via the keyword stage"
    );
    let top = hits
        .iter()
        .map(|e| e.score.unwrap())
        .fold(f64::MIN, f64::max);
    assert!(
        (top - 1.0).abs() < 1e-6,
        "best FTS-only survivor must map to 1.0 on the unit axis, got {top}"
    );
    assert!(
        hits.iter().all(|e| (0.0..=1.0).contains(&e.score.unwrap())),
        "normalized keyword scores must stay on the [0, 1] axis"
    );
}

#[tokio::test]
async fn keyword_only_score_survives_a_weak_vector_only_candidate() {
    let (_tmp, mem) = temp_sqlite_missing_modality();
    mem.store(
        "exact_keyword",
        "needle exact-keyword",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();

    let without_vector = mem
        .recall("needle query-axis", 10, None, None, None)
        .await
        .unwrap();
    let baseline = without_vector
        .iter()
        .find(|entry| entry.key == "exact_keyword")
        .and_then(|entry| entry.score)
        .expect("the FTS candidate must be recalled without vector candidates");
    assert!((baseline - 1.0).abs() < 1e-6);

    mem.store(
        "weak_vector",
        "weak-vector semantic-only",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();

    let with_vector = mem
        .recall("needle query-axis", 10, None, None, None)
        .await
        .unwrap();
    let score = with_vector
        .iter()
        .find(|entry| entry.key == "exact_keyword")
        .and_then(|entry| entry.score)
        .expect("the FTS candidate must survive alongside a weak vector candidate");
    assert!(
        (score - baseline).abs() < 1e-6,
        "a missing vector modality must not reduce an FTS-only score: baseline={baseline}, with_vector={score}"
    );
    assert!(
        score >= 0.4,
        "the FTS-only candidate must remain above the default relevance floor, got {score}"
    );
}

// ── With-embedder constructor test ───────────────────────────

#[test]
fn with_embedder_noop() {
    let tmp = TempDir::new().unwrap();
    let embedder = Arc::new(super::super::embeddings::NoopEmbedding);
    let mem = SqliteMemory::with_embedder(
        "test",
        tmp.path(),
        embedder,
        0.7,
        0.3,
        1000,
        None,
        SearchMode::default(),
    );
    assert!(mem.is_ok());
    assert_eq!(mem.unwrap().name(), "sqlite");
}

// ── Reindex test ─────────────────────────────────────────────

#[tokio::test]
async fn reindex_rebuilds_fts() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("r1", "reindex test alpha", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.store("r2", "reindex test beta", MemoryCategory::Core, None)
        .await
        .unwrap();

    // Reindex should succeed (noop embedder → 0 re-embedded)
    let count = mem.reindex().await.unwrap();
    assert_eq!(count, 0);

    // FTS should still work after rebuild
    let results = mem.recall("reindex", 10, None, None, None).await.unwrap();
    assert_eq!(results.len(), 2);
}

// ── Embedding identity primitives──────────────

/// Embedder that returns a fixed vector, so store() persists real
/// (non-NULL) embeddings and populates the embedding cache.
struct FixedEmbedding(usize);

#[async_trait::async_trait]
impl super::super::embeddings::EmbeddingProvider for FixedEmbedding {
    fn name(&self) -> &str {
        "fixed"
    }
    fn dimensions(&self) -> usize {
        self.0
    }
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.5f32; self.0]).collect())
    }
}

fn identity(
    provider: &str,
    model: &str,
    dimensions: usize,
) -> super::super::embeddings::EmbeddingIdentity {
    super::super::embeddings::EmbeddingIdentity {
        provider: provider.into(),
        model: model.into(),
        dimensions,
    }
}

fn count_scalar(mem: &SqliteMemory, sql: &str) -> i64 {
    let conn = mem.connection().lock();
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

#[test]
fn embedding_identity_roundtrip() {
    let (_tmp, mem) = temp_sqlite();
    // A fresh store (and any store predating identity tracking) has none.
    assert_eq!(mem.stored_embedding_identity().unwrap(), None);

    let id = identity("openai", "text-embedding-3-small", 1536);
    mem.record_embedding_identity(&id).unwrap();
    assert_eq!(mem.stored_embedding_identity().unwrap(), Some(id));
}

#[test]
fn embedding_identity_partial_rows_read_as_absent() {
    let (_tmp, mem) = temp_sqlite();
    {
        let conn = mem.connection().lock();
        conn.execute(
            "INSERT INTO memory_meta (key, value) VALUES ('embedding_model', 'orphan')",
            [],
        )
        .unwrap();
    }
    assert_eq!(mem.stored_embedding_identity().unwrap(), None);
}

#[tokio::test]
async fn invalidate_nulls_vectors_clears_cache_and_stamps_identity() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::with_embedder(
        "test",
        tmp.path(),
        Arc::new(FixedEmbedding(4)),
        0.7,
        0.3,
        1000,
        None,
        SearchMode::default(),
    )
    .unwrap();
    mem.record_embedding_identity(&identity("openai", "old-model", 4))
        .unwrap();

    mem.store("a", "alpha content", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.store("b", "beta content", MemoryCategory::Core, None)
        .await
        .unwrap();
    assert_eq!(
        count_scalar(
            &mem,
            "SELECT COUNT(*) FROM memories WHERE embedding IS NOT NULL"
        ),
        2
    );
    assert_eq!(
        count_scalar(&mem, "SELECT COUNT(*) FROM embedding_cache"),
        2
    );

    let new_id = identity("openai", "new-model", 4);
    let invalidated = mem
        .invalidate_embeddings_for_identity_change(&new_id)
        .unwrap();

    assert_eq!(invalidated, 2);
    assert_eq!(
        count_scalar(
            &mem,
            "SELECT COUNT(*) FROM memories WHERE embedding IS NOT NULL"
        ),
        0
    );
    assert_eq!(
        count_scalar(&mem, "SELECT COUNT(*) FROM embedding_cache"),
        0
    );
    assert_eq!(mem.stored_embedding_identity().unwrap(), Some(new_id));

    // Content is retained, so the existing reindex path re-embeds losslessly.
    let reembedded = mem.reindex().await.unwrap();
    assert_eq!(reembedded, 2);
    assert_eq!(
        count_scalar(
            &mem,
            "SELECT COUNT(*) FROM memories WHERE embedding IS NOT NULL"
        ),
        2
    );
}

// ── Recall limit test ────────────────────────────────────────

#[tokio::test]
async fn recall_respects_limit() {
    let (_tmp, mem) = temp_sqlite();
    for i in 0..20 {
        mem.store(
            &format!("k{i}"),
            &format!("common keyword item {i}"),
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();
    }

    let results = mem
        .recall("common keyword", 5, None, None, None)
        .await
        .unwrap();
    assert!(results.len() <= 5);
}

// ── Score presence test ──────────────────────────────────────

#[tokio::test]
async fn recall_results_have_scores() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("s1", "scored result test", MemoryCategory::Core, None)
        .await
        .unwrap();

    let results = mem.recall("scored", 10, None, None, None).await.unwrap();
    assert!(!results.is_empty());
    for r in &results {
        assert!(r.score.is_some(), "Expected score on result: {:?}", r.key);
    }
}

// ── Edge cases: FTS5 special characters ──────────────────────

#[tokio::test]
async fn recall_with_quotes_in_query() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("q1", "He said hello world", MemoryCategory::Core, None)
        .await
        .unwrap();
    // Quotes in query should not crash FTS5
    let results = mem.recall("\"hello\"", 10, None, None, None).await.unwrap();
    // May or may not match depending on FTS5 escaping, but must not error
    assert!(results.len() <= 10);
}

#[tokio::test]
async fn recall_with_asterisk_in_query() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("a1", "wildcard test content", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.store("b1", "unrelated recent content", MemoryCategory::Core, None)
        .await
        .unwrap();
    let results = mem.recall("wild*", 10, None, None, None).await.unwrap();
    assert!(results.iter().any(|entry| entry.key == "a1"));
    assert!(results.iter().all(|entry| entry.key != "b1"));
}

#[tokio::test]
async fn recall_prefix_wildcard_like_fallback_keeps_token_prefix() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::with_embedder(
        "test",
        tmp.path(),
        Arc::new(super::super::embeddings::NoopEmbedding),
        0.7,
        0.3,
        1000,
        None,
        SearchMode::Embedding,
    )
    .unwrap();
    mem.store("a1", "fallback wildcard token", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.store("b1", "fallback unwild token", MemoryCategory::Core, None)
        .await
        .unwrap();

    let results = mem.recall("wild*", 10, None, None, None).await.unwrap();
    assert!(results.iter().any(|entry| entry.key == "a1"));
    assert!(results.iter().all(|entry| entry.key != "b1"));
}

#[tokio::test]
async fn recall_prefix_wildcard_like_fallback_overfetches_filtered_rows() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::with_embedder(
        "test",
        tmp.path(),
        Arc::new(super::super::embeddings::NoopEmbedding),
        0.7,
        0.3,
        1000,
        None,
        SearchMode::Embedding,
    )
    .unwrap();
    mem.store(
        "real",
        "fallback wildcard token",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();
    for i in 0..3 {
        mem.store(
            &format!("noise{i}"),
            "fallback unwild token",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();
    }
    {
        let conn = mem.conn.lock();
        conn.execute(
            "UPDATE memories SET updated_at = ?1 WHERE key = ?2",
            rusqlite::params!["2026-05-03T00:00:00Z", "real"],
        )
        .unwrap();
        for i in 0..3 {
            conn.execute(
                "UPDATE memories SET updated_at = ?1 WHERE key = ?2",
                rusqlite::params![format!("2026-05-03T00:00:0{}Z", i + 1), format!("noise{i}")],
            )
            .unwrap();
        }
    }

    let results = mem.recall("wild*", 1, None, None, None).await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].key, "real");
}

#[tokio::test]
async fn recall_with_parentheses_in_query() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("p1", "function call test", MemoryCategory::Core, None)
        .await
        .unwrap();
    let results = mem
        .recall("function()", 10, None, None, None)
        .await
        .unwrap();
    assert!(results.len() <= 10);
}

#[tokio::test]
async fn recall_with_sql_injection_attempt() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("safe", "normal content", MemoryCategory::Core, None)
        .await
        .unwrap();
    // Should not crash or leak data
    let results = mem
        .recall("'; DROP TABLE memories; --", 10, None, None, None)
        .await
        .unwrap();
    assert!(results.len() <= 10);
    // Table should still exist
    assert_eq!(mem.count().await.unwrap(), 1);
}

// ── Edge cases: store ────────────────────────────────────────

#[tokio::test]
async fn store_empty_content() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("empty", "", MemoryCategory::Core, None)
        .await
        .unwrap();
    let entry = mem.get("empty").await.unwrap().unwrap();
    assert_eq!(entry.content, "");
}

#[tokio::test]
async fn store_empty_key() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("", "content for empty key", MemoryCategory::Core, None)
        .await
        .unwrap();
    let entry = mem.get("").await.unwrap().unwrap();
    assert_eq!(entry.content, "content for empty key");
}

#[tokio::test]
async fn store_very_long_content() {
    let (_tmp, mem) = temp_sqlite();
    let long_content = "x".repeat(100_000);
    mem.store("long", &long_content, MemoryCategory::Core, None)
        .await
        .unwrap();
    let entry = mem.get("long").await.unwrap().unwrap();
    assert_eq!(entry.content.len(), 100_000);
}

#[tokio::test]
async fn store_unicode_and_emoji() {
    let (_tmp, mem) = temp_sqlite();
    mem.store(
        "emoji_key_🦀",
        "こんにちは 🚀 Ñoño",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();
    let entry = mem.get("emoji_key_🦀").await.unwrap().unwrap();
    assert_eq!(entry.content, "こんにちは 🚀 Ñoño");
}

#[tokio::test]
async fn store_content_with_newlines_and_tabs() {
    let (_tmp, mem) = temp_sqlite();
    let content = "line1\nline2\ttab\rcarriage\n\nnewparagraph";
    mem.store("whitespace", content, MemoryCategory::Core, None)
        .await
        .unwrap();
    let entry = mem.get("whitespace").await.unwrap().unwrap();
    assert_eq!(entry.content, content);
}

// ── Edge cases: recall ───────────────────────────────────────

#[tokio::test]
async fn recall_single_character_query() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("a", "x marks the spot", MemoryCategory::Core, None)
        .await
        .unwrap();
    // Single char may not match FTS5 but LIKE fallback should work
    let results = mem.recall("x", 10, None, None, None).await.unwrap();
    // Should not crash; may or may not find results
    assert!(results.len() <= 10);
}

#[tokio::test]
async fn recall_limit_zero() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("a", "some content", MemoryCategory::Core, None)
        .await
        .unwrap();
    let results = mem.recall("some", 0, None, None, None).await.unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn recall_limit_one() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("a", "matching content alpha", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.store("b", "matching content beta", MemoryCategory::Core, None)
        .await
        .unwrap();
    let results = mem
        .recall("matching content", 1, None, None, None)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
}

#[tokio::test]
async fn recall_matches_by_key_not_just_content() {
    let (_tmp, mem) = temp_sqlite();
    mem.store(
        "rust_preferences",
        "User likes systems programming",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();
    // "rust" appears in key but not content — LIKE fallback checks key too
    let results = mem.recall("rust", 10, None, None, None).await.unwrap();
    assert!(!results.is_empty(), "Should match by key");
}

#[tokio::test]
async fn recall_unicode_query() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("jp", "日本語のテスト", MemoryCategory::Core, None)
        .await
        .unwrap();
    let results = mem.recall("日本語", 10, None, None, None).await.unwrap();
    assert!(!results.is_empty());
}

// ── Edge cases: schema idempotency ───────────────────────────

#[tokio::test]
async fn schema_idempotent_reopen() {
    let tmp = TempDir::new().unwrap();
    {
        let mem = SqliteMemory::new("test", tmp.path()).unwrap();
        mem.store("k1", "v1", MemoryCategory::Core, None)
            .await
            .unwrap();
    }
    // Open again — init_schema runs again on existing DB
    let mem2 = SqliteMemory::new("test", tmp.path()).unwrap();
    let entry = mem2.get("k1").await.unwrap();
    assert!(entry.is_some());
    assert_eq!(entry.unwrap().content, "v1");
    // Store more data — should work fine
    mem2.store("k2", "v2", MemoryCategory::Daily, None)
        .await
        .unwrap();
    assert_eq!(mem2.count().await.unwrap(), 2);
}

#[tokio::test]
async fn schema_triple_open() {
    let tmp = TempDir::new().unwrap();
    let _m1 = SqliteMemory::new("test", tmp.path()).unwrap();
    let _m2 = SqliteMemory::new("test", tmp.path()).unwrap();
    let m3 = SqliteMemory::new("test", tmp.path()).unwrap();
    assert!(m3.health_check().await);
}

// ── Edge cases: forget + FTS5 consistency ────────────────────

#[tokio::test]
async fn forget_then_recall_no_ghost_results() {
    let (_tmp, mem) = temp_sqlite();
    mem.store(
        "ghost",
        "phantom memory content",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();
    mem.forget("ghost").await.unwrap();
    let results = mem
        .recall("phantom memory", 10, None, None, None)
        .await
        .unwrap();
    assert!(
        results.is_empty(),
        "Deleted memory should not appear in recall"
    );
}

#[tokio::test]
async fn forget_and_re_store_same_key() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("cycle", "version 1", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.forget("cycle").await.unwrap();
    mem.store("cycle", "version 2", MemoryCategory::Core, None)
        .await
        .unwrap();
    let entry = mem.get("cycle").await.unwrap().unwrap();
    assert_eq!(entry.content, "version 2");
    assert_eq!(mem.count().await.unwrap(), 1);
}

// ── Edge cases: reindex ──────────────────────────────────────

#[tokio::test]
async fn reindex_empty_db() {
    let (_tmp, mem) = temp_sqlite();
    let count = mem.reindex().await.unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn reindex_twice_is_safe() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("r1", "reindex data", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.reindex().await.unwrap();
    let count = mem.reindex().await.unwrap();
    assert_eq!(count, 0); // Noop embedder → nothing to re-embed
    // Data should still be intact
    let results = mem.recall("reindex", 10, None, None, None).await.unwrap();
    assert_eq!(results.len(), 1);
}

// ── Edge cases: content_hash ─────────────────────────────────

#[test]
fn content_hash_empty_string() {
    let h = SqliteMemory::content_hash("");
    assert!(!h.is_empty());
    assert_eq!(h.len(), 16); // 16 hex chars
}

#[test]
fn content_hash_unicode() {
    let h1 = SqliteMemory::content_hash("🦀");
    let h2 = SqliteMemory::content_hash("🦀");
    assert_eq!(h1, h2);
    let h3 = SqliteMemory::content_hash("🚀");
    assert_ne!(h1, h3);
}

#[test]
fn content_hash_long_input() {
    let long = "a".repeat(1_000_000);
    let h = SqliteMemory::content_hash(&long);
    assert_eq!(h.len(), 16);
}

// ── Edge cases: category helpers ─────────────────────────────

#[test]
fn category_roundtrip_custom_with_spaces() {
    let cat = MemoryCategory::Custom("my custom category".into());
    let s = SqliteMemory::category_to_str(&cat);
    assert_eq!(s, "my custom category");
    let back = SqliteMemory::str_to_category(&s);
    assert_eq!(back, cat);
}

#[test]
fn category_roundtrip_empty_custom() {
    let cat = MemoryCategory::Custom(String::new());
    let s = SqliteMemory::category_to_str(&cat);
    assert_eq!(s, "");
    let back = SqliteMemory::str_to_category(&s);
    assert_eq!(back, MemoryCategory::Custom(String::new()));
}

// ── Edge cases: list ─────────────────────────────────────────

#[tokio::test]
async fn list_custom_category() {
    let (_tmp, mem) = temp_sqlite();
    mem.store(
        "c1",
        "custom1",
        MemoryCategory::Custom("project".into()),
        None,
    )
    .await
    .unwrap();
    mem.store(
        "c2",
        "custom2",
        MemoryCategory::Custom("project".into()),
        None,
    )
    .await
    .unwrap();
    mem.store("c3", "other", MemoryCategory::Core, None)
        .await
        .unwrap();

    let project = mem
        .list(Some(&MemoryCategory::Custom("project".into())), None)
        .await
        .unwrap();
    assert_eq!(project.len(), 2);
}

#[tokio::test]
async fn list_empty_db() {
    let (_tmp, mem) = temp_sqlite();
    let all = mem.list(None, None).await.unwrap();
    assert!(all.is_empty());
}

// ── Bulk deletion tests ───────────────────────────────────────

#[tokio::test]
async fn sqlite_purge_namespace_deletes_only_all_matching_entries() {
    let (_tmp, mem) = temp_sqlite();

    mem.store_with_metadata("a", "data", MemoryCategory::Core, None, Some("ns1"), None)
        .await
        .unwrap();
    mem.store_with_metadata("b", "data", MemoryCategory::Core, None, Some("ns2"), None)
        .await
        .unwrap();

    let in_ns1 = |entries: &[MemoryEntry]| entries.iter().filter(|e| e.namespace == "ns1").count();

    let before = mem.list(None, None).await.unwrap();
    let deleted = mem.purge_namespace("ns1").await.unwrap();
    let after = mem.list(None, None).await.unwrap();

    assert_eq!(in_ns1(&after), 0);
    assert_eq!(after.len() - in_ns1(&after), before.len() - in_ns1(&before));
    assert_eq!(deleted, in_ns1(&before));
}

#[tokio::test]
async fn sqlite_purge_session_removes_all_matching_entries() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("a1", "data1", MemoryCategory::Core, Some("sess-a"))
        .await
        .unwrap();
    mem.store("a2", "data2", MemoryCategory::Core, Some("sess-a"))
        .await
        .unwrap();
    mem.store("b1", "data3", MemoryCategory::Core, Some("sess-b"))
        .await
        .unwrap();

    let count = mem.purge_session("sess-a").await.unwrap();
    assert_eq!(count, 2);
    assert_eq!(mem.count().await.unwrap(), 1);
}

#[tokio::test]
async fn sqlite_purge_session_preserves_other_sessions() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("a1", "data1", MemoryCategory::Core, Some("sess-a"))
        .await
        .unwrap();
    mem.store("b1", "data2", MemoryCategory::Core, Some("sess-b"))
        .await
        .unwrap();
    mem.store("c1", "data3", MemoryCategory::Core, None)
        .await
        .unwrap();

    let count = mem.purge_session("sess-a").await.unwrap();
    assert_eq!(count, 1);
    assert_eq!(mem.count().await.unwrap(), 2);

    let remaining = mem.list(None, None).await.unwrap();
    assert!(
        remaining
            .iter()
            .all(|e| e.session_id.as_deref() != Some("sess-a"))
    );
}

#[tokio::test]
async fn sqlite_purge_session_returns_count() {
    let (_tmp, mem) = temp_sqlite();
    for i in 0..3 {
        mem.store(
            &format!("k{i}"),
            "data",
            MemoryCategory::Core,
            Some("target-sess"),
        )
        .await
        .unwrap();
    }

    let count = mem.purge_session("target-sess").await.unwrap();
    assert_eq!(count, 3);
}

#[tokio::test]
async fn sqlite_purge_session_empty_session_is_noop() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("a", "data", MemoryCategory::Core, Some("sess"))
        .await
        .unwrap();

    let count = mem.purge_session("").await.unwrap();
    assert_eq!(count, 0);
    assert_eq!(mem.count().await.unwrap(), 1);
}

// ── Session isolation ─────────────────────────────────────────

#[tokio::test]
async fn store_and_recall_with_session_id() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("k1", "session A fact", MemoryCategory::Core, Some("sess-a"))
        .await
        .unwrap();
    mem.store("k2", "session B fact", MemoryCategory::Core, Some("sess-b"))
        .await
        .unwrap();
    mem.store("k3", "no session fact", MemoryCategory::Core, None)
        .await
        .unwrap();

    // Recall with session-a filter returns only session-a entry
    let results = mem
        .recall("fact", 10, Some("sess-a"), None, None)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].key, "k1");
    assert_eq!(results[0].session_id.as_deref(), Some("sess-a"));
}

#[tokio::test]
async fn recall_no_session_filter_returns_all() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("k1", "alpha fact", MemoryCategory::Core, Some("sess-a"))
        .await
        .unwrap();
    mem.store("k2", "beta fact", MemoryCategory::Core, Some("sess-b"))
        .await
        .unwrap();
    mem.store("k3", "gamma fact", MemoryCategory::Core, None)
        .await
        .unwrap();

    // Recall without session filter returns all matching entries
    let results = mem.recall("fact", 10, None, None, None).await.unwrap();
    assert_eq!(results.len(), 3);
}

#[tokio::test]
async fn cross_session_recall_isolation() {
    let (_tmp, mem) = temp_sqlite();
    mem.store(
        "secret",
        "session A secret data",
        MemoryCategory::Core,
        Some("sess-a"),
    )
    .await
    .unwrap();

    // Session B cannot see session A data
    let results = mem
        .recall("secret", 10, Some("sess-b"), None, None)
        .await
        .unwrap();
    assert!(results.is_empty());

    // Session A can see its own data
    let results = mem
        .recall("secret", 10, Some("sess-a"), None, None)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
}

#[tokio::test]
async fn list_with_session_filter() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("k1", "a1", MemoryCategory::Core, Some("sess-a"))
        .await
        .unwrap();
    mem.store("k2", "a2", MemoryCategory::Conversation, Some("sess-a"))
        .await
        .unwrap();
    mem.store("k3", "b1", MemoryCategory::Core, Some("sess-b"))
        .await
        .unwrap();
    mem.store("k4", "none1", MemoryCategory::Core, None)
        .await
        .unwrap();

    // List with session-a filter
    let results = mem.list(None, Some("sess-a")).await.unwrap();
    assert_eq!(results.len(), 2);
    assert!(
        results
            .iter()
            .all(|e| e.session_id.as_deref() == Some("sess-a"))
    );

    // List with session-a + category filter
    let results = mem
        .list(Some(&MemoryCategory::Core), Some("sess-a"))
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].key, "k1");
}

#[tokio::test]
async fn schema_migration_idempotent_on_reopen() {
    let tmp = TempDir::new().unwrap();

    // First open: creates schema + migration
    {
        let mem = SqliteMemory::new("test", tmp.path()).unwrap();
        mem.store("k1", "before reopen", MemoryCategory::Core, Some("sess-x"))
            .await
            .unwrap();
    }

    // Second open: migration runs again but is idempotent
    {
        let mem = SqliteMemory::new("test", tmp.path()).unwrap();
        let results = mem
            .recall("reopen", 10, Some("sess-x"), None, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "k1");
        assert_eq!(results[0].session_id.as_deref(), Some("sess-x"));
    }
}

#[tokio::test]
async fn count_in_scope_counts_only_active_rows() {
    let (_tmp, mem) = temp_sqlite();

    mem.store_with_options(
        "core_alpha",
        "core alpha",
        MemoryCategory::Core,
        None,
        StoreOptions {
            namespace: Some("alpha".to_string()),
            pinned: true,
            ..StoreOptions::default()
        },
    )
    .await
    .unwrap();
    mem.store_with_options(
        "daily_alpha",
        "daily alpha",
        MemoryCategory::Daily,
        None,
        StoreOptions {
            namespace: Some("alpha".to_string()),
            ..StoreOptions::default()
        },
    )
    .await
    .unwrap();
    mem.store_with_options(
        "core_beta",
        "core beta",
        MemoryCategory::Core,
        None,
        StoreOptions {
            namespace: Some("beta".to_string()),
            ..StoreOptions::default()
        },
    )
    .await
    .unwrap();

    let beta = mem
        .get("core_beta")
        .await
        .unwrap()
        .expect("core_beta should exist before supersede");
    mem.supersede(&[beta.id], "core_alpha").await.unwrap();

    assert_eq!(mem.count_in_scope(Some("alpha"), None).await.unwrap(), 2);
    assert_eq!(mem.count_in_scope(Some("beta"), None).await.unwrap(), 0);
    assert_eq!(
        mem.count_in_scope(None, Some(&MemoryCategory::Core))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        mem.count_in_scope(None, Some(&MemoryCategory::Daily))
            .await
            .unwrap(),
        1
    );
    assert_eq!(mem.count_in_scope(None, None).await.unwrap(), 2);
}

#[tokio::test]
async fn stats_reports_memory_store_telemetry() {
    let (_tmp, mem) = temp_sqlite();

    mem.store_with_options(
        "core_alpha",
        "core alpha",
        MemoryCategory::Core,
        None,
        StoreOptions {
            pinned: true,
            ..StoreOptions::default()
        },
    )
    .await
    .unwrap();
    mem.store("daily_alpha", "daily alpha", MemoryCategory::Daily, None)
        .await
        .unwrap();
    mem.store("core_beta", "core beta", MemoryCategory::Core, None)
        .await
        .unwrap();

    let beta = mem
        .get("core_beta")
        .await
        .unwrap()
        .expect("core_beta should exist before supersede");
    mem.supersede(&[beta.id], "core_alpha").await.unwrap();

    let stats = mem.stats().await.unwrap();
    assert_eq!(stats.total_rows, 3);
    assert_eq!(stats.superseded_rows, 1);
    assert_eq!(stats.pinned_rows, 1);
    assert_eq!(
        stats.bytes,
        "core alpha".len() as u64 + "daily alpha".len() as u64 + "core beta".len() as u64
    );

    let by_category: std::collections::HashMap<_, _> = stats.by_category.into_iter().collect();
    assert_eq!(by_category.get("core"), Some(&2));
    assert_eq!(by_category.get("daily"), Some(&1));
}

#[tokio::test]
async fn store_with_options_round_trips_memory_kind() {
    let (_tmp, mem) = temp_sqlite();

    mem.store_with_options(
        "decision",
        "Use staged rollout",
        MemoryCategory::Core,
        None,
        StoreOptions::default().with_kind(super::super::traits::MemoryKind::Semantic(
            super::super::traits::SemanticSubtype::Decision,
        )),
    )
    .await
    .unwrap();

    let entry = mem
        .get("decision")
        .await
        .unwrap()
        .expect("kind-tagged row should be readable");
    assert_eq!(
        entry.kind,
        Some(super::super::traits::MemoryKind::Semantic(
            super::super::traits::SemanticSubtype::Decision
        ))
    );

    let recalled = mem.recall("rollout", 5, None, None, None).await.unwrap();
    assert_eq!(
        recalled.first().and_then(|entry| entry.kind.clone()),
        Some(super::super::traits::MemoryKind::Semantic(
            super::super::traits::SemanticSubtype::Decision
        ))
    );
}

#[tokio::test]
async fn pinned_round_trips_through_get_list_recall_and_export() {
    let (_tmp, mem) = temp_sqlite();

    mem.store_with_options(
        "pinned_readback",
        "pinned readback marker",
        MemoryCategory::Core,
        Some("session-pinned"),
        StoreOptions {
            pinned: true,
            ..StoreOptions::default()
        },
    )
    .await
    .unwrap();

    let entry = mem
        .get("pinned_readback")
        .await
        .unwrap()
        .expect("pinned row should be readable");
    assert!(entry.pinned);

    let listed = mem
        .list(Some(&MemoryCategory::Core), Some("session-pinned"))
        .await
        .unwrap();
    assert!(
        listed
            .iter()
            .any(|entry| entry.key == "pinned_readback" && entry.pinned)
    );

    let recalled = mem
        .recall("readback", 10, Some("session-pinned"), None, None)
        .await
        .unwrap();
    assert!(
        recalled
            .iter()
            .any(|entry| entry.key == "pinned_readback" && entry.pinned)
    );

    let exported = mem.export(&ExportFilter::default()).await.unwrap();
    assert!(
        exported
            .iter()
            .any(|entry| entry.key == "pinned_readback" && entry.pinned)
    );
}

#[tokio::test]
async fn tenant_id_round_trips_through_get_list_recall_and_export() {
    let (_tmp, mem) = temp_sqlite();

    mem.store_with_options(
        "tenant_scoped",
        "tenant scoped marker",
        MemoryCategory::Core,
        Some("session-tenant"),
        StoreOptions::default().with_tenant_id("acme"),
    )
    .await
    .unwrap();

    let entry = mem
        .get("tenant_scoped")
        .await
        .unwrap()
        .expect("tenant-scoped row should be readable");
    assert_eq!(entry.tenant_id.as_deref(), Some("acme"));

    let listed = mem
        .list(Some(&MemoryCategory::Core), Some("session-tenant"))
        .await
        .unwrap();
    assert!(listed.iter().any(
        |entry| entry.key == "tenant_scoped" && entry.tenant_id.as_deref() == Some("acme")
    ));

    let recalled = mem
        .recall("marker", 10, Some("session-tenant"), None, None)
        .await
        .unwrap();
    assert!(recalled.iter().any(
        |entry| entry.key == "tenant_scoped" && entry.tenant_id.as_deref() == Some("acme")
    ));

    let exported = mem.export(&ExportFilter::default()).await.unwrap();
    assert!(exported.iter().any(
        |entry| entry.key == "tenant_scoped" && entry.tenant_id.as_deref() == Some("acme")
    ));
}

#[tokio::test]
async fn supersede_soft_hides_losers_but_keeps_them_reversible() {
    let (_tmp, mem) = temp_sqlite();

    mem.store_with_options(
        "old_fact",
        "the office is in Denver",
        MemoryCategory::Core,
        Some("s"),
        StoreOptions::default(),
    )
    .await
    .unwrap();
    mem.store_with_options(
        "new_fact",
        "the office moved to Austin",
        MemoryCategory::Core,
        Some("s"),
        StoreOptions::default(),
    )
    .await
    .unwrap();

    let old = mem.get("old_fact").await.unwrap().expect("old row present");
    let new = mem.get("new_fact").await.unwrap().expect("new row present");

    mem.supersede(std::slice::from_ref(&old.id), &new.id)
        .await
        .unwrap();

    // Recall hides the superseded loser but still surfaces the winner.
    let recalled = mem
        .recall("office", 10, Some("s"), None, None)
        .await
        .unwrap();
    assert!(
        recalled.iter().all(|e| e.key != "old_fact"),
        "superseded row must not surface in recall"
    );
    assert!(
        recalled.iter().any(|e| e.key == "new_fact"),
        "the superseding row still recalls"
    );

    // Soft-hide, not hard delete: the row persists with a supersede marker.
    let hidden = mem
        .get("old_fact")
        .await
        .unwrap()
        .expect("supersede is reversible, the row still exists");
    assert_eq!(
        hidden.superseded_by.as_deref(),
        Some(new.id.as_str()),
        "the loser records who superseded it"
    );
}

#[tokio::test]
async fn schema_migration_tolerates_concurrent_initialization() {
    let tmp = TempDir::new().unwrap();

    // Seed an "old" DB that is missing the newer columns, so migrations have
    // real work to do when multiple initializers race.
    let db_path = tmp.path().join("memory").join("brain.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memories (
                id          TEXT PRIMARY KEY,
                key         TEXT NOT NULL UNIQUE,
                content     TEXT NOT NULL,
                category    TEXT NOT NULL DEFAULT 'core',
                embedding   BLOB,
                created_at  TEXT NOT NULL,
                updated_at  TEXT NOT NULL
            );",
        )
        .unwrap();
    }

    let workers = 12usize;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(workers));
    let mut handles = Vec::new();
    for _ in 0..workers {
        let dir = tmp.path().to_path_buf();
        let barrier = barrier.clone();
        handles.push(tokio::task::spawn_blocking(move || {
            barrier.wait();
            SqliteMemory::new("test", &dir)
        }));
    }

    for h in handles {
        h.await.unwrap().unwrap();
    }

    // Ensure all expected columns exist after the concurrent migration.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let mut stmt = conn.prepare("PRAGMA table_info(memories)").unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut cols = std::collections::HashSet::<String>::new();
    while let Some(row) = rows.next().unwrap() {
        cols.insert(row.get::<_, String>(1).unwrap());
    }

    assert!(cols.contains("session_id"));
    assert!(cols.contains("namespace"));
    assert!(cols.contains("importance"));
    assert!(cols.contains("superseded_by"));
}

// ── §4.1 Concurrent write contention tests ──────────────

#[tokio::test]
async fn sqlite_concurrent_writes_no_data_loss() {
    let (_tmp, mem) = temp_sqlite();
    let mem = std::sync::Arc::new(mem);

    let mut handles = Vec::new();
    for i in 0..10 {
        let mem = std::sync::Arc::clone(&mem);
        handles.push(zeroclaw_spawn::spawn!(async move {
            mem.store(
                &format!("concurrent_key_{i}"),
                &format!("value_{i}"),
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    let count = mem.count().await.unwrap();
    assert_eq!(
        count, 10,
        "all 10 concurrent writes must succeed without data loss"
    );
}

#[tokio::test]
async fn sqlite_concurrent_read_write_no_panic() {
    let (_tmp, mem) = temp_sqlite();
    let mem = std::sync::Arc::new(mem);

    // Pre-populate
    mem.store("shared_key", "initial", MemoryCategory::Core, None)
        .await
        .unwrap();

    let mut handles = Vec::new();

    // Concurrent reads
    for _ in 0..5 {
        let mem = std::sync::Arc::clone(&mem);
        handles.push(zeroclaw_spawn::spawn!(async move {
            let _ = mem.get("shared_key").await.unwrap();
        }));
    }

    // Concurrent writes
    for i in 0..5 {
        let mem = std::sync::Arc::clone(&mem);
        handles.push(zeroclaw_spawn::spawn!(async move {
            mem.store(
                &format!("key_{i}"),
                &format!("val_{i}"),
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    // Should have 6 total entries (1 pre-existing + 5 new)
    assert_eq!(mem.count().await.unwrap(), 6);
}

// ── Export (GDPR Art. 20) tests ─────────────────────────

#[tokio::test]
async fn export_no_filter_returns_all_entries() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("a", "one", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.store("b", "two", MemoryCategory::Daily, None)
        .await
        .unwrap();
    mem.store("c", "three", MemoryCategory::Conversation, None)
        .await
        .unwrap();

    let filter = ExportFilter::default();
    let results = mem.export(&filter).await.unwrap();
    assert_eq!(results.len(), 3);
}

#[tokio::test]
async fn export_with_namespace_filter() {
    let (_tmp, mem) = temp_sqlite();
    mem.store_with_metadata(
        "a",
        "ns1 data",
        MemoryCategory::Core,
        None,
        Some("ns1"),
        None,
    )
    .await
    .unwrap();
    mem.store_with_metadata(
        "b",
        "ns2 data",
        MemoryCategory::Core,
        None,
        Some("ns2"),
        None,
    )
    .await
    .unwrap();

    let filter = ExportFilter {
        namespace: Some("ns1".into()),
        ..Default::default()
    };
    let results = mem.export(&filter).await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].namespace, "ns1");
}

#[tokio::test]
async fn export_with_session_id_filter() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("a", "sess-a data", MemoryCategory::Core, Some("sess-a"))
        .await
        .unwrap();
    mem.store("b", "sess-b data", MemoryCategory::Core, Some("sess-b"))
        .await
        .unwrap();

    let filter = ExportFilter {
        session_id: Some("sess-a".into()),
        ..Default::default()
    };
    let results = mem.export(&filter).await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].key, "a");
}

#[tokio::test]
async fn export_with_category_filter() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("a", "core data", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.store("b", "daily data", MemoryCategory::Daily, None)
        .await
        .unwrap();

    let filter = ExportFilter {
        category: Some(MemoryCategory::Core),
        ..Default::default()
    };
    let results = mem.export(&filter).await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].category, MemoryCategory::Core);
}

#[tokio::test]
async fn export_with_time_range() {
    let (_tmp, mem) = temp_sqlite();
    // Store entries — created_at is set to Local::now() by store
    mem.store("a", "old data", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.store("b", "new data", MemoryCategory::Core, None)
        .await
        .unwrap();

    // Export with a time range that covers everything
    let filter = ExportFilter {
        since: Some("2000-01-01T00:00:00Z".into()),
        until: Some("2099-12-31T23:59:59Z".into()),
        ..Default::default()
    };
    let results = mem.export(&filter).await.unwrap();
    assert_eq!(results.len(), 2);

    // Export with a time range in the far future (no results)
    let filter = ExportFilter {
        since: Some("2099-01-01T00:00:00Z".into()),
        ..Default::default()
    };
    let results = mem.export(&filter).await.unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn export_with_combined_filters() {
    let (_tmp, mem) = temp_sqlite();
    mem.store_with_metadata(
        "a",
        "match",
        MemoryCategory::Core,
        Some("sess-a"),
        Some("ns1"),
        None,
    )
    .await
    .unwrap();
    mem.store_with_metadata(
        "b",
        "no match ns",
        MemoryCategory::Core,
        Some("sess-a"),
        Some("ns2"),
        None,
    )
    .await
    .unwrap();
    mem.store_with_metadata(
        "c",
        "no match sess",
        MemoryCategory::Core,
        None,
        Some("ns1"),
        None,
    )
    .await
    .unwrap();

    let filter = ExportFilter {
        namespace: Some("ns1".into()),
        session_id: Some("sess-a".into()),
        category: Some(MemoryCategory::Core),
        since: Some("2000-01-01T00:00:00Z".into()),
        until: Some("2099-12-31T23:59:59Z".into()),
    };
    let results = mem.export(&filter).await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].key, "a");
}

#[tokio::test]
async fn export_empty_database_returns_empty_vec() {
    let (_tmp, mem) = temp_sqlite();
    let filter = ExportFilter::default();
    let results = mem.export(&filter).await.unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn export_ordering_is_chronological() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("first", "data1", MemoryCategory::Core, None)
        .await
        .unwrap();
    // Small delay to ensure different timestamps
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    mem.store("second", "data2", MemoryCategory::Core, None)
        .await
        .unwrap();

    let filter = ExportFilter::default();
    let results = mem.export(&filter).await.unwrap();
    assert_eq!(results.len(), 2);
    assert!(
        results[0].timestamp <= results[1].timestamp,
        "Export must be ordered by created_at ASC"
    );
}

#[tokio::test]
async fn export_preserves_field_integrity() {
    let (_tmp, mem) = temp_sqlite();
    mem.store_with_metadata(
        "roundtrip_key",
        "roundtrip content",
        MemoryCategory::Custom("custom_cat".into()),
        Some("sess-rt"),
        Some("ns-rt"),
        Some(0.9),
    )
    .await
    .unwrap();

    let filter = ExportFilter::default();
    let results = mem.export(&filter).await.unwrap();
    assert_eq!(results.len(), 1);
    let e = &results[0];
    assert_eq!(e.key, "roundtrip_key");
    assert_eq!(e.content, "roundtrip content");
    assert_eq!(e.category, MemoryCategory::Custom("custom_cat".into()));
    assert_eq!(e.session_id.as_deref(), Some("sess-rt"));
    assert_eq!(e.namespace, "ns-rt");
    assert_eq!(e.importance, Some(0.9));
}

// ── §4.2 Reindex / corruption recovery tests ────────────

#[tokio::test]
async fn sqlite_reindex_preserves_data() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("a", "Rust is fast", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.store("b", "Python is interpreted", MemoryCategory::Core, None)
        .await
        .unwrap();

    mem.reindex().await.unwrap();

    let count = mem.count().await.unwrap();
    assert_eq!(count, 2, "reindex must preserve all entries");

    let entry = mem.get("a").await.unwrap();
    assert!(entry.is_some());
    assert_eq!(entry.unwrap().content, "Rust is fast");
}

#[tokio::test]
async fn sqlite_reindex_idempotent() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("x", "test data", MemoryCategory::Core, None)
        .await
        .unwrap();

    // Multiple reindex calls should be safe
    mem.reindex().await.unwrap();
    mem.reindex().await.unwrap();
    mem.reindex().await.unwrap();

    assert_eq!(mem.count().await.unwrap(), 1);
}

// ── SearchMode tests ─────────────────────────────────────────

#[tokio::test]
async fn search_mode_bm25_only() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::with_embedder(
        "test",
        tmp.path(),
        Arc::new(super::super::embeddings::NoopEmbedding),
        0.7,
        0.3,
        1000,
        None,
        SearchMode::Bm25,
    )
    .unwrap();
    mem.store(
        "lang",
        "User prefers Rust programming",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();
    mem.store("food", "User likes pizza", MemoryCategory::Core, None)
        .await
        .unwrap();

    let results = mem.recall("Rust", 10, None, None, None).await.unwrap();
    assert!(!results.is_empty(), "BM25 mode should find keyword matches");
    assert!(
        results.iter().any(|e| e.content.contains("Rust")),
        "BM25 should match on keyword 'Rust'"
    );
}

#[tokio::test]
async fn search_mode_embedding_only() {
    let tmp = TempDir::new().unwrap();
    // NoopEmbedding returns None, so embedding-only mode will fall back to LIKE
    let mem = SqliteMemory::with_embedder(
        "test",
        tmp.path(),
        Arc::new(super::super::embeddings::NoopEmbedding),
        0.7,
        0.3,
        1000,
        None,
        SearchMode::Embedding,
    )
    .unwrap();
    mem.store(
        "lang",
        "User prefers Rust programming",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();

    // With NoopEmbedding, vector search returns empty, and FTS is skipped.
    // The recall method falls back to LIKE search.
    let results = mem.recall("Rust", 10, None, None, None).await.unwrap();
    // LIKE fallback should still find it
    assert!(
        results.iter().any(|e| e.content.contains("Rust")),
        "Embedding mode with noop should fall back to LIKE and still find results"
    );
}

#[tokio::test]
async fn search_mode_hybrid_default() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::new("test", tmp.path()).unwrap();
    // Default search mode should be Hybrid
    assert_eq!(mem.search_mode, SearchMode::Hybrid);

    mem.store(
        "lang",
        "User prefers Rust programming",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();

    let results = mem.recall("Rust", 10, None, None, None).await.unwrap();
    assert!(!results.is_empty(), "Hybrid mode should find results");
}

#[tokio::test]
async fn get_returns_alias_text_in_agent_alias_and_uuid_in_agent_id() {
    let (_tmp, mem) = temp_sqlite();
    let alpha_uuid = mem.ensure_agent_uuid("clamps").await.unwrap();
    mem.store_with_agent(
        "row1",
        "v",
        MemoryCategory::Core,
        None,
        None,
        None,
        Some(&alpha_uuid),
    )
    .await
    .unwrap();

    let entry = mem.get("row1").await.unwrap().expect("row1 must exist");
    assert_eq!(
        entry.agent_alias.as_deref(),
        Some("clamps"),
        "agent_alias must carry the human-readable alias, not the UUID"
    );
    assert_eq!(
        entry.agent_id.as_deref(),
        Some(alpha_uuid.as_str()),
        "agent_id must carry the raw UUID FK so scoping equality works"
    );
    assert_ne!(
        entry.agent_alias, entry.agent_id,
        "alias and id must differ on a SQL backend"
    );
}

#[tokio::test]
async fn list_returns_alias_text_for_every_row() {
    let (_tmp, mem) = temp_sqlite();
    let a = mem.ensure_agent_uuid("clamps").await.unwrap();
    let b = mem.ensure_agent_uuid("glados").await.unwrap();
    for (key, owner) in [("r1", &a), ("r2", &b)] {
        mem.store_with_agent(
            key,
            "v",
            MemoryCategory::Core,
            None,
            None,
            None,
            Some(owner),
        )
        .await
        .unwrap();
    }

    let mut rows = mem.list(None, None).await.unwrap();
    rows.sort_by(|x, y| x.key.cmp(&y.key));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].agent_alias.as_deref(), Some("clamps"));
    assert_eq!(rows[1].agent_alias.as_deref(), Some("glados"));
    assert!(
        rows.iter().all(|r| r.agent_id.is_some()),
        "every row should carry agent_id"
    );
}

// ── session_id migration ──────────────────────────────────────

#[tokio::test]
async fn migrates_legacy_session_ids_to_sanitized_form() {
    let tmp = TempDir::new().unwrap();
    let raw_sid = "slack_C123_1.2_user one";
    let sanitized = sanitize_session_key(raw_sid);
    assert_ne!(
        raw_sid, sanitized,
        "test only meaningful when sanitization changes the value"
    );

    {
        let mem = SqliteMemory::new("test", tmp.path()).unwrap();
        mem.store(
            "legacy_key",
            "stored before sanitize fix",
            MemoryCategory::Conversation,
            Some(raw_sid),
        )
        .await
        .unwrap();
        let pre = mem.list(None, Some(raw_sid)).await.unwrap();
        assert_eq!(pre.len(), 1, "raw session_id should match before migration");
    }

    let mem = SqliteMemory::new("test", tmp.path()).unwrap();

    let by_sanitized = mem.list(None, Some(&sanitized)).await.unwrap();
    assert_eq!(
        by_sanitized.len(),
        1,
        "row must be discoverable via sanitized session_id"
    );
    assert_eq!(by_sanitized[0].key, "legacy_key");

    let by_raw = mem.list(None, Some(raw_sid)).await.unwrap();
    assert!(
        by_raw.is_empty(),
        "raw form must no longer match after migration"
    );
}

#[tokio::test]
async fn session_id_migration_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let sanitized = sanitize_session_key("slack_C123_1.2_user");

    {
        let mem = SqliteMemory::new("test", tmp.path()).unwrap();
        mem.store("k", "v", MemoryCategory::Core, Some(&sanitized))
            .await
            .unwrap();
    }

    for _ in 0..3 {
        let mem = SqliteMemory::new("test", tmp.path()).unwrap();
        let entries = mem.list(None, Some(&sanitized)).await.unwrap();
        assert_eq!(entries.len(), 1);
    }
}

#[tokio::test]
async fn session_id_migration_leaves_null_rows_untouched() {
    let tmp = TempDir::new().unwrap();

    {
        let mem = SqliteMemory::new("test", tmp.path()).unwrap();
        mem.store("global", "no session", MemoryCategory::Core, None)
            .await
            .unwrap();
    }

    let mem = SqliteMemory::new("test", tmp.path()).unwrap();
    let entry = mem.get("global").await.unwrap().expect("row should exist");
    assert!(entry.session_id.is_none());
}

#[tokio::test]
async fn sqlite_timestamp_loading_is_rfc3339_round_trippable() {
    let (_tmp, mem) = temp_sqlite();
    mem.store(
        "ts-key-1",
        "content one",
        MemoryCategory::Core,
        Some("sess-7694"),
    )
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    mem.store(
        "ts-key-2",
        "content two",
        MemoryCategory::Core,
        Some("sess-7694"),
    )
    .await
    .unwrap();

    let entries = mem.list(None, Some("sess-7694")).await.unwrap();
    assert_eq!(entries.len(), 2);
    for entry in &entries {
        // RFC 3339 / ISO 8601 with timezone designator and millisecond
        // precision — chrono's default serialization. Anything else
        // would mean the schema or row mapper silently changed.
        let parsed = chrono::DateTime::parse_from_rfc3339(&entry.timestamp).unwrap_or_else(|err| {
            panic!(
                "entry {:?} returned non-RFC3339 timestamp {:?}: {err}",
                entry.key, entry.timestamp
            )
        });
        // Round-trip must preserve the original instant.
        assert_eq!(parsed.to_rfc3339(), entry.timestamp);
    }
}

#[tokio::test]
async fn sqlite_session_metadata_ordering_is_stable_descending() {
    let (_tmp, mem) = temp_sqlite();
    let keys = ["ord-a", "ord-b", "ord-c", "ord-d"];
    for key in keys {
        mem.store(key, "body", MemoryCategory::Core, Some("sess-order"))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // First read: capture the ordering.
    let first = mem.list(None, Some("sess-order")).await.unwrap();
    assert_eq!(first.len(), keys.len());
    let first_order: Vec<&str> = first.iter().map(|e| e.key.as_str()).collect();
    assert_eq!(
        first_order,
        vec!["ord-d", "ord-c", "ord-b", "ord-a"],
        "list() must order rows by updated_at DESC (newest first)"
    );

    // Second read with no writes in between: order must be identical.
    let second = mem.list(None, Some("sess-order")).await.unwrap();
    let second_order: Vec<&str> = second.iter().map(|e| e.key.as_str()).collect();
    assert_eq!(
        first_order, second_order,
        "ordering must be stable across reads"
    );

    // And every row must carry the session metadata we asked for.
    for entry in &first {
        assert_eq!(entry.session_id.as_deref(), Some("sess-order"));
    }
}

#[tokio::test]
async fn sqlite_session_metadata_ordering_ties_are_deterministic() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("tie-x", "x", MemoryCategory::Core, Some("sess-tie"))
        .await
        .unwrap();
    mem.store("tie-y", "y", MemoryCategory::Core, Some("sess-tie"))
        .await
        .unwrap();

    let tied_ts = "2026-06-19T00:00:00.000000000+00:00";
    {
        let conn = mem.connection().lock();
        conn.execute(
            "UPDATE memories SET created_at = ?1, updated_at = ?1 \
             WHERE key IN (?2, ?3)",
            rusqlite::params![tied_ts, "tie-x", "tie-y"],
        )
        .unwrap();
    }

    let first = mem.list(None, Some("sess-tie")).await.unwrap();
    assert_eq!(first.len(), 2);

    // Lock in that a tie really occurred. Without this, the test
    // degrades into a generic "stable order" check and the
    // function name overstates what it covers.
    assert_eq!(
        first[0].timestamp, first[1].timestamp,
        "expected both rows to share the forced updated_at"
    );
    assert_eq!(first[0].timestamp, tied_ts);

    // Capture the order once.
    let snapshot: Vec<String> = first.iter().map(|e| e.key.clone()).collect();

    // Five more reads must all agree with the snapshot. If ordering
    // were non-deterministic at a tied timestamp, this would flake.
    for _ in 0..5 {
        let again = mem.list(None, Some("sess-tie")).await.unwrap();
        let again_keys: Vec<String> = again.iter().map(|e| e.key.clone()).collect();
        assert_eq!(
            again_keys, snapshot,
            "list() must yield a deterministic order across reads"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────
// Reserved Soul namespace boundary (storage layer)
// ─────────────────────────────────────────────────────────────────────

/// Plant one Soul-shaped row exactly as the typed Soul services write
/// them (reserved key prefix, reserved namespace, soul category, agent
/// attribution) plus one ambient row for contrast.
async fn seed_soul_and_ambient(mem: &SqliteMemory, agent_id: &str) {
    mem.store_with_agent(
        "soul::agent-a::disposition",
        "soul disposition content",
        MemoryCategory::Custom("soul".to_string()),
        None,
        Some(crate::soul::SOUL_NAMESPACE),
        None,
        Some(agent_id),
    )
    .await
    .unwrap();
    mem.store(
        "ambient_pref",
        "ambient content",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();
}

fn soul_leaks(entries: &[MemoryEntry]) -> Vec<&MemoryEntry> {
    entries
        .iter()
        .filter(|e| e.namespace == crate::soul::SOUL_NAMESPACE)
        .collect()
}

#[tokio::test]
async fn ambient_surfaces_never_see_soul_rows() {
    let (_tmp, mem) = temp_sqlite();
    let agent = mem.ensure_agent_uuid("default").await.unwrap();
    seed_soul_and_ambient(&mem, &agent).await;

    // Keyword recall (FTS channel): the Soul row is the only match for
    // its own content, so any leak is visible.
    let hits = mem
        .recall("soul disposition content", 10, None, None, None)
        .await
        .unwrap();
    assert!(
        soul_leaks(&hits).is_empty(),
        "ambient keyword recall leaked Soul rows: {:?}",
        soul_leaks(&hits)
    );

    // Recent/time-only recall channel.
    let recent = mem.recall("*", 10, None, None, None).await.unwrap();
    assert!(
        soul_leaks(&recent).is_empty(),
        "ambient recent recall leaked Soul rows"
    );

    // Listing.
    let listed = mem.list(None, None).await.unwrap();
    assert!(
        soul_leaks(&listed).is_empty(),
        "ambient listing leaked Soul rows"
    );

    // Exact-key ambient get: reserved row is invisible, ambient row
    // still resolves.
    assert!(
        mem.get("soul::agent-a::disposition")
            .await
            .unwrap()
            .is_none(),
        "ambient exact-key get must not return a Soul row"
    );
    assert!(mem.get("ambient_pref").await.unwrap().is_some());
}

#[tokio::test]
async fn recall_namespaced_still_reads_soul_rows() {
    let (_tmp, mem) = temp_sqlite();
    let agent = mem.ensure_agent_uuid("default").await.unwrap();
    seed_soul_and_ambient(&mem, &agent).await;

    // The namespaced opt-in channel reads the reserved rows and nothing
    // outside the namespace.
    let rows = mem
        .recall_namespaced("soul", "soul disposition content", 10, None, None, None)
        .await
        .unwrap();
    assert!(
        rows.iter().any(|e| e.key == "soul::agent-a::disposition"),
        "namespaced recall must read the Soul row"
    );
    assert!(
        rows.iter()
            .all(|e| e.namespace == crate::soul::SOUL_NAMESPACE),
        "namespaced recall must not return rows outside the namespace"
    );
}

#[tokio::test]
async fn ambient_forget_cannot_delete_soul_rows() {
    let (_tmp, mem) = temp_sqlite();
    let agent = mem.ensure_agent_uuid("default").await.unwrap();
    seed_soul_and_ambient(&mem, &agent).await;

    // Unscoped forget reports nothing deleted for the reserved key and
    // the row survives through the typed read channel.
    let deleted = mem.forget("soul::agent-a::disposition").await.unwrap();
    assert!(!deleted, "ambient forget must not reach Soul rows");
    assert!(
        mem.get_for_agent("soul::agent-a::disposition", &agent)
            .await
            .unwrap()
            .is_some(),
        "the Soul row must survive an ambient forget"
    );

    // Ambient rows still delete normally through the same surface.
    assert!(mem.forget("ambient_pref").await.unwrap());
}

#[tokio::test]
async fn plain_stores_cannot_write_into_the_soul_key_space() {
    let (_tmp, mem) = temp_sqlite();
    let agent = mem.ensure_agent_uuid("default").await.unwrap();

    // Ambient store (default namespace) at a reserved key: refused —
    // this is the upsert-overwrite path through the (agent_id, key)
    // conflict target.
    let refused = mem
        .store(
            "soul::agent-a::disposition",
            "forged",
            MemoryCategory::Core,
            None,
        )
        .await;
    assert!(
        refused.is_err(),
        "an ambient store into the reserved key prefix must be refused"
    );

    // Reserved namespace with a non-reserved key shape: refused too.
    let mismatched = mem
        .store_with_agent(
            "plain-key",
            "x",
            MemoryCategory::Custom("soul".to_string()),
            None,
            Some(crate::soul::SOUL_NAMESPACE),
            None,
            Some(&agent),
        )
        .await;
    assert!(
        mismatched.is_err(),
        "the reserved namespace must refuse non-reserved key shapes"
    );

    // Neither refusal wrote anything.
    assert!(
        mem.get_for_agent("soul::agent-a::disposition", &agent)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        mem.get_for_agent("plain-key", &agent)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn soul_invalid_store_refuses_before_embedding() {
    let (_tmp, mem) = temp_sqlite();
    let embedder = Arc::new(StubEmbedding::new(4, 0.2));
    mem.swap_embedder(embedder.clone());
    for (key, namespace) in [
        (
            "ordinary-key".to_string(),
            Some(crate::soul::SOUL_NAMESPACE.to_string()),
        ),
        (
            format!("{}agent::disposition", crate::soul::SOUL_KEY_PREFIX),
            None,
        ),
    ] {
        mem.store_with_options(
            &key,
            "rejected payload",
            MemoryCategory::Core,
            None,
            StoreOptions {
                namespace,
                ..StoreOptions::default()
            },
        )
        .await
        .expect_err("invalid reservation must refuse");
        assert_eq!(
            embedder.calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "reservation refusal must precede provider invocation"
        );
        assert_eq!(mem.count().await.unwrap(), 0);
    }
}

#[tokio::test]
async fn soul_valid_store_persists_without_embedding() {
    let (_tmp, mem) = temp_sqlite();
    let embedder = Arc::new(StubEmbedding::new(4, 0.2));
    mem.swap_embedder(embedder.clone());
    for suffix in ["disposition", "candidate::pending"] {
        let key = format!("{}agent::{suffix}", crate::soul::SOUL_KEY_PREFIX);
        mem.store_with_options(
            &key,
            "reserved local payload",
            MemoryCategory::Core,
            None,
            StoreOptions {
                namespace: Some(crate::soul::SOUL_NAMESPACE.into()),
                ..StoreOptions::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(embedder.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let row: (String, Option<Vec<u8>>) = mem
            .conn
            .lock()
            .query_row(
                "SELECT content, embedding FROM memories WHERE key = ?1",
                params![key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, ("reserved local payload".into(), None));
    }
    mem.store(
        "ordinary",
        "ordinary embedded payload",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        embedder.calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "ordinary storage must still use its configured embedder"
    );
}

#[tokio::test]
async fn soul_reindex_excludes_reserved_rows_through_scoped_handle() {
    for ordinary_count in [0usize, 2] {
        let (_tmp, mem) = temp_sqlite();
        let mem = Arc::new(mem);
        let scoped = crate::agent_scoped::AgentScopedMemory::new(mem.clone(), "agent", []);
        assert_eq!(
            scoped.reindex().await.unwrap(),
            0,
            "empty store remains valid"
        );
        for suffix in ["disposition", "candidate::pending"] {
            mem.store_with_options(
                &format!("{}agent::{suffix}", crate::soul::SOUL_KEY_PREFIX),
                "reserved local payload",
                MemoryCategory::Core,
                None,
                StoreOptions {
                    namespace: Some(crate::soul::SOUL_NAMESPACE.into()),
                    ..StoreOptions::default()
                },
            )
            .await
            .unwrap();
        }
        for index in 0..ordinary_count {
            mem.store(
                &format!("ordinary-{index}"),
                &format!("ordinary payload {index}"),
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();
        }
        if ordinary_count > 0 {
            mem.conn
                .lock()
                .execute(
                    "UPDATE memories SET namespace = NULL WHERE key = 'ordinary-1'",
                    [],
                )
                .unwrap();
        }
        let embedder = Arc::new(StubEmbedding::new(4, 0.2));
        mem.swap_embedder(embedder.clone());
        assert_eq!(scoped.reindex().await.unwrap(), ordinary_count);
        assert_eq!(
            embedder.calls.load(std::sync::atomic::Ordering::SeqCst),
            ordinary_count,
            "only ordinary missing/default namespace content may reach the embedder"
        );
        let conn = mem.conn.lock();
        let reserved: i64 = conn.query_row(
            "SELECT COUNT(*) FROM memories WHERE namespace = ?1 AND content = 'reserved local payload' AND embedding IS NULL",
            params![crate::soul::SOUL_NAMESPACE], |r| r.get(0)).unwrap();
        assert_eq!(reserved, 2, "ambient reindex must preserve reserved rows");
        let embedded: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE embedding IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(embedded, ordinary_count);
        let indexed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH 'reserved'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            indexed, 2,
            "local FTS maintenance must still rebuild protected rows"
        );
    }
}

//! Downstream scenario validation for the tachi backend (feature `tachi`).
//!
//! # Scenario mapping
//!
//! ## RomanBath (SillyTavern-style character chat)
//! RomanBath's live chat path uses `ChatMemoryStore` with **one SQLite DB per
//! `character_name`** (`{data_dir}/chat_memory/{sanitized}_memory.db`), not
//! `agent_id` / `namespace` / `session_id` on the install-wide `Memory` trait
//! (see RomanBath `crates/zeroclaw-memory-sigil/src/chat_memory.rs:15-33` and
//! gateway `api_characters.rs:414-422`).
//!
//! When that character isolation is expressed through ZeroClaw's install-wide
//! `Memory` surface (tachi), the matching feature is **`agent_id`**:
//! `store_with_agent` / `recall_for_agents` / `export_agent` / `purge_agent`.
//! Character deletion maps to export→purge cascade on that agent alias.
//!
//! ## Hyperion (agent self-memory / "know your human")
//! Trading memory stays on hapi memory-server MCP `:6888` (AGENTS.md). Tachi
//! holds the agent's **own** memory: persona + human preferences via
//! namespaces (`user:*` vs `default`) on a single default agent, with
//! `recall_namespaced` isolation.

#[cfg(all(test, feature = "tachi"))]
mod tests {
    use crate::tachi::TachiMemory;
    use crate::traits::{Memory, MemoryCategory};
    use memcore::{MemoryEntry, MemoryStore};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn temp_tachi() -> (TempDir, TachiMemory) {
        let tmp = TempDir::new().unwrap();
        let mem = TachiMemory::with_embedder(
            "tachi",
            tmp.path(),
            Arc::new(crate::embeddings::NoopEmbedding),
            0.7,
            0.3,
        )
        .unwrap();
        (tmp, mem)
    }

    // ── Scenario 1: RomanBath character isolation via agent_id ─────────────

    /// Two characters (agent aliases) store/recall independently — no leakage.
    #[tokio::test]
    async fn romanbath_two_characters_store_recall_isolated() {
        let (_tmp, mem) = temp_tachi();
        let alice = mem.ensure_agent_uuid("char_alice").await.unwrap();
        let bob = mem.ensure_agent_uuid("char_bob").await.unwrap();

        mem.store_with_agent(
            "greeting",
            "Alice remembers the user likes tea ceremonies",
            MemoryCategory::Core,
            None,
            None,
            None,
            Some(&alice),
        )
        .await
        .unwrap();
        mem.store_with_agent(
            "greeting",
            "Bob remembers the user prefers espresso shots",
            MemoryCategory::Core,
            None,
            None,
            None,
            Some(&bob),
        )
        .await
        .unwrap();

        let alice_hits = mem
            .recall_for_agents(&[&alice], "tea", 8, None, None, None)
            .await
            .unwrap();
        assert_eq!(alice_hits.len(), 1, "{alice_hits:?}");
        assert!(alice_hits[0].content.contains("tea"));
        assert!(
            !alice_hits.iter().any(|e| e.content.contains("espresso")),
            "Alice must not see Bob's memory"
        );

        let bob_hits = mem
            .recall_for_agents(&[&bob], "espresso", 8, None, None, None)
            .await
            .unwrap();
        assert_eq!(bob_hits.len(), 1, "{bob_hits:?}");
        assert!(bob_hits[0].content.contains("espresso"));
        assert!(
            !bob_hits.iter().any(|e| e.content.contains("tea")),
            "Bob must not see Alice's memory"
        );
    }

    /// Per-character export returns only that character's rows.
    #[tokio::test]
    async fn romanbath_per_character_export() {
        let (_tmp, mem) = temp_tachi();
        mem.store_with_agent(
            "trait",
            "Alice is witty",
            MemoryCategory::Core,
            None,
            None,
            None,
            Some("char_alice"),
        )
        .await
        .unwrap();
        mem.store_with_agent(
            "trait",
            "Bob is stoic",
            MemoryCategory::Core,
            None,
            None,
            None,
            Some("char_bob"),
        )
        .await
        .unwrap();
        mem.store_with_agent(
            "quirk",
            "Alice collects stamps",
            MemoryCategory::Daily,
            None,
            None,
            None,
            Some("char_alice"),
        )
        .await
        .unwrap();

        let alice_export = mem.export_agent("char_alice").await.unwrap();
        assert_eq!(alice_export.len(), 2);
        assert!(
            alice_export
                .iter()
                .all(|e| { e.content.contains("Alice") || e.key == "trait" || e.key == "quirk" })
        );
        assert!(
            !alice_export.iter().any(|e| e.content.contains("Bob")),
            "export must not include other characters"
        );

        let bob_export = mem.export_agent("char_bob").await.unwrap();
        assert_eq!(bob_export.len(), 1);
        assert!(bob_export[0].content.contains("Bob"));
    }

    /// Character deletion = export then purge; other characters untouched.
    #[tokio::test]
    async fn romanbath_character_delete_export_purge_cascade() {
        let (_tmp, mem) = temp_tachi();
        mem.store_with_agent(
            "m1",
            "Alice memory one",
            MemoryCategory::Core,
            None,
            None,
            None,
            Some("char_alice"),
        )
        .await
        .unwrap();
        mem.store_with_agent(
            "m2",
            "Alice memory two",
            MemoryCategory::Core,
            None,
            None,
            None,
            Some("char_alice"),
        )
        .await
        .unwrap();
        mem.store_with_agent(
            "m1",
            "Bob memory one",
            MemoryCategory::Core,
            None,
            None,
            None,
            Some("char_bob"),
        )
        .await
        .unwrap();

        // Cascade: export → purge (gateway agent-deletion pattern).
        let archive = mem.export_agent("char_alice").await.unwrap();
        assert_eq!(archive.len(), 2);
        let purged = mem.purge_agent("char_alice").await.unwrap();
        assert_eq!(purged, 2);

        assert!(
            mem.export_agent("char_alice").await.unwrap().is_empty(),
            "Alice memory must be gone after purge"
        );
        let bob_left = mem.export_agent("char_bob").await.unwrap();
        assert_eq!(bob_left.len(), 1);
        assert!(bob_left[0].content.contains("Bob"));

        let bob = mem.ensure_agent_uuid("char_bob").await.unwrap();
        let bob_recall = mem
            .recall_for_agents(&[&bob], "Bob", 5, None, None, None)
            .await
            .unwrap();
        assert_eq!(bob_recall.len(), 1);
    }

    // ── Scenario 2: Hyperion agent self-memory via namespaces ──────────────

    /// Default agent: `user:kyle` preferences coexist with `default` notes;
    /// `recall_namespaced` isolates both ways.
    #[tokio::test]
    async fn hyperion_user_and_default_namespace_coexist() {
        let (_tmp, mem) = temp_tachi();
        let agent = mem.ensure_agent_uuid("default").await.unwrap();

        mem.store_with_agent(
            "pref_timezone",
            "Kyle prefers Asia/Shanghai timezone for reports",
            MemoryCategory::Core,
            None,
            Some("user:kyle"),
            Some(0.9),
            Some(&agent),
        )
        .await
        .unwrap();
        mem.store_with_agent(
            "persona_tone",
            "Agent persona: concise Rust-first operator notes",
            MemoryCategory::Core,
            None,
            Some("default"),
            Some(0.8),
            Some(&agent),
        )
        .await
        .unwrap();

        let user_hits = mem
            .recall_namespaced("user:kyle", "*", 8, None, None, None)
            .await
            .unwrap();
        assert_eq!(user_hits.len(), 1);
        assert_eq!(user_hits[0].namespace, "user:kyle");
        assert!(user_hits[0].content.contains("Asia/Shanghai"));
        assert!(
            !user_hits.iter().any(|e| e.content.contains("persona")),
            "user namespace must not leak default persona notes"
        );

        let default_hits = mem
            .recall_namespaced("default", "*", 8, None, None, None)
            .await
            .unwrap();
        assert_eq!(default_hits.len(), 1);
        assert_eq!(default_hits[0].namespace, "default");
        assert!(default_hits[0].content.contains("persona"));
        assert!(
            !default_hits
                .iter()
                .any(|e| e.content.contains("Asia/Shanghai")),
            "default namespace must not leak user preferences"
        );
    }

    /// Cross-namespace near-dup must never merge under unattended governance.
    ///
    /// Discriminating vs `2ac918587`: that tip asserted `near_dup_archived == 1`
    /// (cross-namespace fold). This test asserts `0` + both namespaces still
    /// recallable — RED against that tip, GREEN with partition-scoped merges.
    #[tokio::test]
    async fn hyperion_governance_near_dup_cross_namespace_never_merges() {
        let (_tmp, mem) = temp_tachi();
        let base = "The operator prefers concise Rust answers for equity trading notes";

        {
            let mut store = mem.store_handle().lock();
            seed_raw_scoped(
                &mut store,
                "ns-user",
                &TachiMemory::storage_path(
                    Some("default"),
                    Some("user:kyle"),
                    &MemoryCategory::Core,
                    "pref",
                ),
                base,
                0.5,
                "default",
                "user:kyle",
                "pref",
            );
            seed_raw_scoped(
                &mut store,
                "ns-default",
                &TachiMemory::storage_path(
                    Some("default"),
                    Some("default"),
                    &MemoryCategory::Core,
                    "pref",
                ),
                &format!("{base} today"),
                0.95,
                "default",
                "default",
                "pref",
            );
        }

        let report = mem.run_light_sleep_governance_report().unwrap();
        assert_eq!(
            report.near_dup_archived, 0,
            "near-dup must not archive across namespaces: {report:?}"
        );

        {
            let store = mem.store_handle().lock();
            assert!(!store.get("ns-user").unwrap().expect("user row").archived);
            assert!(
                !store
                    .get("ns-default")
                    .unwrap()
                    .expect("default row")
                    .archived
            );
        }

        let user_hits = mem
            .recall_namespaced("user:kyle", "*", 8, None, None, None)
            .await
            .unwrap();
        assert_eq!(user_hits.len(), 1);
        assert!(user_hits[0].content.contains("equity trading"));

        let default_hits = mem
            .recall_namespaced("default", "*", 8, None, None, None)
            .await
            .unwrap();
        assert_eq!(default_hits.len(), 1);
        assert!(default_hits[0].content.contains("equity trading"));
    }

    /// A metadata-less row partitions as `(None, None)` — its own bucket, never
    /// merged with an explicit `("default", "default")` row even on identical
    /// text.
    #[tokio::test]
    async fn governance_near_dup_metadata_less_row_never_merges_with_default() {
        let (_tmp, mem) = temp_tachi();
        let base = "The operator prefers concise Rust answers for equity trading notes";

        {
            let mut store = mem.store_handle().lock();
            seed_raw_scoped(
                &mut store,
                "explicit-default",
                &TachiMemory::storage_path(
                    Some("default"),
                    Some("default"),
                    &MemoryCategory::Core,
                    "pref",
                ),
                base,
                0.95,
                "default",
                "default",
                "pref",
            );
            // Same text, no zeroclaw metadata at all (non-zeroclaw row).
            let mut bare = store.get("explicit-default").unwrap().unwrap();
            bare.id = "metadata-less".into();
            bare.path = "/agents/orphan/pref".into();
            bare.importance = 0.5;
            bare.metadata = serde_json::json!({});
            store.upsert(&bare).unwrap();
        }

        let report = mem.run_light_sleep_governance_report().unwrap();
        assert_eq!(
            report.near_dup_archived, 0,
            "metadata-less rows must not merge into the default partition: {report:?}"
        );
        let store = mem.store_handle().lock();
        assert!(
            !store
                .get("metadata-less")
                .unwrap()
                .expect("bare row")
                .archived
        );
        assert!(
            !store
                .get("explicit-default")
                .unwrap()
                .expect("default row")
                .archived
        );
    }

    /// Cross-agent near-dup must never merge (RomanBath character isolation).
    ///
    /// Discriminating vs `2ac918587`: that tip would archive one of two
    /// near-identical character rows; this asserts zero archives.
    #[tokio::test]
    async fn romanbath_governance_near_dup_never_crosses_agents() {
        let (_tmp, mem) = temp_tachi();
        let base = "Shared character greeting about tea ceremonies and quiet evenings";

        {
            let mut store = mem.store_handle().lock();
            seed_raw_scoped(
                &mut store,
                "char-alice",
                &TachiMemory::storage_path(
                    Some("char_alice"),
                    Some("default"),
                    &MemoryCategory::Core,
                    "greeting",
                ),
                base,
                0.5,
                "char_alice",
                "default",
                "greeting",
            );
            seed_raw_scoped(
                &mut store,
                "char-bob",
                &TachiMemory::storage_path(
                    Some("char_bob"),
                    Some("default"),
                    &MemoryCategory::Core,
                    "greeting",
                ),
                &format!("{base} tonight"),
                0.95,
                "char_bob",
                "default",
                "greeting",
            );
        }

        let report = mem.run_light_sleep_governance_report().unwrap();
        assert_eq!(
            report.near_dup_archived, 0,
            "near-dup must not archive across agents/characters: {report:?}"
        );

        let store = mem.store_handle().lock();
        assert!(!store.get("char-alice").unwrap().expect("alice").archived);
        assert!(!store.get("char-bob").unwrap().expect("bob").archived);
    }

    fn seed_raw_scoped(
        store: &mut MemoryStore,
        id: &str,
        path: &str,
        text: &str,
        importance: f64,
        agent: &str,
        namespace: &str,
        key: &str,
    ) {
        let now = chrono::Local::now().to_rfc3339();
        let entry = MemoryEntry {
            id: id.into(),
            path: path.into(),
            summary: text.chars().take(80).collect(),
            text: text.into(),
            importance,
            timestamp: now.clone(),
            valid_from: now,
            valid_until: None,
            category: "fact".into(),
            topic: String::new(),
            keywords: Vec::new(),
            persons: Vec::new(),
            entities: Vec::new(),
            location: String::new(),
            source: "zeroclaw-scenario".into(),
            scope: "general".into(),
            archived: false,
            access_count: 0,
            last_access: None,
            revision: 1,
            vector: None,
            retention_policy: None,
            domain: None,
            metadata: serde_json::json!({
                "zeroclaw_key": key,
                "zeroclaw_category": "core",
                "zeroclaw_namespace": namespace,
                "zeroclaw_agent": agent,
            }),
            recall_count: 0,
            query_diversity: 0,
            tier: "raw".into(),
        };
        store.upsert(&entry).expect("seed");
    }
}

use super::*;
use tempfile::TempDir;
use zeroclaw_api::TOOL_LOOP_MEMORY_SESSIONS;

#[tokio::test]
async fn point_reads_deletes_and_new_writes_do_not_grow_full_entry_pages() {
    let tmp = TempDir::new().unwrap();
    {
        let memory = TachiMemory::new("tachi", tmp.path()).unwrap();
        for i in 0..12 {
            memory
                .store(
                    &format!("key-{i}"),
                    &"payload ".repeat(8192),
                    MemoryCategory::Core,
                    None,
                )
                .await
                .unwrap();
        }
    }
    // Full-entry listing still fails closed at this deliberately tiny cap.
    // Point operations must not use that listing to find one logical key.
    let memory = TachiMemory::with_embedder_and_page_size(
        "tachi",
        tmp.path(),
        Arc::new(crate::embeddings::NoopEmbedding),
        0.7,
        0.3,
        2,
        2,
    )
    .unwrap();
    assert!(memory.list(None, None).await.is_err());
    assert_eq!(memory.get("key-11").await.unwrap().unwrap().key, "key-11");
    assert_eq!(
        memory
            .get_for_agent("key-11", "default")
            .await
            .unwrap()
            .unwrap()
            .key,
        "key-11"
    );
    assert!(memory.get("absent").await.unwrap().is_none());
    memory
        .store("new-key", "new value", MemoryCategory::Core, None)
        .await
        .unwrap();
    memory
        .store_with_metadata(
            "key-11",
            "updated value",
            MemoryCategory::Daily,
            None,
            Some("updated-namespace"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        memory.get("key-11").await.unwrap().unwrap().content,
        "updated value"
    );
    assert!(memory.forget("key-11").await.unwrap());
    assert!(!memory.forget("absent").await.unwrap());
    assert!(memory.get("key-11").await.unwrap().is_none());
    assert_eq!(memory.count().await.unwrap(), 12);
}

#[tokio::test]
async fn typed_key_candidates_exclude_unrelated_bodies_and_archived_rows() {
    let tmp = TempDir::new().unwrap();
    let memory = TachiMemory::new("tachi", tmp.path()).unwrap();
    let key = "quote%'_\"雪/..";
    for i in 0..12 {
        memory
            .store_with_agent(
                &format!("other-{i}"),
                "unrelated",
                MemoryCategory::Core,
                None,
                None,
                None,
                Some("alpha"),
            )
            .await
            .unwrap();
    }
    for session in ["A", "B"] {
        memory
            .store_with_agent(
                key,
                session,
                MemoryCategory::Conversation,
                Some(session),
                None,
                None,
                Some("alpha"),
            )
            .await
            .unwrap();
    }
    let a = TOOL_LOOP_MEMORY_SESSIONS
        .scope(vec!["A".into()], memory.get_for_agent(key, "alpha"))
        .await
        .unwrap()
        .unwrap();
    {
        let mut store = memory.store_handle().lock();
        let pattern = TachiMemory::key_path_pattern(key, Some("alpha"));
        let candidates = store.list_memories_by_path_prefix(&pattern).unwrap();
        assert_eq!(
            candidates.len(),
            2,
            "SQL projection returns only this encoded key's candidates"
        );
        assert!(
            candidates
                .iter()
                .all(|row| row.metadata.contains(METADATA_ZC_KEY))
        );
        store.archive_memory(&a.id).unwrap();
        let live = TachiMemory::rows_matching_key(&store, key, Some("alpha"), &pattern).unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].text, "B");
    }
    assert!(
        TOOL_LOOP_MEMORY_SESSIONS
            .scope(vec!["A".into()], memory.get_for_agent(key, "alpha"))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        TOOL_LOOP_MEMORY_SESSIONS
            .scope(vec!["B".into()], memory.get_for_agent(key, "alpha"))
            .await
            .unwrap()
            .unwrap()
            .content,
        "B"
    );
}

#[tokio::test]
async fn legacy_metadata_fallback_keeps_session_precedence_and_complete_delete() {
    let tmp = TempDir::new().unwrap();
    let memory = TachiMemory::new("tachi", tmp.path()).unwrap();
    memory
        .store_with_agent(
            "shared",
            "legacy A",
            MemoryCategory::Conversation,
            Some("A"),
            None,
            None,
            Some("alpha"),
        )
        .await
        .unwrap();
    let a_id = TOOL_LOOP_MEMORY_SESSIONS
        .scope(vec!["A".into()], memory.get_for_agent("shared", "alpha"))
        .await
        .unwrap()
        .unwrap()
        .id;
    {
        let mut store = memory.store_handle().lock();
        let mut row = store.get(&a_id).unwrap().unwrap();
        row.path = format!(
            "{}historical/opaque",
            TachiMemory::agent_path_prefix(Some("alpha"))
        );
        store.upsert(&row).unwrap();
    }
    memory
        .store_with_agent(
            "shared",
            "global",
            MemoryCategory::Core,
            None,
            None,
            None,
            Some("alpha"),
        )
        .await
        .unwrap();
    assert_eq!(
        TOOL_LOOP_MEMORY_SESSIONS
            .scope(vec!["A".into()], memory.get_for_agent("shared", "alpha"))
            .await
            .unwrap()
            .unwrap()
            .content,
        "legacy A"
    );
    assert_eq!(
        TOOL_LOOP_MEMORY_SESSIONS
            .scope(vec!["B".into()], memory.get_for_agent("shared", "alpha"))
            .await
            .unwrap()
            .unwrap()
            .content,
        "global"
    );
    memory
        .store_with_agent(
            "shared",
            "A updated",
            MemoryCategory::Conversation,
            Some("A"),
            None,
            None,
            Some("alpha"),
        )
        .await
        .unwrap();
    let updated = TOOL_LOOP_MEMORY_SESSIONS
        .scope(vec!["A".into()], memory.get_for_agent("shared", "alpha"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.id, a_id);
    assert_eq!(updated.content, "A updated");
    // A delete must include historical duplicates even when the canonical
    // path already matches. No shape/UUID is accepted as session authority.
    {
        let mut store = memory.store_handle().lock();
        let mut duplicate = store.get(&a_id).unwrap().unwrap();
        duplicate.id = Uuid::new_v4().to_string();
        duplicate.path = format!(
            "{}historical/duplicate",
            TachiMemory::agent_path_prefix(Some("alpha"))
        );
        store.upsert(&duplicate).unwrap();
    }
    assert!(
        TOOL_LOOP_MEMORY_SESSIONS
            .scope(vec!["A".into()], memory.forget_for_agent("shared", "alpha"))
            .await
            .unwrap()
    );
    let rows = memory.export_agent("alpha").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].content, "global");
}

#[tokio::test]
async fn candidate_path_never_overrides_exact_agent_and_key_metadata() {
    let tmp = TempDir::new().unwrap();
    let memory = TachiMemory::new("tachi", tmp.path()).unwrap();
    memory
        .store_with_agent(
            "shared",
            "foreign",
            MemoryCategory::Core,
            None,
            None,
            None,
            Some("beta"),
        )
        .await
        .unwrap();
    let foreign = memory
        .get_for_agent("shared", "beta")
        .await
        .unwrap()
        .unwrap();
    {
        let mut store = memory.store_handle().lock();
        let mut row = store.get(&foreign.id).unwrap().unwrap();
        row.path = TachiMemory::storage_path(Some("alpha"), None, &MemoryCategory::Core, "shared");
        store.upsert(&row).unwrap();
    }
    assert!(
        memory
            .get_for_agent("shared", "alpha")
            .await
            .unwrap()
            .is_none()
    );
    assert!(!memory.forget_for_agent("shared", "alpha").await.unwrap());
    memory
        .store_with_agent(
            "shared",
            "own",
            MemoryCategory::Core,
            None,
            None,
            None,
            Some("alpha"),
        )
        .await
        .unwrap();
    let own = memory
        .get_for_agent("shared", "alpha")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(own.content, "own");
    assert_ne!(own.id, foreign.id);
    assert!(
        memory
            .store_handle()
            .lock()
            .get(&foreign.id)
            .unwrap()
            .is_some()
    );
}

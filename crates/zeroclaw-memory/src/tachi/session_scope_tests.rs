use super::*;
use crate::AgentScopedMemory;
use tempfile::TempDir;
use zeroclaw_api::TOOL_LOOP_MEMORY_SESSIONS;

fn temp_memory() -> (TempDir, TachiMemory) {
    let tmp = TempDir::new().unwrap();
    let memory = TachiMemory::new("tachi", tmp.path()).unwrap();
    (tmp, memory)
}

async fn scoped_get(memory: &dyn Memory, session: &str, key: &str) -> Option<MemoryEntry> {
    TOOL_LOOP_MEMORY_SESSIONS
        .scope(vec![session.into()], memory.get(key))
        .await
        .unwrap()
}

#[tokio::test]
async fn same_key_sessions_and_global_fact_roundtrip_without_overwrite() {
    let (_tmp, memory) = temp_memory();
    for session in ["A", "B"] {
        memory
            .store(
                "shared",
                session,
                MemoryCategory::Conversation,
                Some(session),
            )
            .await
            .unwrap();
    }
    let a_id = scoped_get(&memory, "A", "shared").await.unwrap().id;
    memory
        .store("shared", "global", MemoryCategory::Core, Some("provenance"))
        .await
        .unwrap();
    let global_id = scoped_get(&memory, "C", "shared").await.unwrap().id;
    memory
        .store_with_metadata(
            "shared",
            "global updated",
            MemoryCategory::Daily,
            Some("other provenance"),
            Some("user:preferences"),
            None,
        )
        .await
        .unwrap();
    memory
        .store_with_metadata(
            "shared",
            "A updated",
            MemoryCategory::Conversation,
            Some("A"),
            Some("dialogue"),
            None,
        )
        .await
        .unwrap();

    for (session, expected) in [("A", "A updated"), ("B", "B"), ("C", "global updated")] {
        let row = scoped_get(&memory, session, "shared").await.unwrap();
        assert_eq!(row.key, "shared");
        assert_eq!(row.content, expected);
    }
    assert_eq!(scoped_get(&memory, "A", "shared").await.unwrap().id, a_id);
    assert_eq!(
        scoped_get(&memory, "C", "shared").await.unwrap().id,
        global_id
    );
    assert_eq!(memory.count().await.unwrap(), 3);
    assert_eq!(memory.list(None, Some("A")).await.unwrap().len(), 1);
    assert!(
        TOOL_LOOP_MEMORY_SESSIONS
            .scope(vec!["A".into()], memory.forget("shared"))
            .await
            .unwrap()
    );
    assert_eq!(
        scoped_get(&memory, "A", "shared").await.unwrap().content,
        "global updated"
    );
    assert_eq!(
        scoped_get(&memory, "B", "shared").await.unwrap().content,
        "B"
    );
}

#[tokio::test]
async fn foreign_empty_and_secondary_session_contexts_cannot_read_or_delete() {
    let (_tmp, memory) = temp_memory();
    for key in ["", "0", "a:*?[]\\\"雪\0"] {
        for session in ["A", "B"] {
            memory
                .store(key, session, MemoryCategory::Conversation, Some(session))
                .await
                .unwrap();
        }
        assert_eq!(scoped_get(&memory, "A", key).await.unwrap().content, "A");
        assert_eq!(scoped_get(&memory, "B", key).await.unwrap().content, "B");
        for sessions in [
            vec![],
            vec![String::new()],
            vec![" ".into()],
            vec!["C".into(), "A".into()],
        ] {
            assert!(
                TOOL_LOOP_MEMORY_SESSIONS
                    .scope(sessions.clone(), memory.get(key))
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(
                !TOOL_LOOP_MEMORY_SESSIONS
                    .scope(sessions, memory.forget(key))
                    .await
                    .unwrap()
            );
        }
        assert!(
            memory.get(key).await.unwrap().is_some(),
            "administrative reads remain available"
        );
    }
    memory
        .store(
            "unbound",
            "legacy unbound",
            MemoryCategory::Conversation,
            None,
        )
        .await
        .unwrap();
    assert!(memory.get("unbound").await.unwrap().is_some());
    assert!(scoped_get(&memory, "A", "unbound").await.is_none());
    assert!(
        !TOOL_LOOP_MEMORY_SESSIONS
            .scope(vec!["A".into()], memory.forget("unbound"))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn legacy_unscoped_path_keeps_identity_and_governance_on_restore() {
    let (tmp, memory) = temp_memory();
    memory
        .store(
            "legacy",
            "session A",
            MemoryCategory::Conversation,
            Some("A"),
        )
        .await
        .unwrap();
    let id = scoped_get(&memory, "A", "legacy").await.unwrap().id;
    {
        let mut store = memory.store_handle().lock();
        let mut legacy = store.get(&id).unwrap().unwrap();
        // Fixture from the former adapter: metadata has session authority,
        // but the physical path has no session component.
        legacy.path =
            TachiMemory::storage_path(None, None, &MemoryCategory::Conversation, "legacy");
        legacy.keywords = vec!["retained governance".into()];
        legacy.access_count = 7;
        store.upsert(&legacy).unwrap();
    }
    drop(memory);
    let memory = TachiMemory::new("tachi", tmp.path()).unwrap();
    memory
        .store(
            "legacy",
            "session B",
            MemoryCategory::Conversation,
            Some("B"),
        )
        .await
        .unwrap();
    assert_eq!(scoped_get(&memory, "A", "legacy").await.unwrap().id, id);
    memory
        .store(
            "legacy",
            "A updated",
            MemoryCategory::Conversation,
            Some("A"),
        )
        .await
        .unwrap();
    let row = scoped_get(&memory, "A", "legacy").await.unwrap();
    assert_eq!(row.id, id);
    assert_eq!(row.content, "A updated");
    let stored = memory.store_handle().lock().get(&id).unwrap().unwrap();
    assert_eq!(stored.keywords, vec!["retained governance"]);
    assert_eq!(stored.access_count, 7);
    assert_eq!(
        scoped_get(&memory, "B", "legacy").await.unwrap().content,
        "session B"
    );
    assert_eq!(memory.count().await.unwrap(), 2);
}

#[tokio::test]
async fn agent_wrapper_keeps_session_agent_and_soul_boundaries() {
    let (_tmp, memory) = temp_memory();
    let memory = Arc::new(memory);
    let alpha = AgentScopedMemory::new(memory.clone(), "alpha", Vec::<String>::new());
    let beta = AgentScopedMemory::new(memory.clone(), "beta", Vec::<String>::new());
    for (wrapper, session, content) in [
        (&alpha, "A", "alpha A"),
        (&alpha, "B", "alpha B"),
        (&beta, "A", "beta A"),
    ] {
        wrapper
            .store(
                "shared",
                content,
                MemoryCategory::Conversation,
                Some(session),
            )
            .await
            .unwrap();
    }
    assert_eq!(
        scoped_get(&alpha, "A", "shared").await.unwrap().content,
        "alpha A"
    );
    assert_eq!(
        scoped_get(&beta, "A", "shared").await.unwrap().content,
        "beta A"
    );
    let listed = alpha.list(None, Some("A")).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].content, "alpha A");
    let recalled = alpha.recall("", 10, Some("A"), None, None).await.unwrap();
    assert_eq!(recalled.len(), 1);
    assert_eq!(recalled[0].content, "alpha A");
    assert!(
        TOOL_LOOP_MEMORY_SESSIONS
            .scope(vec!["A".into()], alpha.forget("shared"))
            .await
            .unwrap()
    );
    assert!(scoped_get(&alpha, "A", "shared").await.is_none());
    assert_eq!(
        scoped_get(&alpha, "B", "shared").await.unwrap().content,
        "alpha B"
    );
    assert_eq!(
        scoped_get(&beta, "A", "shared").await.unwrap().content,
        "beta A"
    );

    let soul_key = format!("{}alpha::fixture", crate::SOUL_KEY_PREFIX);
    memory
        .store_with_agent(
            &soul_key,
            "reserved fixture",
            MemoryCategory::Core,
            None,
            Some(crate::SOUL_NAMESPACE),
            None,
            Some("alpha"),
        )
        .await
        .unwrap();
    assert!(scoped_get(&alpha, "A", &soul_key).await.is_none());
    assert!(
        TOOL_LOOP_MEMORY_SESSIONS
            .scope(vec!["A".into()], alpha.forget(&soul_key))
            .await
            .is_err()
    );
    assert!(memory.get(&soul_key).await.unwrap().is_none());
    assert!(!memory.forget(&soul_key).await.unwrap());
    assert!(
        memory
            .get_for_agent(&soul_key, "alpha")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn physical_path_shaped_key_does_not_alias_logical_identity() {
    let (_tmp, memory) = temp_memory();
    memory
        .store("plain", "private", MemoryCategory::Conversation, Some("A"))
        .await
        .unwrap();
    let row = scoped_get(&memory, "A", "plain").await.unwrap();
    let path = memory
        .store_handle()
        .lock()
        .get(&row.id)
        .unwrap()
        .unwrap()
        .path;
    assert!(scoped_get(&memory, "A", &path).await.is_none());
    assert!(
        !TOOL_LOOP_MEMORY_SESSIONS
            .scope(vec!["A".into()], memory.forget(&path))
            .await
            .unwrap()
    );
    memory
        .store(&path, "literal key", MemoryCategory::Core, None)
        .await
        .unwrap();
    assert_eq!(
        scoped_get(&memory, "A", &path).await.unwrap().content,
        "literal key"
    );
    assert_eq!(
        scoped_get(&memory, "A", "plain").await.unwrap().content,
        "private"
    );
    assert_eq!(memory.count().await.unwrap(), 2);
}

#[tokio::test]
async fn same_key_session_purge_and_agent_rename_preserve_other_identity() {
    let (_tmp, memory) = temp_memory();
    for (agent, session) in [("alpha", "A"), ("alpha", "B"), ("beta", "A")] {
        memory
            .store_with_agent(
                "shared",
                &format!("{agent} {session}"),
                MemoryCategory::Conversation,
                Some(session),
                None,
                None,
                Some(agent),
            )
            .await
            .unwrap();
    }
    assert_eq!(memory.rename_agent("alpha", "renamed").await.unwrap(), 2);
    assert_eq!(
        memory
            .purge_session_for_agent("A", "renamed")
            .await
            .unwrap(),
        1
    );
    assert!(
        TOOL_LOOP_MEMORY_SESSIONS
            .scope(vec!["A".into()], memory.get_for_agent("shared", "renamed"))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        TOOL_LOOP_MEMORY_SESSIONS
            .scope(vec!["B".into()], memory.get_for_agent("shared", "renamed"))
            .await
            .unwrap()
            .unwrap()
            .content,
        "alpha B"
    );
    assert_eq!(
        TOOL_LOOP_MEMORY_SESSIONS
            .scope(vec!["A".into()], memory.get_for_agent("shared", "beta"))
            .await
            .unwrap()
            .unwrap()
            .content,
        "beta A"
    );
}

#[tokio::test]
async fn light_sleep_cannot_archive_another_conversation_session_or_global_fact() {
    let (_tmp, memory) = temp_memory();
    let content = "Synthetic fixture with identical conversation context for every session";
    for session in [Some("A"), Some("B"), None] {
        let key = if session.is_some() {
            "shared"
        } else {
            "unbound"
        };
        memory
            .store(key, content, MemoryCategory::Conversation, session)
            .await
            .unwrap();
    }
    memory
        .store("shared", content, MemoryCategory::Core, None)
        .await
        .unwrap();
    let report = memory.run_light_sleep_governance_report().unwrap();
    assert_eq!(report.near_dup_archived, 0);
    assert_eq!(memory.count().await.unwrap(), 4);
    for session in ["A", "B"] {
        let row = scoped_get(&memory, session, "shared").await.unwrap();
        assert_eq!(row.category, MemoryCategory::Conversation);
        assert_eq!(row.session_id.as_deref(), Some(session));
    }
    assert_eq!(
        scoped_get(&memory, "C", "shared").await.unwrap().category,
        MemoryCategory::Core
    );

    memory
        .store("twin", content, MemoryCategory::Conversation, Some("A"))
        .await
        .unwrap();
    let report = memory.run_light_sleep_governance_report().unwrap();
    assert_eq!(
        report.near_dup_archived, 1,
        "same-session near duplicates still merge"
    );
    assert_eq!(
        memory
            .list(Some(&MemoryCategory::Conversation), Some("A"))
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(scoped_get(&memory, "B", "shared").await.is_some());
}

#[tokio::test]
async fn global_session_provenance_does_not_disable_light_sleep_deduplication() {
    let (_tmp, memory) = temp_memory();
    for session in ["A", "B"] {
        memory
            .store(
                session,
                "Identical synthetic global preference fixture",
                MemoryCategory::Core,
                Some(session),
            )
            .await
            .unwrap();
    }
    let report = memory.run_light_sleep_governance_report().unwrap();
    assert_eq!(report.near_dup_archived, 1);
}

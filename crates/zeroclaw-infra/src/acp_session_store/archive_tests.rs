use super::*;
use serde_json::Value;

fn legacy_store() -> (tempfile::TempDir, AcpSessionStore) {
    let directory = tempfile::tempdir().unwrap();
    let store = AcpSessionStore::new(directory.path()).unwrap();
    store.conn.lock().execute_batch(
        r#"INSERT INTO acp_sessions (id, session_uuid, agent_alias, workspace_dir, token_count, killed_at, created_at, last_activity, plan_json)
         VALUES (1, 'archived-session', 'alpha', '/example/workspace', 123, '2026-01-02T00:00:00Z', '2026-01-01T00:00:00Z', '2026-01-02T00:00:00Z', '{raw legacy plan}'),
                (2, 'other-session', 'beta', '/example/other', 0, NULL, '2026-01-01T00:00:00Z', '2026-01-02T00:00:00Z', NULL);
         INSERT INTO acp_messages (id, session_id, role, content, reasoning_content, created_at)
         VALUES (10, 1, 'assistant', 'complete body 中文', 'retained reasoning', '2026-01-01T00:00:01Z');
         INSERT INTO acp_tool_calls (id, message_id, tool_call_id, tool_name, event_kind, payload, outcome, created_at)
         VALUES (20, 10, 'call-one', 'file_read', 'in', '{"path":"note.txt"}', NULL, '2026-01-01T00:00:02Z'),
                (21, 10, 'call-one', 'file_read', 'out', 'complete tool result', 'success', '2026-01-01T00:00:03Z');
         INSERT INTO acp_session_events (id, session_id, action, outcome, payload, created_at)
         VALUES (30, 1, 'note', 'success', 'complete event', '2026-01-01T00:00:04Z');"#
    ).unwrap();
    (directory, store)
}

#[test]
fn full_archive_preserves_all_children_raw_plan_and_other_agent() {
    let (directory, store) = legacy_store();
    let path = directory.path().join("archive/acp.json");
    assert_eq!(
        store
            .archive_and_delete_sessions_by_agent("alpha", &path)
            .unwrap(),
        1
    );
    let snapshot: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let tables = &snapshot["tables"];
    assert_eq!(tables["acp_sessions"][0]["plan_json"], "{raw legacy plan}");
    assert_eq!(tables["acp_sessions"][0]["token_count"], 123);
    assert_eq!(tables["acp_messages"][0]["content"], "complete body 中文");
    assert_eq!(
        tables["acp_messages"][0]["reasoning_content"],
        "retained reasoning"
    );
    assert_eq!(tables["acp_tool_calls"].as_array().unwrap().len(), 2);
    assert_eq!(
        tables["acp_tool_calls"][1]["payload"],
        "complete tool result"
    );
    assert_eq!(tables["acp_session_events"][0]["payload"], "complete event");
    assert_eq!(
        tables["acp_tool_calls"][0]["message_id"],
        tables["acp_messages"][0]["id"]
    );
    assert_eq!(
        tables["acp_messages"][0]["session_id"],
        tables["acp_sessions"][0]["id"]
    );
    assert!(store.load_session("archived-session").unwrap().is_none());
    assert!(store.load_session("other-session").unwrap().is_some());
    // A repeated cleanup cannot overwrite the only archive with an empty one.
    let before = std::fs::read(&path).unwrap();
    assert!(
        store
            .archive_and_delete_sessions_by_agent("alpha", &path)
            .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn failed_write_corrupt_archive_and_live_gate_preserve_original_rows() {
    let (directory, store) = legacy_store();
    let blocker = directory.path().join("file");
    std::fs::write(&blocker, b"block").unwrap();
    assert!(
        store
            .archive_and_delete_sessions_by_agent("alpha", &blocker.join("acp.json"))
            .is_err()
    );
    let path = directory.path().join("corrupt.json");
    std::fs::write(&path, b"{truncated").unwrap();
    assert!(
        store
            .archive_and_delete_sessions_by_agent("alpha", &path)
            .is_err()
    );
    assert!(
        store
            .archive_and_delete_sessions_by_agent("beta", &directory.path().join("beta.json"))
            .is_err()
    );
    let connection = store.conn.lock();
    for (table, expected) in [
        ("acp_sessions", 2),
        ("acp_messages", 1),
        ("acp_tool_calls", 2),
        ("acp_session_events", 1),
    ] {
        let count: i64 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, expected);
    }
}

#[test]
fn raw_export_error_keeps_original_records_and_publishes_no_archive() {
    let (directory, store) = legacy_store();
    store
        .conn
        .lock()
        .execute(
            "UPDATE acp_messages SET content = CAST(X'FF' AS TEXT) WHERE id = 10",
            [],
        )
        .unwrap();
    let path = directory.path().join("archive/acp.json");
    assert!(
        store
            .archive_and_delete_sessions_by_agent("alpha", &path)
            .is_err()
    );
    assert!(!path.exists());
    let connection = store.conn.lock();
    let raw: Vec<u8> = connection
        .query_row(
            "SELECT CAST(content AS BLOB) FROM acp_messages WHERE id = 10",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(raw, [0xff]);
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM acp_sessions WHERE agent_alias = 'alpha'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn concurrent_second_connection_never_deletes_unarchived_sessions() {
    let (directory, store) = legacy_store();
    let database = directory.path().join("sessions/acp-sessions.db");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let writer_barrier = barrier.clone();
    let writer = std::thread::spawn(move || {
        let connection = Connection::open(database).unwrap();
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        writer_barrier.wait();
        for index in 0..32 {
            connection.execute(
                "INSERT INTO acp_sessions (session_uuid, agent_alias, workspace_dir, killed_at, created_at, last_activity)
                 VALUES (?1, 'alpha', '/example/workspace', 'terminal', 'created', 'activity')",
                [format!("concurrent-{index}")],
            ).unwrap();
        }
    });
    let path = directory.path().join("archive/acp.json");
    barrier.wait();
    store
        .archive_and_delete_sessions_by_agent("alpha", &path)
        .unwrap();
    writer.join().unwrap();
    let snapshot: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let mut ids: std::collections::HashSet<String> = snapshot["tables"]["acp_sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["session_uuid"].as_str().unwrap().to_owned())
        .collect();
    let connection = store.conn.lock();
    let mut statement = connection
        .prepare("SELECT session_uuid FROM acp_sessions WHERE agent_alias = 'alpha'")
        .unwrap();
    for id in statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
    {
        assert!(
            ids.insert(id.unwrap()),
            "a snapshot row must not survive successful deletion"
        );
    }
    assert_eq!(
        ids.len(),
        33,
        "every committed row must be archived or still present"
    );
}

#[cfg(unix)]
#[test]
fn ancestor_directory_sync_failure_retains_database_rows() {
    use std::os::unix::fs::PermissionsExt;
    let (directory, store) = legacy_store();
    let ancestor = directory.path().join("unreadable-parent");
    std::fs::create_dir(&ancestor).unwrap();
    // Creation below this directory is allowed, but opening the ancestor for
    // directory fsync must fail for the unprivileged test/production process.
    std::fs::set_permissions(&ancestor, std::fs::Permissions::from_mode(0o300)).unwrap();
    let result =
        store.archive_and_delete_sessions_by_agent("alpha", &ancestor.join("cascade/acp.json"));
    std::fs::set_permissions(&ancestor, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(result.is_err());
    assert!(
        ancestor.join("cascade/acp.json").is_file(),
        "publication succeeded before ancestor synchronization failed"
    );
    assert!(store.load_session("archived-session").unwrap().is_some());
}

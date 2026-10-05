use super::*;
use zeroclaw_api::review::{UserMessageIngress, UserMessageSource};

#[test]
fn message_source_migration_preserves_unknown_history_and_bound_sources_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
    let path = dir.path().join("sessions/sessions.db");
    let old = Connection::open(&path).unwrap();
    old.execute_batch("CREATE TABLE sessions (id INTEGER PRIMARY KEY AUTOINCREMENT, session_key TEXT NOT NULL, role TEXT NOT NULL, content TEXT NOT NULL, created_at TEXT NOT NULL); INSERT INTO sessions (session_key, role, content, created_at) VALUES ('mixed', 'user', 'unattributed legacy input', '2026-10-01T00:00:00Z');").unwrap();
    drop(old);
    let store = SqliteSessionBackend::new(dir.path()).unwrap();
    let ingress = UserMessageIngress {
        source: UserMessageSource::Channel {
            sender_id: "owner-fixture".into(),
        },
        text: "bound owner input".into(),
    };
    store
        .append_with_ingress(
            "mixed",
            &ChatMessage::user("enriched derived context"),
            &ingress,
        )
        .unwrap();
    store
        .set_session_context(
            "mixed",
            SessionContext {
                channel_id: Some("whatsapp.main"),
                room_id: Some("group-fixture"),
                sender_id: Some("stranger-fixture"),
            },
        )
        .unwrap();
    drop(store);
    let reopened = SqliteSessionBackend::new(dir.path()).unwrap();
    let rows = reopened.load_with_timestamps("mixed");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].message.content, "unattributed legacy input");
    assert!(rows[0].ingress.is_none());
    assert!(rows[0].created_at.is_some());
    assert_eq!(rows[1].ingress, Some(ingress));
    assert_eq!(rows[1].message.content, "enriched derived context");
    assert_eq!(
        reopened
            .get_session_metadata("mixed")
            .unwrap()
            .sender_id
            .as_deref(),
        Some("stranger-fixture")
    );
}

#[test]
fn source_and_message_append_roll_back_when_metadata_write_fails() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionBackend::new(dir.path()).unwrap();
    store.conn.lock().execute_batch("CREATE TRIGGER deny_metadata BEFORE INSERT ON session_metadata BEGIN SELECT RAISE(ABORT, 'fixture metadata refusal'); END;").unwrap();
    let err = store
        .append_with_ingress(
            "owner",
            &ChatMessage::user("owner input"),
            &UserMessageIngress {
                source: UserMessageSource::Operator,
                text: "owner input".into(),
            },
        )
        .unwrap_err();
    assert!(err.to_string().contains("fixture metadata refusal"));
    assert!(store.load("owner").is_empty());
    assert!(store.load_with_timestamps("owner").is_empty());
    assert!(
        store
            .append_with_ingress(
                "owner",
                &ChatMessage::assistant("assistant"),
                &UserMessageIngress {
                    source: UserMessageSource::Operator,
                    text: "owner input".into()
                }
            )
            .is_err()
    );
}

#[test]
fn invalid_or_source_only_ingress_records_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionBackend::new(dir.path()).unwrap();
    for raw in [
        "broken json",
        r#"{"kind":"operator"}"#,
        r#"{"source":{"kind":"operator"}}"#,
        r#"{"source":{"kind":"operator"},"text":"owner","extra":true}"#,
        r#"{"source":{"kind":"unknown"},"text":"owner"}"#,
    ] {
        store.conn.lock().execute(
            "INSERT INTO sessions(session_key,role,content,created_at,message_source) VALUES ('invalid','user','derived context','2026-10-01T00:00:00Z',?1)",
            [raw],
        ).unwrap();
    }
    let rows = store.load_with_timestamps("invalid");
    assert_eq!(rows.len(), 5, "history remains readable");
    assert!(rows.iter().all(|r| r.ingress.is_none()));
}

use super::*;
use zeroclaw_api::review::UserMessageSource;

#[tokio::test]
async fn sequential_owner_turns_get_sources_but_anonymous_and_steering_do_not() {
    let mut chat = SharedChat::new();
    chat.state.pairing = Arc::new(zeroclaw_config::pairing::PairingGuard::new(
        false,
        &["owner-source-token".into()],
    ));
    chat.scope.auth_subject = Some(zeroclaw_config::pairing::PairingGuard::token_hash(
        "owner-source-token",
    ));
    chat.state.session_backend = Some(Arc::new(
        zeroclaw_infra::session_sqlite::SqliteSessionBackend::new(chat._tmp.path()).unwrap(),
    ));
    chat.state
        .session_backend
        .as_ref()
        .unwrap()
        .set_session_agent_alias(&chat.scope.session_key, "web")
        .unwrap();
    let mut socket = chat.attach().await;
    for (id, text) in [
        ("owner-1", "first owner input"),
        ("owner-2", "second owner input"),
    ] {
        chat.gate.add_permits(1);
        assert_eq!(
            chat.send(&socket, message_with_id(text, id)).unwrap()["status"],
            "accepted"
        );
        assert_eq!(
            frames_until_end(&mut socket).await.last().unwrap()["type"],
            "done"
        );
    }
    let mut anonymous_scope = chat.scope.clone();
    anonymous_scope.auth_subject = None;
    chat.gate.add_permits(1);
    let (_, claim) = handle_message_frame(
        &chat.state,
        &socket.conversation,
        &anonymous_scope,
        &message_with_id("anonymous input", "anonymous-1"),
    );
    start_ws_turns(
        &chat.state,
        &socket.conversation,
        &anonymous_scope,
        claim.unwrap(),
    );
    assert_eq!(
        frames_until_end(&mut socket).await.last().unwrap()["type"],
        "done"
    );

    // A late joined steering message must not inherit the first socket's
    // owner identity when it becomes a follow-up turn in the same invocation.
    let (_, first) = handle_message_frame(
        &chat.state,
        &socket.conversation,
        &chat.scope,
        &message_with_id("third owner input", "owner-3"),
    );
    let (ack, second) = handle_message_frame(
        &chat.state,
        &socket.conversation,
        &anonymous_scope,
        &message_with_id("anonymous steering", "anonymous-steering"),
    );
    assert!(second.is_none());
    assert_eq!(ack.unwrap()["turn"], "steered");
    chat.gate.add_permits(2);
    start_ws_turns(
        &chat.state,
        &socket.conversation,
        &chat.scope,
        first.unwrap(),
    );
    frames_until_end(&mut socket).await;
    // Wait for possible late steering follow-up and all persistence to settle.
    tokio::time::timeout(Duration::from_secs(10), async {
        while socket.conversation.is_running() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let reopened =
        zeroclaw_infra::session_sqlite::SqliteSessionBackend::new(chat._tmp.path()).unwrap();
    use zeroclaw_infra::session_backend::SessionBackend;
    let rows = reopened.load_with_timestamps(&chat.scope.session_key);
    let owner_rows: Vec<_> = rows
        .iter()
        .filter(|r| {
            r.ingress
                .as_ref()
                .is_some_and(|i| i.source == UserMessageSource::Operator)
        })
        .collect();
    assert_eq!(
        owner_rows.len(),
        3,
        "each ordinary paired turn has its own source"
    );
    assert!(
        owner_rows
            .iter()
            .any(|r| r.message.content.contains("first owner input"))
    );
    assert!(
        owner_rows
            .iter()
            .any(|r| r.message.content.contains("second owner input"))
    );
    assert!(
        owner_rows
            .iter()
            .all(|r| !r.message.content.contains("anonymous"))
    );
    assert!(
        owner_rows.iter().all(|r| {
            let text = &r.ingress.as_ref().unwrap().text;
            [
                "first owner input",
                "second owner input",
                "third owner input",
            ]
            .contains(&text.as_str())
        }),
        "evidence contains exact input without derived timestamps"
    );
    assert!(
        rows.iter()
            .filter(|r| r.message.content.contains("anonymous"))
            .all(|r| r.ingress.is_none())
    );
}

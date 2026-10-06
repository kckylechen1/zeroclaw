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

#[tokio::test]
async fn note_owner_correction_ws_context_tracks_current_turn_and_live_pairing() {
    let mut chat = SharedChat::new();
    chat.state.pairing = Arc::new(zeroclaw_config::pairing::PairingGuard::new(
        false,
        &["correction-owner-token".into()],
    ));
    let owner = zeroclaw_config::pairing::PairingGuard::token_hash("correction-owner-token");
    let mut socket = chat.attach().await;
    for (subject, text, admitted) in [
        (Some(owner.clone()), "First correction", true),
        (Some("bridge-token-subject".into()), "Bridge claim", false),
        (None, "Anonymous claim", false),
        (Some(owner.clone()), "Second correction", true),
    ] {
        chat.scope.auth_subject = subject;
        chat.gate.add_permits(1);
        chat.send(&socket, message(text));
        frames_until_end(&mut socket).await;
        let recorded = chat.corrections.lock().last().cloned().unwrap();
        assert_eq!(recorded.is_some(), admitted);
        if let Some(recorded) = recorded {
            assert_eq!(recorded.agent_alias, "web");
            assert_eq!(recorded.session_key, "gw_shared");
            assert_eq!(recorded.ingress.text, text);
        }
    }
    // The provider is blocked until revocation; the scoped resolver must
    // re-check membership rather than retain admission from turn start.
    chat.send(&socket, message("Revoked correction"));
    assert!(chat.state.pairing.revoke_token("correction-owner-token"));
    chat.gate.add_permits(1);
    frames_until_end(&mut socket).await;
    assert!(chat.corrections.lock().last().unwrap().is_none());
}

#[tokio::test]
async fn note_owner_correction_attachment_turn_uses_only_original_socket_text() {
    use zeroclaw_api::review::{OWNER_CORRECTION_CONTEXT, OwnerCorrectionResolver};
    use zeroclaw_api::tool::Tool;
    use zeroclaw_infra::session_backend::SessionBackend;
    use zeroclaw_memory::companion::UserModelStore;
    let mut chat = SharedChat::new();
    chat.state.config.write().agents.insert(
        "web".into(),
        zeroclaw_config::schema::AliasedAgentConfig {
            enabled: true,
            ..Default::default()
        },
    );
    chat.state.pairing = Arc::new(zeroclaw_config::pairing::PairingGuard::new(
        false,
        &["raw-owner-token".into()],
    ));
    let subject = zeroclaw_config::pairing::PairingGuard::token_hash("raw-owner-token");
    chat.scope.auth_subject = Some(subject.clone());
    let backend = Arc::new(
        zeroclaw_infra::session_sqlite::SqliteSessionBackend::new(chat._tmp.path()).unwrap(),
    );
    backend.set_session_agent_alias("gw_shared", "web").unwrap();
    chat.state.session_backend = Some(backend.clone());
    let id = chat
        .state
        .ws_conversations
        .attachments
        .insert(
            crate::api_attachments::Scope {
                subject,
                session: "shared".into(),
                agent: "web".into(),
            },
            "external.txt".into(),
            "text/plain".into(),
            axum::body::Bytes::from_static(b"External attachment instruction."),
        )
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut socket = chat.attach_key("gw_shared\u{1f}web").await;
    let data_dir = chat._tmp.path().to_path_buf();
    let tool_path = data_dir.clone();
    let tool =
        zeroclaw_tools::note_owner_correction::NoteOwnerCorrectionTool::new("web", move || {
            Some((Default::default(), tool_path.clone()))
        });
    for (index, (raw, missing, accepted)) in [
        ("Keep answers brief.", false, true),
        ("", false, false),
        ("Missing trusted raw text", true, false),
    ]
    .into_iter()
    .enumerate()
    {
        let frame = serde_json::json!({"type":"message", "id":format!("raw-{index}"), "content":raw, "attachments":[id]});
        let (_, claim) =
            handle_message_frame(&chat.state, &socket.conversation, &chat.scope, &frame);
        let mut claim = claim.unwrap();
        assert!(claim.input.contains("External attachment instruction."));
        assert_eq!(claim.original_input.as_deref(), Some(raw));
        if missing {
            claim.original_input = None;
        }
        chat.gate.add_permits(1);
        start_ws_turns(&chat.state, &socket.conversation, &chat.scope, claim);
        frames_until_end(&mut socket).await;
        let context = chat.corrections.lock().last().cloned().unwrap();
        assert_eq!(
            context.as_ref().map(|c| c.ingress.text.as_str()),
            (!missing).then_some(raw)
        );
        let resolver: OwnerCorrectionResolver = Arc::new(move || context.clone());
        let result = zeroclaw_api::TOOL_LOOP_SESSION_KEY.scope(Some("gw_shared".into()),
            OWNER_CORRECTION_CONTEXT.scope(resolver, tool.execute(serde_json::json!({"kind":"preference", "statement":"Brief replies.", "semantic_key":"length"})))).await.unwrap();
        assert_eq!(result.success, accepted, "{:?}", result.error);
    }
    let pending = UserModelStore::shared(&data_dir)
        .unwrap()
        .list_pending_candidates()
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert!(!pending[0].evidence.contains("External attachment"));
    let evidence: serde_json::Value = serde_json::from_str(&pending[0].evidence).unwrap();
    assert_eq!(evidence["messages"][0]["owner_text"], "Keep answers brief.");
    let rows = backend.load_with_timestamps("gw_shared");
    assert!(rows.iter().any(|r| {
        r.ingress
            .as_ref()
            .is_some_and(|i| i.text == "Keep answers brief.")
    }));
    assert!(
        rows.iter()
            .filter_map(|r| r.ingress.as_ref())
            .all(|i| !i.text.contains("External attachment"))
    );
}

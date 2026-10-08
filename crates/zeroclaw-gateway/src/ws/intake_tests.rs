use super::*;
use serde_json::{Value, json};
use zeroclaw_api::bridge_intake::{BridgeResume, BridgeSource};

fn intake_chat() -> SharedChat {
    let mut chat = SharedChat::new();
    let subject = zeroclaw_config::pairing::PairingGuard::token_hash("intake-test-bridge");
    chat.scope.auth_subject = Some(subject.clone());
    {
        let mut config = chat.state.config.write();
        config.agents.insert(
            "web".into(),
            zeroclaw_config::schema::AliasedAgentConfig {
                enabled: true,
                ..Default::default()
            },
        );
        config.gateway.bridges.insert(
            "files".into(),
            zeroclaw_config::schema::GatewayBridgeConfig {
                token_hash: subject,
                sessions: vec!["shared".into()],
                ..Default::default()
            },
        );
    }
    reopen_intake(&mut chat);
    chat
}

fn reopen_intake(chat: &mut SharedChat) {
    chat.state.ws_conversations = Default::default();
    // Drop the old handle as well as the hub. Recovery must read SQLite,
    // not an old conversation, attachment cache, or scheduling claim.
    chat.state.session_backend = None;
    chat.state.session_backend = Some(Arc::new(
        zeroclaw_infra::session_sqlite::SqliteSessionBackend::new(chat._tmp.path()).unwrap(),
    ));
}

async fn intake_socket(chat: &SharedChat) -> crate::ws_conversation::Subscription<WsSession> {
    chat.attach_key(&format!("{}\u{1f}web", chat.scope.session_key))
        .await
}

fn intake_source() -> BridgeSource {
    BridgeSource {
        key: "files:telegram:7:9".into(),
        session_key: "gw_shared".into(),
        agent_alias: "web".into(),
    }
}

fn intake_snapshot(chat: &SharedChat) -> BridgeResume {
    chat.state
        .session_backend
        .as_ref()
        .unwrap()
        .bridge_resume(&intake_source())
        .unwrap()
}

fn source_message(update: i64, previous_cursor: i64, content: &str) -> Value {
    json!({
        "type": "message", "id": format!("tg:7:9:{update}"), "content": content,
        "source": {"namespace": "telegram:7:9", "update_id": update, "previous_cursor": previous_cursor}
    })
}

fn receive_intake(
    chat: &SharedChat,
    socket: &crate::ws_conversation::Subscription<WsSession>,
    frame: Value,
) -> (Value, Option<TurnClaim>) {
    let (reply, claim) = handle_client_text(
        &chat.state,
        &socket.conversation,
        &chat.scope,
        &frame.to_string(),
    );
    (
        reply.expect("source frames always have an application receipt"),
        claim,
    )
}

async fn run_intake(
    chat: &SharedChat,
    socket: &crate::ws_conversation::Subscription<WsSession>,
    claim: TurnClaim,
) {
    tokio::time::timeout(
        Duration::from_secs(10),
        run_ws_turns(
            chat.state.clone(),
            socket.conversation.clone(),
            chat.scope.clone(),
            claim,
        ),
    )
    .await
    .expect("scripted intake turns must settle");
}

fn resume_frame() -> Value {
    json!({"type": "source_resume", "source": "telegram:7:9"})
}

#[tokio::test]
async fn accepted_input_survives_crash_before_spawn_and_recovers_its_body() {
    let mut chat = intake_chat();
    let a = intake_socket(&chat).await;
    let (ack, claim) = receive_intake(&chat, &a, source_message(41, 0, "recover this exact body"));
    assert_eq!(ack["status"], "accepted");
    assert_eq!(ack["durable"], true);
    assert_eq!(ack["source"]["cursor"], 42);
    assert!(claim.is_some());
    let stored = intake_snapshot(&chat);
    assert_eq!(stored.inputs[0].state, "pending");
    assert_eq!(
        serde_json::from_str::<Value>(&stored.inputs[0].payload).unwrap()["content"],
        "recover this exact body"
    );
    assert!(chat.seen.lock().is_empty());

    // The socket ACK and turn spawn never happen before the simulated crash.
    drop(claim);
    drop(a);
    reopen_intake(&mut chat);
    let mut b = intake_socket(&chat).await;
    let (ready, recovered) = receive_intake(&chat, &b, resume_frame());
    assert_eq!(ready["type"], "source_ready");
    assert_eq!(ready["cursor"], 42);
    let recovered = recovered.expect("pending body must be recoverable after reopening");
    assert_eq!(recovered.input, "recover this exact body");
    chat.gate.add_permits(1);
    run_intake(&chat, &b, recovered).await;
    let done = frames_until_end(&mut b).await.pop().unwrap();
    assert_eq!(done["type"], "done");
    assert_eq!(done["id"], "tg:7:9:41");
    assert_eq!(chat.seen.lock().len(), 1);
    assert!(chat.seen.lock()[0][0].ends_with("recover this exact body"));
    assert_eq!(intake_snapshot(&chat).inputs[0].state, "done");
}

#[tokio::test]
async fn lost_ack_retry_after_restart_does_not_execute_a_completed_input_twice() {
    let mut chat = intake_chat();
    let a = intake_socket(&chat).await;
    let frame = source_message(6, 0, "execute only once");
    let (_, claim) = receive_intake(&chat, &a, frame.clone());
    chat.gate.add_permits(2);
    run_intake(&chat, &a, claim.unwrap()).await;
    assert_eq!(chat.seen.lock().len(), 1);
    drop(a);
    reopen_intake(&mut chat);

    let b = intake_socket(&chat).await;
    let (ack, claim) = receive_intake(&chat, &b, frame);
    assert_eq!(ack["status"], "duplicate");
    assert_eq!(ack["state"], "done");
    assert_eq!(ack["source"]["cursor"], 7);
    assert!(claim.is_none());
    assert!(!b.conversation.is_running());
    assert_eq!(chat.seen.lock().len(), 1);
    assert_eq!(intake_snapshot(&chat).inputs.len(), 1);
}

#[tokio::test]
async fn a_running_input_after_restart_is_unknown_and_never_replayed() {
    let mut chat = intake_chat();
    let a = intake_socket(&chat).await;
    let frame = source_message(8, 0, "possibly performed an effect");
    let (_, claim) = receive_intake(&chat, &a, frame.clone());
    let claim = claim.unwrap();
    assert!(
        intake::begin(
            &chat.state,
            &a.conversation,
            &chat.scope,
            claim.intake.as_ref()
        )
        .unwrap()
    );
    assert_eq!(intake_snapshot(&chat).inputs[0].state, "running");
    drop(claim);
    drop(a);
    reopen_intake(&mut chat);

    let b = intake_socket(&chat).await;
    let (ready, claim) = receive_intake(&chat, &b, resume_frame());
    assert_eq!(ready["unknown"], json!(["tg:7:9:8"]));
    assert!(claim.is_none());
    let (ack, retry) = receive_intake(&chat, &b, frame);
    assert_eq!(ack["status"], "duplicate");
    assert_eq!(ack["state"], "running");
    assert!(retry.is_none());
    assert!(!b.conversation.is_running());
    assert!(chat.seen.lock().is_empty());
}

#[tokio::test]
async fn deleting_the_conversation_redacts_pending_input_without_resurrecting_it() {
    let mut chat = intake_chat();
    let a = intake_socket(&chat).await;
    let frame = source_message(11, 0, "owner deleted this body");
    let (_, claim) = receive_intake(&chat, &a, frame.clone());
    assert!(claim.is_some());
    chat.state
        .session_backend
        .as_ref()
        .unwrap()
        .delete_session(&chat.scope.session_key)
        .unwrap();
    drop(claim);
    drop(a);
    reopen_intake(&mut chat);

    let b = intake_socket(&chat).await;
    let (ready, recovered) = receive_intake(&chat, &b, resume_frame());
    assert_eq!(ready["cursor"], 12);
    assert!(recovered.is_none());
    let stored = intake_snapshot(&chat);
    assert_eq!(stored.inputs[0].state, "rejected");
    assert!(stored.inputs[0].payload.is_empty());
    let (retry, claim) = receive_intake(&chat, &b, frame);
    assert_eq!(retry["code"], "SOURCE_NOT_RECORDED");
    assert!(claim.is_none());
    assert!(!b.conversation.is_running());
    assert!(chat.seen.lock().is_empty());
}

#[tokio::test]
async fn lost_attachment_on_restart_rejects_the_whole_input_including_its_caption() {
    let mut chat = intake_chat();
    let handle = chat
        .state
        .ws_conversations
        .attachments
        .insert(
            crate::api_attachments::Scope {
                subject: chat.scope.auth_subject.clone().unwrap(),
                session: chat.scope.session_id.clone(),
                agent: "web".into(),
            },
            "note.txt".into(),
            "text/plain".into(),
            axum::body::Bytes::from_static(b"must accompany the caption"),
        )
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let a = intake_socket(&chat).await;
    let mut frame = source_message(13, 0, "do not run this caption alone");
    frame["attachments"] = json!([handle]);
    let (ack, claim) = receive_intake(&chat, &a, frame.clone());
    assert_eq!(ack["state"], "pending");
    assert!(claim.is_some());
    drop(claim);
    drop(a);
    reopen_intake(&mut chat);

    let mut b = intake_socket(&chat).await;
    let (ready, claim) = receive_intake(&chat, &b, resume_frame());
    assert_eq!(ready["cursor"], 14);
    assert!(claim.is_none());
    let error = frames_until_end(&mut b).await.pop().unwrap();
    assert_eq!(error["code"], "SOURCE_ATTACHMENT_UNAVAILABLE");
    assert_eq!(error["id"], "tg:7:9:13");
    assert_eq!(intake_snapshot(&chat).inputs[0].state, "rejected");
    let (ack, retry) = receive_intake(&chat, &b, frame);
    assert_eq!(ack["status"], "duplicate");
    assert_eq!(ack["state"], "rejected");
    assert!(retry.is_none());
    assert!(!b.conversation.is_running());
    assert!(chat.seen.lock().is_empty());
}

#[tokio::test]
async fn durable_intake_requires_sqlite_and_rechecks_revocation_before_execution() {
    let mut chat = intake_chat();
    chat.state.session_backend = None;
    let a = intake_socket(&chat).await;
    let frame = source_message(15, 0, "must remain unexecuted");
    for request in [resume_frame(), frame.clone()] {
        let (error, claim) = receive_intake(&chat, &a, request);
        assert_eq!(error["code"], "DURABLE_INTAKE_UNAVAILABLE");
        assert!(claim.is_none());
    }
    assert!(!a.conversation.is_running());
    drop(a);
    reopen_intake(&mut chat);
    let mut b = intake_socket(&chat).await;
    let (ack, claim) = receive_intake(&chat, &b, frame);
    assert_eq!(ack["status"], "accepted");
    chat.state.config.write().gateway.bridges.clear();
    chat.gate.add_permits(1);
    run_intake(&chat, &b, claim.unwrap()).await;
    assert_eq!(
        frames_until_end(&mut b).await.pop().unwrap()["code"],
        "SOURCE_NOT_STARTED"
    );
    let (error, retry) = receive_intake(&chat, &b, resume_frame());
    assert_eq!(error["code"], "SOURCE_UNAUTHORIZED");
    assert!(retry.is_none());
    assert_eq!(intake_snapshot(&chat).inputs[0].state, "pending");
    assert!(chat.seen.lock().is_empty());
}

#[tokio::test]
async fn two_sockets_queue_durable_inputs_as_distinct_turns_instead_of_steering() {
    let chat = intake_chat();
    let a = intake_socket(&chat).await;
    let b = intake_socket(&chat).await;
    assert!(Arc::ptr_eq(&a.conversation, &b.conversation));
    let (_, first) = receive_intake(&chat, &a, source_message(20, 0, "first source input"));
    let mut first = first.unwrap();
    let second_frame = source_message(21, 21, "second source input");
    let (ack, second) = receive_intake(&chat, &b, second_frame.clone());
    assert_eq!(ack["status"], "accepted");
    assert_eq!(ack["state"], "pending");
    assert_ne!(ack["turn"], "steered");
    assert!(second.is_none());
    assert!(matches!(
        first.steering.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    let (duplicate, extra) = receive_intake(&chat, &a, second_frame);
    assert_eq!(duplicate["status"], "duplicate");
    assert!(extra.is_none());

    chat.gate.add_permits(2);
    run_intake(&chat, &a, first).await;
    let seen = chat.seen.lock();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].len(), 1);
    assert!(seen[0][0].ends_with("first source input"));
    assert_eq!(seen[1].len(), 2);
    assert!(seen[1][1].ends_with("second source input"));
    drop(seen);
    let stored = intake_snapshot(&chat);
    assert_eq!(stored.cursor, 22);
    assert_eq!(stored.inputs.len(), 2);
    assert!(stored.inputs.iter().all(|input| input.state == "done"));
}

#[tokio::test]
async fn out_of_order_dispositions_advance_only_the_contiguous_source_prefix() {
    let chat = intake_chat();
    let a = intake_socket(&chat).await;
    let disposition = |update, previous, state| {
        let mut frame = source_message(update, previous, "unused");
        frame["type"] = json!("source_disposition");
        frame["disposition"] = json!(state);
        frame.as_object_mut().unwrap().remove("content");
        frame
    };
    let (later, claim) = receive_intake(&chat, &a, disposition(12, 10, "rejected"));
    assert_eq!(later["source"]["cursor"], 0);
    assert!(claim.is_none());
    let (earlier, claim) = receive_intake(&chat, &a, disposition(9, 0, "ignored"));
    assert_eq!(earlier["source"]["cursor"], 13);
    assert!(claim.is_none());
    let (ready, claim) = receive_intake(&chat, &a, resume_frame());
    assert_eq!(ready["cursor"], 13);
    assert!(claim.is_none());
    assert_eq!(intake_snapshot(&chat).inputs.len(), 2);
    assert!(!a.conversation.is_running());
    assert!(chat.seen.lock().is_empty());
}

#[tokio::test]
async fn out_of_order_messages_wait_for_the_prefix_and_execute_in_source_order() {
    let chat = intake_chat();
    let a = intake_socket(&chat).await;
    let (later, claim) = receive_intake(&chat, &a, source_message(12, 11, "source input B"));
    assert_eq!(later["status"], "accepted");
    assert_eq!(later["state"], "pending");
    assert_eq!(later["source"]["cursor"], 0);
    assert!(
        claim.is_none(),
        "B cannot execute before the missing prefix"
    );
    assert!(!a.conversation.is_running());
    assert!(chat.seen.lock().is_empty());
    let (ready, recovered) = receive_intake(&chat, &a, resume_frame());
    assert_eq!(ready["cursor"], 0);
    assert!(
        recovered.is_none(),
        "resume must also respect the source gap"
    );

    let (earlier, claim) = receive_intake(&chat, &a, source_message(10, 0, "source input A"));
    assert_eq!(earlier["source"]["cursor"], 13);
    let claim = claim.expect("filling the prefix makes A executable");
    assert_eq!(claim.input, "source input A");
    chat.gate.add_permits(2);
    run_intake(&chat, &a, claim).await;

    let seen = chat.seen.lock();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].len(), 1);
    assert!(seen[0][0].ends_with("source input A"));
    assert_eq!(seen[1].len(), 2);
    assert!(seen[1][0].ends_with("source input A"));
    assert!(seen[1][1].ends_with("source input B"));
    drop(seen);
    let stored = intake_snapshot(&chat);
    assert_eq!(stored.cursor, 13);
    assert_eq!(stored.inputs.len(), 2);
    assert!(stored.inputs.iter().all(|input| input.state == "done"));
}

#[tokio::test]
async fn a_deleted_unrun_claim_cannot_settle_deleted_input_or_block_the_next_input() {
    let chat = intake_chat();
    let mut a = intake_socket(&chat).await;
    let (_, old_claim) = receive_intake(&chat, &a, source_message(30, 0, "deleted source input A"));
    let old_claim = old_claim.expect("retain A's turn slot without running it");
    chat.state
        .session_backend
        .as_ref()
        .unwrap()
        .delete_session(&chat.scope.session_key)
        .unwrap();
    let (later, claim) = receive_intake(&chat, &a, source_message(31, 31, "live source input B"));
    assert_eq!(later["status"], "accepted");
    assert_eq!(later["state"], "pending");
    assert!(claim.is_none(), "A still owns the in-process turn slot");
    let before = intake_snapshot(&chat);
    assert_eq!(before.inputs[0].state, "rejected");
    assert!(before.inputs[0].payload.is_empty());
    assert_eq!(before.inputs[1].state, "pending");
    assert!(chat.seen.lock().is_empty());

    // The old claim loses the pending->running CAS. B must still be resumed;
    // a single permit also catches an accidental provider call for A.
    chat.gate.add_permits(1);
    run_intake(&chat, &a, old_claim).await;
    let rejected = frames_until_end(&mut a).await.pop().unwrap();
    assert_eq!(rejected["code"], "SOURCE_NOT_STARTED");
    assert_eq!(rejected["id"], "tg:7:9:30");
    let done = frames_until_end(&mut a).await.pop().unwrap();
    assert_eq!(done["type"], "done");
    assert_eq!(done["id"], "tg:7:9:31");
    let seen = chat.seen.lock();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].len(), 1);
    assert!(seen[0][0].ends_with("live source input B"));
    drop(seen);
    let after = intake_snapshot(&chat);
    assert_eq!(after.inputs.len(), 2);
    assert_eq!(after.inputs[0].state, "rejected");
    assert!(after.inputs[0].payload.is_empty());
    assert_eq!(after.inputs[1].state, "done");
    assert!(!a.conversation.is_running());
}

#[tokio::test]
async fn thin_client_and_real_websocket_route_agree_on_durable_source_receipts() {
    use zeroclaw_gateway_client::{Client, ConnectOptions, Frame, SourceInput};
    let chat = intake_chat();
    chat.state.config.write().providers.models.openai.insert(
        "default".into(),
        zeroclaw_config::schema::OpenAIModelProviderConfig {
            base: zeroclaw_config::schema::ModelProviderConfig {
                model: Some("test-model".into()),
                ..Default::default()
            },
        },
    );
    // Keep the actual shared conversation alive with its scripted provider.
    // The route must find this owner; no network model is constructed.
    let _owner = intake_socket(&chat).await;
    chat.gate.add_permits(1);
    let app = axum::Router::new()
        .route("/ws/chat", axum::routing::get(handle_ws_chat))
        .with_state(chat.state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let options = ConnectOptions {
        gateway: format!("ws://{}", listener.local_addr().unwrap()),
        agent: "web".into(),
        session_id: Some("shared".into()),
        token: Some("intake-test-bridge".into()),
    };
    let exchange = async {
        let mut client = Client::connect(&options).await.unwrap();
        let ready = client.resume_source("telegram:7:9").await.unwrap();
        assert_eq!(ready.cursor, 0);
        let source = SourceInput {
            namespace: "telegram:7:9".into(),
            update_id: 41,
            previous_cursor: 0,
        };
        client
            .send_source_message("tg:7:9:41", "wire handoff", &[], &source)
            .await
            .unwrap();
        assert!(
            matches!(client.next_frame().await.unwrap(), Some(Frame::Ack {
            id, durable: Some(true), intake_version: Some(1), source: Some(receipt), ..
        }) if id == "tg:7:9:41" && receipt.cursor == 42 && receipt.namespace == "telegram:7:9")
        );
        loop {
            match client.next_frame().await.unwrap().unwrap() {
                Frame::Done { .. } => break,
                Frame::Error { code, .. } => panic!("wire execution failed: {code:?}"),
                _ => {}
            }
        }
        drop(client);
        let mut client = Client::connect(&options).await.unwrap();
        let ready = client.resume_source("telegram:7:9").await.unwrap();
        assert_eq!(ready.cursor, 42);
        assert_eq!(ready.receipts.len(), 1);
        assert_eq!(ready.receipts[0].state, "done");
        client
            .send_source_message("tg:7:9:41", "wire handoff", &[], &source)
            .await
            .unwrap();
        assert!(
            matches!(client.next_frame().await.unwrap(), Some(Frame::Ack {
            status, state: Some(state), durable: Some(true), ..
        }) if status == "duplicate" && state == "done")
        );
        assert_eq!(chat.seen.lock().len(), 1);
    };
    tokio::select! {
        result = axum::serve(listener, app.into_make_service()) => panic!("server stopped: {result:?}"),
        result = tokio::time::timeout(Duration::from_secs(10), exchange) => result.unwrap(),
    }
}

#[tokio::test]
async fn an_ignored_predecessor_unblocks_the_pending_message_without_reconnect() {
    let chat = intake_chat();
    let a = intake_socket(&chat).await;
    let (_, blocked) = receive_intake(&chat, &a, source_message(12, 11, "pending after ignored"));
    assert!(blocked.is_none());
    let mut ignored = source_message(10, 0, "");
    ignored["type"] = json!("source_disposition");
    ignored["disposition"] = json!("ignored");
    let (ack, claim) = receive_intake(&chat, &a, ignored);
    assert_eq!(ack["source"]["cursor"], 13);
    assert!(
        ack.get("turn").is_none(),
        "the ignored update did not start its own turn"
    );
    let claim = claim.expect("closing the gap must schedule the pending successor");
    assert_eq!(claim.request_id.as_deref(), Some("tg:7:9:12"));
    chat.gate.add_permits(1);
    run_intake(&chat, &a, claim).await;
    assert_eq!(chat.seen.lock().len(), 1);
    assert_eq!(intake_snapshot(&chat).inputs[1].state, "done");
}

#[tokio::test]
async fn bridge_token_rotation_preserves_the_same_source_identity() {
    let mut chat = intake_chat();
    let a = intake_socket(&chat).await;
    let frame = source_message(21, 0, "one identity through token rotation");
    let (_, claim) = receive_intake(&chat, &a, frame.clone());
    chat.gate.add_permits(1);
    run_intake(&chat, &a, claim.unwrap()).await;
    drop(a);
    let new_subject = zeroclaw_config::pairing::PairingGuard::token_hash("rotated-intake-fixture");
    {
        let mut config = chat.state.config.write();
        let mut bridge = config.gateway.bridges.remove("files").unwrap();
        bridge.token_hash = new_subject.clone();
        config.gateway.bridges.insert("files".into(), bridge);
    }
    chat.scope.auth_subject = Some(new_subject);
    reopen_intake(&mut chat);
    let a = intake_socket(&chat).await;
    let (ack, claim) = receive_intake(&chat, &a, frame);
    assert_eq!(ack["status"], "duplicate");
    assert_eq!(ack["state"], "done");
    assert!(claim.is_none());
    assert_eq!(chat.seen.lock().len(), 1);
}

#[tokio::test]
async fn ambiguous_bridge_credentials_cannot_choose_a_source_by_hashmap_order() {
    let chat = intake_chat();
    {
        let mut config = chat.state.config.write();
        let mut bridge = config.gateway.bridges["files"].clone();
        bridge.token_hash = bridge.token_hash.to_ascii_uppercase();
        config
            .gateway
            .bridges
            .insert("ambiguous-files".into(), bridge);
    }
    let a = intake_socket(&chat).await;
    for frame in [resume_frame(), source_message(21, 0, "refuse ambiguity")] {
        let (error, claim) = receive_intake(&chat, &a, frame);
        assert_eq!(error["code"], "SOURCE_UNAUTHORIZED");
        assert!(claim.is_none());
    }
    assert_eq!(intake_snapshot(&chat).cursor, 0);
    assert!(intake_snapshot(&chat).inputs.is_empty());
    assert!(chat.seen.lock().is_empty());
}

#[tokio::test]
async fn deleting_an_unrun_intake_preserves_already_accepted_legacy_steering() {
    let chat = intake_chat();
    let a = intake_socket(&chat).await;
    let (_, claim) = receive_intake(&chat, &a, source_message(10, 0, "deleted source body"));
    let ack = chat
        .send(
            &a,
            message_with_id("accepted legacy followup", "legacy-followup"),
        )
        .unwrap();
    assert_eq!(ack["turn"], "steered");
    chat.state
        .session_backend
        .as_ref()
        .unwrap()
        .delete_session(&chat.scope.session_key)
        .unwrap();
    chat.gate.add_permits(1);
    run_intake(&chat, &a, claim.unwrap()).await;
    assert_eq!(chat.seen.lock().len(), 1);
    assert!(chat.seen.lock()[0][0].ends_with("accepted legacy followup"));
    assert!(!chat.seen.lock()[0][0].contains("deleted source body"));
    assert_eq!(intake_snapshot(&chat).inputs[0].state, "rejected");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_concurrent_socket_cannot_overtake_the_reserved_source_head() {
    let chat = Arc::new(intake_chat());
    let a = intake_socket(&chat).await;
    let b = intake_socket(&chat).await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let (release, wait) = std::sync::mpsc::channel();
    let wait = std::sync::Mutex::new(wait);
    let entered_hook = entered.clone();
    *chat.state.ws_conversations.intake.reservation_hook.lock() = Some(Arc::new(move |id| {
        if id == 10 {
            entered_hook.notify_one();
            wait.lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
        }
    }));
    let first_chat = chat.clone();
    let first = tokio::task::spawn_blocking(move || {
        receive_intake(&first_chat, &a, source_message(10, 0, "reserved first"))
    });
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    let (ack, overtaking) = receive_intake(
        &chat,
        &b,
        source_message(11, 11, "second after reservation"),
    );
    // Release before asserting, so a failed assertion cannot strand a worker.
    release.send(()).unwrap();
    assert_eq!(ack["source"]["cursor"], 12);
    assert!(
        overtaking.is_none(),
        "the reserved predecessor still owns source order"
    );
    let (_, first) = first.await.unwrap();
    *chat.state.ws_conversations.intake.reservation_hook.lock() = None;
    chat.gate.add_permits(2);
    run_intake(&chat, &b, first.unwrap()).await;
    assert_eq!(chat.seen.lock().len(), 2);
    assert!(chat.seen.lock()[0][0].ends_with("reserved first"));
    assert!(chat.seen.lock()[1][1].ends_with("second after reservation"));
}

#[tokio::test]
async fn surface_recovery_uses_the_accepted_input_and_preserves_legacy_payloads() {
    for original in [Some(ChatSurface::Telegram), None] {
        let mut chat = intake_chat();
        chat.scope.surface = original;
        let socket = intake_socket(&chat).await;
        let frame = source_message(1, 0, "persisted presentation");
        let (_, claim) = receive_intake(&chat, &socket, frame.clone());
        assert_eq!(claim.as_ref().unwrap().surface, original);
        let payload = intake_snapshot(&chat).inputs[0].payload.clone();
        drop(claim);
        drop(socket);
        reopen_intake(&mut chat);
        chat.scope.surface = Some(ChatSurface::Cli);
        let socket = intake_socket(&chat).await;
        let (ack, claim) = receive_intake(&chat, &socket, frame.clone());
        assert_eq!(ack["status"], "duplicate");
        assert_eq!(intake_snapshot(&chat).inputs[0].payload, payload);
        assert_eq!(claim.as_ref().unwrap().surface, original);
        let mut changed = frame;
        changed["content"] = json!("changed body");
        let (ack, _) = receive_intake(&chat, &socket, changed);
        assert_eq!(ack["type"], "error");
        chat.gate.add_permits(1);
        run_intake(&chat, &socket, claim.unwrap()).await;
        let prompt = chat.systems.lock()[0].clone();
        assert_eq!(prompt.contains("## Surface\n\nTelegram:"), original.is_some());
        assert!(!prompt.contains("## Surface\n\nCLI:"));
        assert_eq!(intake_snapshot(&chat).inputs[0].state, "done");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn racing_surface_registrations_return_duplicate_and_execute_the_winner() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use zeroclaw_api::bridge_intake::{BridgeInput, BridgeReceipt};
    use zeroclaw_api::model_provider::ChatMessage;
    use zeroclaw_infra::session_backend::SessionBackend;

    struct RacingBackend {
        inner: Arc<dyn SessionBackend>,
        records: AtomicUsize,
        barrier: std::sync::Barrier,
    }
    impl SessionBackend for RacingBackend {
        fn load(&self, key: &str) -> Vec<ChatMessage> {
            self.inner.load(key)
        }
        fn append(&self, key: &str, message: &ChatMessage) -> std::io::Result<()> {
            self.inner.append(key, message)
        }
        fn remove_last(&self, key: &str) -> std::io::Result<bool> {
            self.inner.remove_last(key)
        }
        fn list_sessions(&self) -> Vec<String> {
            self.inner.list_sessions()
        }
        fn bridge_resume(&self, source: &BridgeSource) -> std::io::Result<BridgeResume> {
            self.inner.bridge_resume(source)
        }
        fn bridge_receipt(
            &self,
            source: &BridgeSource,
            id: i64,
        ) -> std::io::Result<Option<BridgeReceipt>> {
            self.inner.bridge_receipt(source, id)
        }
        fn bridge_record(
            &self,
            source: &BridgeSource,
            input: &BridgeInput,
        ) -> std::io::Result<BridgeReceipt> {
            // Both first submissions reach persistence before either can win.
            // A canonical retry must bypass the barrier.
            if self.records.fetch_add(1, Ordering::SeqCst) < 2 {
                self.barrier.wait();
            }
            self.inner.bridge_record(source, input)
        }
        fn bridge_claim(&self, source: &BridgeSource, id: i64) -> std::io::Result<bool> {
            self.inner.bridge_claim(source, id)
        }
        fn bridge_finish(
            &self,
            source: &BridgeSource,
            id: i64,
            state: &str,
        ) -> std::io::Result<()> {
            self.inner.bridge_finish(source, id, state)
        }
    }
    let mut chat = intake_chat();
    chat.state.session_backend = Some(Arc::new(RacingBackend {
        inner: chat.state.session_backend.take().unwrap(),
        records: AtomicUsize::new(0),
        barrier: std::sync::Barrier::new(2),
    }));
    let chat = Arc::new(chat);
    let a = intake_socket(&chat).await;
    let b = intake_socket(&chat).await;
    let mut workers = Vec::new();
    for surface in [ChatSurface::Web, ChatSurface::Telegram] {
        let chat = chat.clone();
        let conversation = a.conversation.clone();
        workers.push(tokio::task::spawn_blocking(move || {
            let mut scope = chat.scope.clone();
            scope.surface = Some(surface);
            handle_client_text(
                &chat.state,
                &conversation,
                &scope,
                &source_message(1, 0, "racing presentation").to_string(),
            )
        }));
    }
    let mut accepted = 0;
    let mut duplicate = 0;
    let mut claims = Vec::new();
    for worker in workers {
        let (ack, claim) = tokio::time::timeout(Duration::from_secs(10), worker)
            .await
            .unwrap()
            .unwrap();
        let ack = ack.unwrap();
        assert_eq!(ack["durable"], true, "{ack}");
        assert_eq!(ack["source"]["cursor"], 2);
        match ack["status"].as_str() {
            Some("accepted") => accepted += 1,
            Some("duplicate") => duplicate += 1,
            _ => panic!("unexpected intake receipt: {ack}"),
        }
        claims.extend(claim);
    }
    assert_eq!((accepted, duplicate, claims.len()), (1, 1, 1));
    let snapshot = intake_snapshot(&chat);
    assert_eq!(snapshot.inputs.len(), 1);
    let payload = snapshot.inputs[0].payload.clone();
    let winner: ChatSurface =
        serde_json::from_value(serde_json::from_str::<Value>(&payload).unwrap()["surface"].clone())
            .unwrap();
    assert_eq!(claims[0].surface, Some(winner));

    // Reusing presentation metadata must never relax the immutable input checks.
    let mut changed = source_message(1, 0, "racing presentation");
    changed["attachments"] = json!([uuid::Uuid::new_v4().to_string()]);
    let (error, _) = receive_intake(&chat, &b, changed);
    assert_eq!(error["code"], "SOURCE_NOT_RECORDED");
    let (error, _) = receive_intake(&chat, &b, source_message(1, 1, "racing presentation"));
    assert_eq!(error["code"], "SOURCE_NOT_RECORDED");
    assert_eq!(intake_snapshot(&chat).inputs[0].payload, payload);

    chat.gate.add_permits(1);
    run_intake(&chat, &b, claims.pop().unwrap()).await;
    assert_eq!(chat.seen.lock().len(), 1);
    let prompt = chat.systems.lock()[0].clone();
    assert_eq!(
        prompt.contains("## Surface\n\nWeb:"),
        winner == ChatSurface::Web
    );
    assert_eq!(
        prompt.contains("## Surface\n\nTelegram:"),
        winner == ChatSurface::Telegram
    );
    assert_eq!(intake_snapshot(&chat).inputs[0].state, "done");
}

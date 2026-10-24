use super::*;
use serde_json::{Value, json};

fn control_chat() -> SharedChat {
    let mut chat = SharedChat::new();
    let subject = zeroclaw_config::tokens::hash_token("stop-test-bridge");
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
            "telegram".into(),
            zeroclaw_config::schema::GatewayBridgeConfig {
                token_hash: subject,
                sessions: vec!["shared".into()],
                ..Default::default()
            },
        );
    }
    chat
}

async fn socket(chat: &SharedChat) -> crate::ws_conversation::Subscription<WsSession> {
    chat.attach_key("gw_shared\u{1f}web").await
}

fn cancel(id: &str, target: &str) -> Value {
    json!({"type":"cancel","control_version":1,"id":id,"turn_id":target})
}

#[tokio::test]
async fn late_and_duplicate_scoped_stop_cannot_cancel_a_new_turn() {
    let chat = control_chat();
    let a = socket(&chat).await;
    let conv = &a.conversation;
    let Submitted::Start(old) = conv.submit("old".into()) else {
        panic!()
    };
    let old_stop = cancel("source-stop", &old.turn_id);
    conv.finish_turn(old.generation);
    let Submitted::Start(new) = conv.submit("new".into()) else {
        panic!()
    };
    for _ in 0..2 {
        let ack = chat.send(&a, old_stop.clone()).unwrap();
        assert_eq!(ack["type"], "cancel_ack");
        assert_eq!(ack["id"], "source-stop");
        assert_eq!(ack["turn_id"], old.turn_id);
        assert_eq!(ack["status"], "stale");
        assert!(!new.cancel.is_cancelled());
    }
    let other = chat.attach_key("gw_other\u{1f}web").await;
    let Submitted::Start(other_turn) = other.conversation.submit("other".into()) else {
        panic!()
    };
    assert_eq!(
        chat.send(&other, cancel("cross-session", &new.turn_id))
            .unwrap()["status"],
        "stale"
    );
    assert!(!other_turn.cancel.is_cancelled());
    conv.finish_turn(new.generation);
    assert_eq!(chat.send(&a, old_stop).unwrap()["status"], "no_active");
}

#[tokio::test]
async fn scoped_stop_requires_version_target_and_current_scoped_authority() {
    let mut chat = control_chat();
    let a = socket(&chat).await;
    let Submitted::Start(turn) = a.conversation.submit("running".into()) else {
        panic!()
    };
    let valid = cancel("stop", &turn.turn_id);
    for frame in [
        json!({"type":"cancel","id":"stop","turn_id":turn.turn_id}),
        json!({"type":"cancel","control_version":2,"id":"stop","turn_id":turn.turn_id}),
        json!({"type":"cancel","control_version":1,"id":"stop"}),
        json!({"type":"cancel","control_version":1,"id":"stop","turn_id":null}),
        json!({"type":"cancel","control_version":1,"id":"stop","turn_id":""}),
        json!({"type":"cancel","control_version":1,"id":"","turn_id":turn.turn_id}),
    ] {
        assert_eq!(chat.send(&a, frame).unwrap()["status"], "invalid");
        assert!(!turn.cancel.is_cancelled());
    }
    chat.scope.auth_subject = None;
    assert_eq!(
        chat.send(&a, valid.clone()).unwrap()["status"],
        "unauthorized"
    );
    chat.scope.auth_subject = Some(zeroclaw_config::tokens::hash_token(
        "stop-test-bridge",
    ));
    chat.scope.session_id = "outside-scope".into();
    assert_eq!(
        chat.send(&a, valid.clone()).unwrap()["status"],
        "unauthorized"
    );
    chat.scope.session_id = "shared".into();
    chat.state
        .config
        .write()
        .agents
        .get_mut("web")
        .unwrap()
        .enabled = false;
    assert_eq!(
        chat.send(&a, valid.clone()).unwrap()["status"],
        "unauthorized"
    );
    chat.state
        .config
        .write()
        .agents
        .get_mut("web")
        .unwrap()
        .enabled = true;
    chat.state.config.write().gateway.bridges.clear();
    assert_eq!(chat.send(&a, valid).unwrap()["status"], "unauthorized");
    assert!(!turn.cancel.is_cancelled());
    // Existing anonymous/local clients retain their explicit bare-cancel contract.
    assert!(chat.send(&a, json!({"type":"cancel"})).is_none());
    assert!(turn.cancel.is_cancelled());
}

#[tokio::test]
async fn cancel_receipt_is_requested_before_the_shared_turn_confirms_aborted() {
    let chat = control_chat();
    let mut a = socket(&chat).await;
    let mut b = socket(&chat).await;
    assert!(chat.send(&a, message("long work")).is_none());
    let snapshot = a.conversation.enable_turn_controls();
    let target = snapshot["turn_id"].as_str().unwrap();
    let ack = chat.send(&b, cancel("stop", target)).unwrap();
    assert_eq!(ack["status"], "requested");
    assert!(
        a.conversation.is_running(),
        "a request receipt does not release ownership"
    );
    assert_eq!(
        chat.send(&b, cancel("stop", target)).unwrap()["status"],
        "requested"
    );
    chat.gate.add_permits(1);
    for sub in [&mut a, &mut b] {
        let end = frames_until_end(sub).await.pop().unwrap();
        assert_eq!(end["type"], "aborted");
    }
}

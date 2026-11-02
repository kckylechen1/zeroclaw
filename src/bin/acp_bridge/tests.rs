use super::*;
fn frame(turn: &Turn, seq: u64, event: Value, terminal: bool) -> Value {
    json!({"type":"turn_event","version":1,"epoch":turn.epoch,"request_id":turn.request,"turn_id":"00000000-0000-4000-8000-000000000001","seq":seq,"event":event,"terminal":terminal,"stream_complete":true})
}
#[test]
fn withheld_tail_and_nonprefix_chunks_emit_only_authoritative_done() {
    for chunk in ["prefix", "wrong sanitized draft", ""] {
        let mut turn = Turn::new("epoch".into(), 1);
        let delta = frame(&turn, 1, json!({"type":"chunk","content":chunk}), false);
        assert!(turn.event(&delta).unwrap().is_none());
        assert!(turn.event(&delta).unwrap().is_none());
        let done = frame(
            &turn,
            2,
            json!({"type":"done","full_response":"prefix withheld tail"}),
            false,
        );
        assert!(turn.event(&done).unwrap().is_none());
        let end = frame(
            &turn,
            3,
            json!({"type":"turn_terminal","outcome":"done"}),
            true,
        );
        assert_eq!(
            turn.event(&end).unwrap(),
            Some(("end_turn", Some("prefix withheld tail".into())))
        );
        assert!(turn.event(&end).unwrap().is_none());
    }
}
#[test]
fn cancellation_requires_terminal_and_completion_race_keeps_actual_outcome() {
    let mut turn = Turn::new("epoch".into(), 1);
    turn.cancelled = true;
    let aborted = frame(&turn, 1, json!({"type":"aborted"}), false);
    assert!(turn.event(&aborted).unwrap().is_none());
    let end = frame(
        &turn,
        2,
        json!({"type":"turn_terminal","outcome":"aborted"}),
        true,
    );
    assert_eq!(turn.event(&end).unwrap(), Some(("cancelled", None)));
    let mut turn = Turn::new("epoch".into(), 1);
    turn.cancelled = true;
    let done = frame(&turn, 1, json!({"type":"done","full_response":""}), false);
    turn.event(&done).unwrap();
    let end = frame(
        &turn,
        2,
        json!({"type":"turn_terminal","outcome":"done"}),
        true,
    );
    assert_eq!(
        turn.event(&end).unwrap(),
        Some(("end_turn", Some(String::new())))
    );
}
#[test]
fn gap_wrong_identity_and_missing_done_cannot_report_success() {
    let mut turn = Turn::new("epoch".into(), 1);
    let gap = frame(
        &turn,
        2,
        json!({"type":"done","full_response":"wrong"}),
        false,
    );
    assert!(turn.event(&gap).is_err());
    let mut wrong = frame(&turn, 1, json!({"type":"chunk","content":"wrong"}), false);
    wrong["epoch"] = "other".into();
    assert!(turn.event(&wrong).is_err());
    let end = frame(
        &turn,
        1,
        json!({"type":"turn_terminal","outcome":"done"}),
        true,
    );
    assert!(turn.event(&end).is_err());
}
#[test]
fn text_profile_rejects_resource_media_missing_empty_and_oversize() {
    for value in [
        json!({}),
        json!({"prompt":[]}),
        json!({"prompt":[{"type":"text","text":" "}]}),
        json!({"prompt":[{"type":"resource_link","uri":"file:///secret"}]}),
        json!({"prompt":[{"type":"image","data":"x"}]}),
        json!({"prompt":[{"type":"text","text":"x".repeat(65537)}]}),
    ] {
        assert!(prompt(&value).is_err());
    }
    assert_eq!(
        prompt(&json!({"prompt":[{"type":"text","text":"one"},{"type":"text","text":"two"}]}))
            .unwrap(),
        "one\ntwo"
    );
}
#[cfg(unix)]
#[test]
fn credential_rejects_symlinks_permissions_and_malformed_tokens() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("token");
    std::fs::write(&path, "a".repeat(32)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(credential(&path).is_ok());
    let link = tmp.path().join("link");
    symlink(&path, &link).unwrap();
    assert!(credential(&link).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(credential(&path).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(&path, "bad token").unwrap();
    assert!(credential(&path).is_err());
}

#[test]
fn permission_provenance_never_turns_absence_into_human_choice() {
    let now = Instant::now();
    let permission = Permission {
        rpc: "session/approval".into(),
        request: "approval".into(),
        expires: now + IO,
    };
    let response = json!({"id":permission.rpc,"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}});
    assert_eq!(permission.decision(&response, false, now), Some("approve"));
    assert_eq!(permission.decision(&response, true, now), None);
    assert_eq!(permission.decision(&response, false, now + IO), None);
    for option in ["allow_always", "reject_always", "unknown"] {
        let mut value = response.clone();
        value["result"]["outcome"]["optionId"] = option.into();
        assert_eq!(permission.decision(&value, false, now), None);
    }
    let mut value = response.clone();
    value["result"]["outcome"]["optionId"] = "reject_once".into();
    assert_eq!(permission.decision(&value, false, now), Some("deny"));
    value["result"]["outcome"]["outcome"] = "cancelled".into();
    assert_eq!(permission.decision(&value, false, now), None);
    assert_eq!(
        permission.decision(&json!({"id":permission.rpc,"error":{}}), false, now),
        None
    );
    assert_eq!(permission.decision(&json!({}), false, now), None);
}

#[tokio::test]
async fn dispatcher_rejects_busy_before_actor_dequeue_and_old_cancel_cannot_cross_terminal() {
    let (tx, mut rx) = mpsc::channel(8);
    let admission = Arc::new(Admission::default());
    let task = zeroclaw_spawn::spawn!(std::future::pending::<()>());
    let session = Session {
        tx,
        task,
        admission: admission.clone(),
    };
    session.prompt(json!(1), "A".into()).unwrap();
    // A has not even been dequeued. B must be refused at the real dispatcher
    // boundary, not left for a select race against A's terminal.
    assert_eq!(
        session.prompt(json!(2), "B".into()),
        Err("ACP_SESSION_BUSY")
    );
    let Some(Command::Prompt(_, _, first)) = rx.recv().await else {
        panic!("missing A")
    };
    assert!(
        rx.try_recv().is_err(),
        "B must never be queued for execution"
    );
    let stale = admission.cancellation().unwrap();
    assert!(session.tx.try_send(stale).is_ok());
    // This is the actor's completion boundary after replying to A. A queued
    // cancellation remains associated with A even if a new C is admitted.
    admission.finish(first);
    session.prompt(json!(3), "C".into()).unwrap();
    let Some(Command::Cancel(captured)) = rx.recv().await else {
        panic!("missing cancellation")
    };
    let Some(Command::Prompt(_, _, current)) = rx.recv().await else {
        panic!("missing C")
    };
    assert_eq!(captured, first);
    assert_ne!(captured, current);
    let mut turn = Turn::new("epoch".into(), current);
    assert!(!turn.cancel(captured));
    assert!(!turn.cancelled);
    admission.finish(first);
    assert_eq!(
        session.prompt(json!(4), "D".into()),
        Err("ACP_SESSION_BUSY")
    );
    let Some(Command::Cancel(current_cancel)) = admission.cancellation() else {
        panic!("missing current cancellation")
    };
    assert!(turn.cancel(current_cancel));
    admission.poison();
    assert_eq!(
        session.prompt(json!(5), "E".into()),
        Err("ACP_SESSION_INCOMPLETE")
    );
    assert!(admission.cancellation().is_none());
}

// The callback trait fixes the error type; implement it directly rather than
// inferring that large error type on an otherwise infallible closure.
struct ChatProtocol;
impl tokio_tungstenite::tungstenite::handshake::server::Callback for ChatProtocol {
    fn on_request(
        self,
        _: &tokio_tungstenite::tungstenite::handshake::server::Request,
        mut response: tokio_tungstenite::tungstenite::handshake::server::Response,
    ) -> std::result::Result<
        tokio_tungstenite::tungstenite::handshake::server::Response,
        tokio_tungstenite::tungstenite::handshake::server::ErrorResponse,
    > {
        response.headers_mut().insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("zeroclaw.v1"),
        );
        Ok(response)
    }
}

#[tokio::test]
async fn production_connect_refuses_missing_or_denied_cancel_grant() {
    for grant in [Value::Null, Value::Bool(false)] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = zeroclaw_spawn::spawn!(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_hdr_async(stream, ChatProtocol)
                .await
                .unwrap();
            ws.send(Message::Text(
                json!({"type":"session_start","session_id":"acp-test"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
            let connect = ws.next().await.unwrap().unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(connect.to_text().unwrap()).unwrap()["type"],
                "connect"
            );
            let mut ack = json!({"type":"connected","owner_bridge_approval_once":true});
            if !grant.is_null() {
                ack["owner_bridge_cancel"] = grant;
            }
            ws.send(Message::Text(ack.to_string().into()))
                .await
                .unwrap();
        });
        let config = Config {
            settings: Settings {
                enabled: true,
                endpoint: format!("ws://{address}/ws/chat"),
                agent: "owner".into(),
                session_prefix: "acp-".into(),
                credential_file: PathBuf::new(),
            },
            token: "synthetic-test-token".into(),
        };
        assert!(matches!(
            connect(&config, "acp-test").await,
            Err("OWNER_BRIDGE_REQUIRED")
        ));
        server.await.unwrap();
    }
}

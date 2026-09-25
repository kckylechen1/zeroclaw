use super::*;
use tokio::net::TcpListener;

#[test]
fn chat_url_encodes_query_values_and_keeps_the_token_out() {
    let options = ConnectOptions {
        gateway: "ws://127.0.0.1:42617/".into(),
        agent: "my agent".into(),
        session_id: Some("a/b".into()),
        token: Some("zc_secret".into()),
    };
    let url = options.chat_url();
    assert_eq!(
        url,
        "ws://127.0.0.1:42617/ws/chat?agent=my%20agent&session_id=a%2Fb"
    );
    assert!(!url.contains("zc_secret"));
}

#[test]
fn frames_parse_and_unknown_ones_pass_through() {
    assert_eq!(
        Frame::parse(r#"{"type":"chunk","content":"hi"}"#).unwrap(),
        Frame::Chunk {
            content: "hi".into()
        }
    );
    let done =
        Frame::parse(r#"{"type":"done","id":"r1","full_response":"ok","tokens_used":3}"#).unwrap();
    assert!(done.is_terminal());
    assert_eq!(
        done,
        Frame::Done {
            id: Some("r1".into()),
            full_response: "ok".into()
        }
    );
    let other = Frame::parse(r#"{"type":"cron_result","output":"x"}"#).unwrap();
    assert!(matches!(other, Frame::Other(ref v) if v["type"] == "cron_result"));
    assert!(!other.is_terminal());
    assert!(Frame::parse("not json").is_err());
}

/// A gateway stand-in: checks the handshake and the client's frames, and
/// answers the way `/ws/chat` does.
async fn fake_gateway(listener: TcpListener) -> Vec<serde_json::Value> {
    let (stream, _) = listener.accept().await.unwrap();
    let mut seen_auth = None;
    let mut ws = tokio_tungstenite::accept_hdr_async(
        stream,
        |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
         mut resp: tokio_tungstenite::tungstenite::handshake::server::Response| {
            seen_auth = req
                .headers()
                .get(header::AUTHORIZATION)
                .map(|v| v.to_str().unwrap().to_string());
            assert!(req.uri().query().unwrap().contains("agent=main"));
            resp.headers_mut().insert(
                header::SEC_WEBSOCKET_PROTOCOL,
                HeaderValue::from_static(PROTOCOL),
            );
            Ok(resp)
        },
    )
    .await
    .unwrap();
    assert_eq!(seen_auth.as_deref(), Some("Bearer zc_token"));

    let send = |v: serde_json::Value| Message::Text(Utf8Bytes::from(v.to_string()));
    ws.send(send(serde_json::json!({
        "type": "session_start", "session_id": "s1", "resumed": true, "message_count": 4
    })))
    .await
    .unwrap();

    let mut received = Vec::new();
    while let Some(Ok(Message::Text(text))) = ws.next().await {
        let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
        received.push(frame.clone());
        match frame["type"].as_str().unwrap() {
            "connect" => {
                ws.send(send(serde_json::json!({ "type": "connected" })))
                    .await
                    .unwrap();
            }
            "message" => {
                let id = frame["id"].clone();
                for reply in [
                    serde_json::json!({ "type": "ack", "id": id, "status": "accepted", "turn": "started", "durable": true }),
                    serde_json::json!({ "type": "approval_request", "request_id": "ap1", "tool": "shell", "arguments_summary": "ls", "timeout_secs": 120 }),
                    serde_json::json!({ "type": "something_new" }),
                    serde_json::json!({ "type": "chunk", "content": "hello" }),
                    serde_json::json!({ "type": "done", "id": id, "full_response": "hello" }),
                ] {
                    ws.send(send(reply)).await.unwrap();
                }
            }
            "approval_response" | "cancel" => {}
            other => panic!("unexpected client frame {other}"),
        }
        if frame["type"] == "cancel" {
            break;
        }
    }
    received
}

#[tokio::test]
async fn a_client_attaches_sends_and_streams_a_turn() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(fake_gateway(listener));

    let mut client = Client::connect(&ConnectOptions {
        gateway: format!("ws://{addr}"),
        agent: "main".into(),
        session_id: None,
        token: Some("zc_token".into()),
    })
    .await
    .unwrap();
    assert_eq!(client.session().session_id, "s1");
    assert!(client.session().resumed);

    let id = client.send_message("hi").await.unwrap();
    let mut frames = Vec::new();
    while let Some(frame) = client.next_frame().await.unwrap() {
        if let Frame::ApprovalRequest { request_id, .. } = &frame {
            client
                .answer_approval(request_id, Decision::Always)
                .await
                .unwrap();
        }
        let end = frame.is_terminal();
        frames.push(frame);
        if end {
            break;
        }
    }
    client.cancel().await.unwrap();

    assert!(
        matches!(&frames[0], Frame::Ack { id: acked, turn: Some(t), .. } if *acked == id && t == "started")
    );
    assert!(matches!(&frames[2], Frame::Other(v) if v["type"] == "something_new"));
    assert_eq!(
        frames.last().unwrap(),
        &Frame::Done {
            id: Some(id.clone()),
            full_response: "hello".into()
        }
    );

    let received = server.await.unwrap();
    let types: Vec<_> = received
        .iter()
        .map(|f| f["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["connect", "message", "approval_response", "cancel"]);
    assert_eq!(received[1]["content"], "hi");
    assert_eq!(received[1]["id"], id);
    assert_eq!(received[2]["request_id"], "ap1");
    assert_eq!(received[2]["decision"], "always");
}

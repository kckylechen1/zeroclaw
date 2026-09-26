//! The bridge against a fake Telegram Bot API and a fake gateway.

// Test servers run on plain tokio tasks; there is no attribution span to
// carry.
#![allow(clippy::disallowed_methods)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Json, Path, State};
use axum::routing::post;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use zeroclaw_bridge_telegram::{BridgeConfig, run};
use zeroclaw_gateway_client::ConnectOptions;

const OWNER: i64 = 42;
const STRANGER: i64 = 7;
const WAIT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct FakeTelegram {
    updates: VecDeque<Value>,
    calls: Vec<(String, Value)>,
    next_update: i64,
    next_message: i64,
}

type Shared = Arc<Mutex<FakeTelegram>>;

async fn bot_api(
    State(tg): State<Shared>,
    Path((bot, method)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Json<Value> {
    assert_eq!(bot, "botTEST", "the token travels in the path");
    if method == "getUpdates" {
        for _ in 0..10 {
            let batch: Vec<Value> = tg.lock().unwrap().updates.drain(..).collect();
            if !batch.is_empty() {
                return Json(json!({ "ok": true, "result": batch }));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        return Json(json!({ "ok": true, "result": [] }));
    }
    let mut tg = tg.lock().unwrap();
    tg.calls.push((method.clone(), body));
    if method == "sendMessage" {
        tg.next_message += 1;
        let id = tg.next_message;
        return Json(json!({ "ok": true, "result": {
            "message_id": id, "chat": { "id": OWNER, "type": "private" }
        }}));
    }
    Json(json!({ "ok": true, "result": true }))
}

impl FakeTelegram {
    fn push(tg: &Shared, update: Value) {
        let mut tg = tg.lock().unwrap();
        tg.next_update += 1;
        let mut update = update;
        update["update_id"] = json!(tg.next_update);
        tg.updates.push_back(update);
    }

    fn text(tg: &Shared, from: i64, text: &str) {
        Self::push(
            tg,
            json!({ "message": {
                "message_id": 1000, "from": { "id": from },
                "chat": { "id": from, "type": "private" }, "text": text
            }}),
        );
    }

    fn press(tg: &Shared, from: i64, message_id: i64, data: &str) {
        Self::push(
            tg,
            json!({ "callback_query": {
                "id": format!("cb-{from}"), "from": { "id": from }, "data": data,
                "message": { "message_id": message_id,
                    "chat": { "id": OWNER, "type": "private" }, "text": "Allow shell?" }
            }}),
        );
    }

    /// Wait for a call matching `pred` and return its body.
    async fn wait_for(tg: &Shared, method: &str, pred: impl Fn(&Value) -> bool) -> Value {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            {
                let tg = tg.lock().unwrap();
                if let Some((_, body)) = tg.calls.iter().find(|(m, b)| m == method && pred(b)) {
                    return body.clone();
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "no {method} call matched; calls: {:?}",
                    tg.calls
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

type Ws = WebSocketStream<TcpStream>;

/// Accept the bridge's socket and complete the gateway side of the
/// handshake.
#[allow(clippy::result_large_err)] // the handshake callback's error type is tungstenite's
async fn attach(listener: &TcpListener) -> Ws {
    let (stream, _) = tokio::time::timeout(WAIT, listener.accept())
        .await
        .expect("the bridge connects")
        .unwrap();
    let mut ws = tokio_tungstenite::accept_hdr_async(
        stream,
        |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
         mut resp: tokio_tungstenite::tungstenite::handshake::server::Response| {
            let query = req.uri().query().unwrap_or_default().to_string();
            assert_eq!(query, "agent=assistant&session_id=main");
            assert_eq!(
                req.headers().get("authorization").unwrap(),
                "Bearer zc_token"
            );
            resp.headers_mut()
                .insert("sec-websocket-protocol", "zeroclaw.v1".parse().unwrap());
            Ok(resp)
        },
    )
    .await
    .unwrap();
    send(
        &mut ws,
        json!({ "type": "session_start", "session_id": "main", "resumed": true }),
    )
    .await;
    assert_eq!(recv(&mut ws).await["type"], "connect");
    send(&mut ws, json!({ "type": "connected" })).await;
    ws
}

async fn send(ws: &mut Ws, frame: Value) {
    ws.send(Message::Text(frame.to_string().into()))
        .await
        .unwrap();
}

async fn recv(ws: &mut Ws) -> Value {
    loop {
        let message = tokio::time::timeout(WAIT, ws.next())
            .await
            .expect("a frame from the bridge")
            .expect("the socket is open")
            .unwrap();
        if let Message::Text(text) = message {
            return serde_json::from_str(&text).unwrap();
        }
    }
}

#[tokio::test]
async fn the_bridge_relays_the_owners_chat_to_a_gateway_session() {
    let tg: Shared = Arc::default();
    let tg_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tg_addr = tg_listener.local_addr().unwrap();
    let app = Router::new()
        .route("/{bot}/{method}", post(bot_api))
        .with_state(tg.clone());
    tokio::spawn(async move { axum::serve(tg_listener, app).await.unwrap() });

    let gateway = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bridge = tokio::spawn(run(BridgeConfig {
        telegram_api: format!("http://{tg_addr}"),
        telegram_token: "TEST".into(),
        owner_id: OWNER,
        gateway: ConnectOptions {
            gateway: format!("ws://{}", gateway.local_addr().unwrap()),
            agent: "assistant".into(),
            session_id: Some("main".into()),
            token: Some("zc_token".into()),
        },
        poll_wait: Duration::from_secs(1),
    }));
    let mut ws = attach(&gateway).await;

    // A stranger is ignored; the owner's message becomes a turn that
    // streams into one Telegram message.
    FakeTelegram::text(&tg, STRANGER, "let me in");
    FakeTelegram::text(&tg, OWNER, "hello");
    let message = recv(&mut ws).await;
    assert_eq!(message["type"], "message");
    assert_eq!(message["content"], "hello");
    let id = message["id"].clone();
    for frame in [
        json!({ "type": "ack", "id": id, "status": "accepted", "turn": "started" }),
        json!({ "type": "chunk", "content": "Hel" }),
        json!({ "type": "chunk", "content": "lo" }),
        json!({ "type": "done", "id": id, "full_response": "Hello!" }),
    ] {
        send(&mut ws, frame).await;
    }
    let first = FakeTelegram::wait_for(&tg, "sendMessage", |b| b["text"] == "Hel").await;
    assert_eq!(first["chat_id"], OWNER);
    FakeTelegram::wait_for(&tg, "editMessageText", |b| {
        b["text"] == "Hello!" && b["message_id"] == 1
    })
    .await;
    FakeTelegram::wait_for(&tg, "sendChatAction", |b| b["action"] == "typing").await;

    // A message during a turn steers it.
    FakeTelegram::text(&tg, OWNER, "and this");
    let steer = recv(&mut ws).await;
    assert_eq!(steer["content"], "and this");
    send(
        &mut ws,
        json!({ "type": "ack", "id": steer["id"], "status": "accepted", "turn": "steered" }),
    )
    .await;
    FakeTelegram::wait_for(&tg, "sendMessage", |b| {
        b["text"] == "(added to the current turn)"
    })
    .await;

    // Approvals: buttons for the owner, a stranger's press is ignored.
    send(
        &mut ws,
        json!({ "type": "approval_request", "request_id": "ap1", "tool": "shell",
                "arguments_summary": "ls", "timeout_secs": 120 }),
    )
    .await;
    let prompt =
        FakeTelegram::wait_for(&tg, "sendMessage", |b| b.get("reply_markup").is_some()).await;
    let buttons = &prompt["reply_markup"]["inline_keyboard"][0];
    assert_eq!(buttons[0]["callback_data"], "ap:ap1:y");
    assert_eq!(buttons[1]["callback_data"], "ap:ap1:a");
    assert_eq!(buttons[2]["callback_data"], "ap:ap1:n");
    FakeTelegram::press(&tg, STRANGER, 3, "ap:ap1:n");
    FakeTelegram::press(&tg, OWNER, 3, "ap:ap1:y");
    let answer = recv(&mut ws).await;
    assert_eq!(answer["type"], "approval_response");
    assert_eq!(answer["request_id"], "ap1");
    assert_eq!(answer["decision"], "approve");
    FakeTelegram::wait_for(&tg, "answerCallbackQuery", |b| {
        b["callback_query_id"] == "cb-42"
    })
    .await;
    FakeTelegram::wait_for(&tg, "editMessageText", |b| {
        b["message_id"] == 3 && b["text"] == "Allow shell?\n\nApproved"
    })
    .await;

    // A turn another client started on the session is mirrored.
    send(
        &mut ws,
        json!({ "type": "chunk", "content": "from the laptop" }),
    )
    .await;
    send(
        &mut ws,
        json!({ "type": "done", "full_response": "from the laptop" }),
    )
    .await;
    FakeTelegram::wait_for(&tg, "sendMessage", |b| b["text"] == "from the laptop").await;

    // A message whose ack is lost goes again, under the same id, after
    // the bridge reconnects to the same session.
    FakeTelegram::text(&tg, OWNER, "again");
    let lost = recv(&mut ws).await;
    assert_eq!(lost["content"], "again");
    drop(ws);
    let mut ws = attach(&gateway).await;
    let resent = recv(&mut ws).await;
    assert_eq!(resent["type"], "message");
    assert_eq!(resent["content"], "again");
    assert_eq!(resent["id"], lost["id"]);
    send(
        &mut ws,
        json!({ "type": "ack", "id": resent["id"], "status": "duplicate", "state": "done" }),
    )
    .await;

    FakeTelegram::text(&tg, OWNER, "/cancel");
    assert_eq!(recv(&mut ws).await["type"], "cancel");

    // Nothing the stranger sent reached Telegram's owner chat or the gateway.
    let calls = tg.lock().unwrap().calls.clone();
    assert!(
        calls
            .iter()
            .all(|(_, body)| body.get("chat_id").is_none_or(|c| *c == OWNER))
    );
    assert!(
        !calls.iter().any(|(m, b)| m == "answerCallbackQuery"
            && b["callback_query_id"] == format!("cb-{STRANGER}"))
    );
    assert!(!bridge.is_finished(), "the bridge keeps running");
    bridge.abort();
}

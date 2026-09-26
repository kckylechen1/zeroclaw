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
use tokio::sync::mpsc;
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
    /// Refuse this many `sendMessage` calls with a retryable error.
    fail_sends: usize,
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
    // A refused send is not recorded: Telegram did not deliver it.
    if method == "sendMessage" && tg.fail_sends > 0 {
        tg.fail_sends -= 1;
        return Json(json!({ "ok": false, "error_code": 502, "description": "Bad Gateway" }));
    }
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

/// The gateway side: sockets the bridge opened, by path.
struct FakeGateway {
    chat: mpsc::Receiver<Ws>,
    control: mpsc::Receiver<Ws>,
}

/// Accept the bridge's sockets and sort them by path. Both must carry the
/// bridge token.
#[allow(clippy::result_large_err)] // the handshake callback's error type is tungstenite's
fn fake_gateway(listener: TcpListener) -> FakeGateway {
    let (chat_tx, chat) = mpsc::channel(4);
    let (control_tx, control) = mpsc::channel(4);
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut path = String::new();
            let ws = tokio_tungstenite::accept_hdr_async(
                stream,
                |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                 mut resp: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    path = req.uri().path().to_string();
                    assert_eq!(
                        req.headers().get("authorization").unwrap(),
                        "Bearer zc_token"
                    );
                    let protocol = if path == "/ws/bridge" {
                        assert_eq!(req.uri().query(), None, "identity comes from the token");
                        "zeroclaw.bridge.v1"
                    } else {
                        let query = req.uri().query().unwrap_or_default().to_string();
                        assert_eq!(query, "agent=assistant&session_id=main");
                        "zeroclaw.v1"
                    };
                    resp.headers_mut()
                        .insert("sec-websocket-protocol", protocol.parse().unwrap());
                    Ok(resp)
                },
            )
            .await
            .unwrap();
            let tx = if path == "/ws/bridge" {
                &control_tx
            } else {
                &chat_tx
            };
            if tx.send(ws).await.is_err() {
                return;
            }
        }
    });
    FakeGateway { chat, control }
}

/// Take the bridge's next chat socket and complete the gateway side of the
/// handshake.
async fn attach(gateway: &mut FakeGateway) -> Ws {
    let mut ws = tokio::time::timeout(WAIT, gateway.chat.recv())
        .await
        .expect("the bridge connects")
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

/// Take the bridge's next control socket and start it.
async fn control(gateway: &mut FakeGateway) -> Ws {
    let mut ws = tokio::time::timeout(WAIT, gateway.control.recv())
        .await
        .expect("the bridge opens its control socket")
        .unwrap();
    send(
        &mut ws,
        json!({ "type": "bridge_start", "bridge": "telegram" }),
    )
    .await;
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

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_url = format!("ws://{}", listener.local_addr().unwrap());
    let mut gateway = fake_gateway(listener);
    let bridge = tokio::spawn(run(BridgeConfig {
        telegram_api: format!("http://{tg_addr}"),
        telegram_token: "TEST".into(),
        owner_id: OWNER,
        gateway: ConnectOptions {
            gateway: gateway_url,
            agent: "assistant".into(),
            session_id: Some("main".into()),
            token: Some("zc_token".into()),
        },
        poll_wait: Duration::from_secs(1),
    }));
    let mut ws = attach(&mut gateway).await;

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
    let mut ws = attach(&mut gateway).await;
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

    // Proactive messages: sent to the owner's chat, acknowledged only after
    // Telegram accepted them, deduplicated by id.
    let mut ctl = control(&mut gateway).await;
    send(
        &mut ctl,
        json!({ "type": "deliver", "id": "d1", "to": OWNER.to_string(),
                "content": "cron says hi" }),
    )
    .await;
    FakeTelegram::wait_for(&tg, "sendMessage", |b| b["text"] == "cron says hi").await;
    assert_eq!(
        recv(&mut ctl).await,
        json!({ "type": "delivered", "id": "d1" })
    );
    // A replayed id is acknowledged without sending it twice; a message
    // for anyone but the owner is dropped (and acknowledged).
    send(
        &mut ctl,
        json!({ "type": "deliver", "id": "d1", "to": OWNER.to_string(),
                "content": "cron says hi" }),
    )
    .await;
    assert_eq!(recv(&mut ctl).await["id"], "d1");
    send(
        &mut ctl,
        json!({ "type": "deliver", "id": "d2", "to": STRANGER.to_string(), "content": "leak" }),
    )
    .await;
    assert_eq!(recv(&mut ctl).await["id"], "d2");
    send(
        &mut ctl,
        json!({ "type": "deliver", "id": "d3", "to": OWNER.to_string(),
                "thread_id": "12", "content": "in a topic" }),
    )
    .await;
    let topic = FakeTelegram::wait_for(&tg, "sendMessage", |b| b["text"] == "in a topic").await;
    assert_eq!(topic["message_thread_id"], 12);
    assert_eq!(recv(&mut ctl).await["id"], "d3");
    let sends = |text: &str| {
        tg.lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(m, b)| m == "sendMessage" && b["text"] == text)
            .count()
    };
    assert_eq!(sends("cron says hi"), 1);
    assert_eq!(sends("leak"), 0);

    // A retryable Telegram failure is not acknowledged: the bridge drops
    // the control socket and the gateway replays the message on the next
    // connection.
    tg.lock().unwrap().fail_sends = 1;
    let d4 = json!({ "type": "deliver", "id": "d4", "to": OWNER.to_string(),
                     "content": "after a failure" });
    send(&mut ctl, d4.clone()).await;
    let closed = tokio::time::timeout(WAIT, ctl.next())
        .await
        .expect("the bridge gives up the socket");
    assert!(
        !matches!(closed, Some(Ok(Message::Text(_)))),
        "no ack: {closed:?}"
    );
    let mut ctl = control(&mut gateway).await;
    send(&mut ctl, d4).await;
    assert_eq!(recv(&mut ctl).await["id"], "d4");
    assert_eq!(sends("after a failure"), 1);

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

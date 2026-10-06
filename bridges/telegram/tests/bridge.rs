//! The bridge against a fake Telegram Bot API and a fake gateway.

// Test servers run on plain tokio tasks; there is no attribution span to
// carry.
#![allow(clippy::disallowed_methods)]

use std::collections::{HashMap, VecDeque};
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
    if method == "getMe" {
        return Json(json!({"ok":true,"result":{"id":9000,"is_bot":true}}));
    }
    if method == "getUpdates" {
        {
            let mut tg = tg.lock().unwrap();
            tg.calls.push((method.clone(), body.clone()));
            let offset = body["offset"].as_i64().unwrap();
            tg.updates
                .retain(|update| update["update_id"].as_i64().unwrap() >= offset);
        }
        for _ in 0..10 {
            let batch: Vec<Value> = tg
                .lock()
                .unwrap()
                .updates
                .iter()
                .take(100)
                .cloned()
                .collect();
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

#[derive(Default)]
struct SourceLedger {
    received: HashMap<i64, Value>,
    receipts: HashMap<i64, Value>,
    cursor: i64,
}

struct Ws {
    socket: WebSocketStream<TcpStream>,
    ledger: Arc<Mutex<SourceLedger>>,
}

impl std::ops::Deref for Ws {
    type Target = WebSocketStream<TcpStream>;
    fn deref(&self) -> &Self::Target {
        &self.socket
    }
}

impl std::ops::DerefMut for Ws {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.socket
    }
}

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
    let ledger = Arc::new(Mutex::new(SourceLedger::default()));
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
            if tx
                .send(Ws {
                    socket: ws,
                    ledger: ledger.clone(),
                })
                .await
                .is_err()
            {
                return;
            }
        }
    });
    FakeGateway { chat, control }
}

/// Take the bridge's next chat socket and complete the gateway side of the
/// handshake.
async fn attach(gateway: &mut FakeGateway) -> Ws {
    attach_ready(gateway, Value::Null).await
}

async fn attach_ready(gateway: &mut FakeGateway, ready: Value) -> Ws {
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
    assert_eq!(
        recv_raw(&mut ws).await,
        json!({"type":"source_resume","source":"telegram:9000:42"})
    );
    let ready = if ready.is_null() {
        let ledger = ws.ledger.lock().unwrap();
        json!({"type":"source_ready","intake_version":1,"source":"telegram:9000:42","cursor":ledger.cursor,"unknown":[],"receipts":ledger.receipts.values().collect::<Vec<_>>()})
    } else {
        ready
    };
    send(&mut ws, ready).await;
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

async fn send(ws: &mut Ws, mut frame: Value) {
    if frame["type"] == "ack" {
        let update_id: i64 = frame["id"]
            .as_str()
            .unwrap()
            .rsplit(':')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let object = frame.as_object_mut().unwrap();
        object.entry("durable").or_insert(json!(true));
        object.entry("intake_version").or_insert(json!(1));
        object.entry("state").or_insert(json!("pending"));
        let cursor = {
            let mut ledger = ws.ledger.lock().unwrap();
            let input = ledger
                .received
                .get(&update_id)
                .expect("ACK needs a received source input")
                .clone();
            ledger.receipts.insert(update_id, json!({"update_id":update_id,"previous_cursor":input["source"]["previous_cursor"],"id":object["id"],"state":object["state"]}));
            while let Some(next) = ledger
                .receipts
                .values()
                .find(|row| row["previous_cursor"] == ledger.cursor)
                .and_then(|row| row["update_id"].as_i64())
            {
                ledger.cursor = next + 1;
            }
            ledger.cursor
        };
        object.entry("source").or_insert(
            json!({"namespace":"telegram:9000:42","update_id":update_id,"cursor":cursor}),
        );
    }
    ws.send(Message::Text(frame.to_string().into()))
        .await
        .unwrap();
}

async fn recv(ws: &mut Ws) -> Value {
    loop {
        let frame = recv_raw(ws).await;
        if frame["type"] != "source_disposition" {
            return frame;
        }
        send(
            ws,
            json!({"type":"ack","id":frame["id"],"status":"accepted","state":frame["disposition"]}),
        )
        .await;
    }
}

async fn ack_disposition(ws: &mut Ws) {
    let frame = recv_raw(ws).await;
    assert_eq!(frame["type"], "source_disposition");
    send(
        ws,
        json!({"type":"ack","id":frame["id"],"status":"accepted","state":frame["disposition"]}),
    )
    .await;
}

async fn recv_raw(ws: &mut Ws) -> Value {
    loop {
        let message = tokio::time::timeout(WAIT, ws.next())
            .await
            .expect("a frame from the bridge")
            .expect("the socket is open")
            .unwrap();
        if let Message::Text(text) = message {
            let frame: Value = serde_json::from_str(&text).unwrap();
            if let Some(update_id) = frame["source"]["update_id"].as_i64() {
                ws.ledger
                    .lock()
                    .unwrap()
                    .received
                    .insert(update_id, frame.clone());
            }
            return frame;
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

    // Durable source messages wait as their own inputs while another turn runs.
    FakeTelegram::text(&tg, OWNER, "and this");
    let steer = recv(&mut ws).await;
    assert_eq!(steer["content"], "and this");
    send(
        &mut ws,
        json!({ "type": "ack", "id": steer["id"], "status": "accepted", "state": "pending" }),
    )
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
    let prompt_id = tg.lock().unwrap().next_message;
    let buttons = &prompt["reply_markup"]["inline_keyboard"][0];
    assert_eq!(buttons[0]["callback_data"], "ap:ap1:y");
    assert_eq!(buttons[1]["callback_data"], "ap:ap1:a");
    assert_eq!(buttons[2]["callback_data"], "ap:ap1:n");
    FakeTelegram::press(&tg, STRANGER, prompt_id, "ap:ap1:n");
    FakeTelegram::press(&tg, OWNER, prompt_id, "ap:ap1:y");
    let answer = recv(&mut ws).await;
    assert_eq!(answer["type"], "approval_response");
    assert_eq!(answer["request_id"], "ap1");
    assert_eq!(answer["decision"], "approve");
    FakeTelegram::wait_for(&tg, "answerCallbackQuery", |b| {
        b["callback_query_id"] == "cb-42"
    })
    .await;
    FakeTelegram::wait_for(&tg, "editMessageText", |b| {
        b["message_id"] == prompt_id && b["text"] == "Allow shell?\n\nApproved"
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
    // for anyone but the owner is refused without a success receipt.
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
    let closed = tokio::time::timeout(WAIT, ctl.next())
        .await
        .expect("refusal closes the control socket");
    assert!(
        !matches!(closed, Some(Ok(Message::Text(_)))),
        "refusal must not acknowledge delivery: {closed:?}"
    );
    let mut ctl = control(&mut gateway).await;
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

    // An uncertain Telegram failure is not acknowledged. The real gateway
    // retains it as unknown and never replays it (covered by ws_bridge tests).
    // A later candidate still proceeds after reconnect.
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
    send(&mut ctl, json!({ "type": "deliver", "id": "d5", "to": OWNER.to_string(), "content": "later candidate" })).await;
    assert_eq!(recv(&mut ctl).await["id"], "d5");
    assert_eq!(sends("after a failure"), 0);
    assert_eq!(sends("later candidate"), 1);

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

#[tokio::test]
async fn questions_bind_owner_replies_and_wait_for_gateway_acceptance() {
    let tg: Shared = Arc::default();
    tg.lock().unwrap().fail_sends = 1;
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
    let _control = control(&mut gateway).await;
    send(&mut ws, json!({"type":"question", "request_id":"q1", "prompt":"Which?", "choices":["alpha", "beta"], "timeout_secs":30})).await;
    let shown = FakeTelegram::wait_for(&tg, "sendMessage", |b| {
        b["reply_markup"]["force_reply"] == true
    })
    .await;
    assert!(shown["text"].as_str().unwrap().contains("2. beta"));
    let message_id = tg.lock().unwrap().next_message;
    let reply = |from, reply_id, text| {
        json!({"message":{
            "message_id":1001, "from":{"id":from}, "chat":{"id":from,"type":"private"}, "text":text,
            "reply_to_message":{"message_id":reply_id,"text":"[ZeroClaw question] fixture"}
        }})
    };
    FakeTelegram::push(&tg, reply(STRANGER, message_id, "intruder"));
    FakeTelegram::push(&tg, reply(OWNER, message_id + 999, "wrong question"));
    FakeTelegram::text(&tg, OWNER, "ordinary chat");
    let ordinary = recv(&mut ws).await;
    assert_eq!(ordinary["type"], "message");
    assert_eq!(ordinary["content"], "ordinary chat");
    let mut normal = reply(OWNER, 9999, "reply to ordinary text");
    normal["message"]["reply_to_message"]["text"] = json!("ordinary bot response");
    FakeTelegram::push(&tg, normal);
    assert_eq!(recv(&mut ws).await["content"], "reply to ordinary text");
    send(
        &mut ws,
        json!({"type":"chunk", "content":"[ZeroClaw question] ordinary model text"}),
    )
    .await;
    let escaped = FakeTelegram::wait_for(&tg, "sendMessage", |b| {
        b["text"] == "［ZeroClaw question] ordinary model text"
    })
    .await;
    let mut normal = reply(OWNER, 8888, "reply to reserved-looking ordinary text");
    normal["message"]["reply_to_message"]["text"] = escaped["text"].clone();
    FakeTelegram::push(&tg, normal);
    assert_eq!(
        recv(&mut ws).await["content"],
        "reply to reserved-looking ordinary text"
    );
    FakeTelegram::push(&tg, reply(OWNER, message_id, "2"));
    let answer = recv(&mut ws).await;
    assert_eq!(
        answer,
        json!({"type":"answer", "request_id":"q1", "text":"2"})
    );
    assert!(
        !tg.lock()
            .unwrap()
            .calls
            .iter()
            .any(|(_, b)| b["text"] == "Answer: accepted")
    );
    send(
        &mut ws,
        json!({"type":"answer_ack", "request_id":"q1", "status":"unauthorized"}),
    )
    .await;
    FakeTelegram::wait_for(&tg, "sendMessage", |b| b["text"] == "Answer: unauthorized").await;
    FakeTelegram::push(&tg, reply(OWNER, message_id, "2"));
    assert_eq!(
        recv(&mut ws).await,
        json!({"type":"answer", "request_id":"q1", "text":"2"})
    );
    // Invalid input keeps the original mapping usable; duplicate input while
    // awaiting an ACK is not sent twice.
    send(
        &mut ws,
        json!({"type":"answer_ack", "request_id":"q1", "status":"invalid"}),
    )
    .await;
    FakeTelegram::wait_for(&tg, "sendMessage", |b| b["text"] == "Answer: invalid").await;
    FakeTelegram::push(&tg, reply(OWNER, message_id, "1"));
    assert_eq!(recv(&mut ws).await["text"], "1");
    // The first answer is now awaiting its answer_ack. Submit the duplicate
    // only here so recv() cannot consume both source dispositions together.
    FakeTelegram::push(&tg, reply(OWNER, message_id, "1"));
    ack_disposition(&mut ws).await;
    FakeTelegram::wait_for(&tg, "sendMessage", |b| {
        b["text"] == "The previous answer is awaiting confirmation"
    })
    .await;
    // The Gateway has not consumed the answer. Lost ACK/disconnection alone
    // must not replay it; a pending-question replay permits an explicit retry.
    ws.close(None).await.unwrap();
    let mut ws = attach(&mut gateway).await;
    send(&mut ws, json!({"type":"question", "request_id":"q1", "prompt":"Which?", "choices":["alpha", "beta"], "timeout_secs":20})).await;
    FakeTelegram::wait_for(&tg, "sendMessage", |b| {
        b["text"] == "The question is still pending; reply to it again"
    })
    .await;
    // Original ordinary messages have not been ACKed in this stand-in.
    for _ in 0..3 {
        assert_eq!(recv(&mut ws).await["type"], "message");
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(200), ws.next())
            .await
            .is_err(),
        "no automatic answer replay"
    );
    FakeTelegram::push(&tg, reply(OWNER, message_id, "1"));
    assert_eq!(
        recv(&mut ws).await,
        json!({"type":"answer", "request_id":"q1", "text":"1"})
    );
    send(
        &mut ws,
        json!({"type":"answer_ack", "request_id":"q1", "status":"accepted"}),
    )
    .await;
    FakeTelegram::wait_for(&tg, "sendMessage", |b| b["text"] == "Answer: accepted").await;
    FakeTelegram::push(&tg, reply(OWNER, message_id, "late"));
    ack_disposition(&mut ws).await;
    FakeTelegram::wait_for(&tg, "sendMessage", |b| {
        b["text"] == "This request is no longer known"
    })
    .await;
    assert!(
        tokio::time::timeout(Duration::from_millis(400), ws.next())
            .await
            .is_err()
    );
    bridge.abort();
}

async fn intake_fixture() -> (
    Shared,
    FakeGateway,
    BridgeConfig,
    tokio::task::JoinHandle<()>,
) {
    let tg: Shared = Arc::default();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/{bot}/{method}", post(bot_api))
        .with_state(tg.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_url = format!("ws://{}", listener.local_addr().unwrap());
    let gateway = fake_gateway(listener);
    let config = BridgeConfig {
        telegram_api: api,
        telegram_token: "TEST".into(),
        owner_id: OWNER,
        gateway: ConnectOptions {
            gateway: gateway_url,
            agent: "assistant".into(),
            session_id: Some("main".into()),
            token: Some("zc_token".into()),
        },
        poll_wait: Duration::from_millis(10),
    };
    (tg, gateway, config, server)
}

fn intake_ack(message: &Value, cursor: i64) -> Value {
    json!({"type":"ack","id":message["id"],"status":"accepted","durable":true,"intake_version":1,"state":"pending",
        "source":{"namespace":message["source"]["namespace"],"update_id":message["source"]["update_id"],"cursor":cursor}})
}

async fn raw_send(ws: &mut Ws, frame: Value) {
    ws.send(Message::Text(frame.to_string().into()))
        .await
        .unwrap();
}

fn no_source_confirmation(tg: &Shared) {
    assert!(
        tg.lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(method, _)| method == "getUpdates")
            .all(|(_, body)| body["offset"] == 0),
        "unaccepted source must stay on Telegram"
    );
}

#[tokio::test]
async fn only_matching_versioned_durable_acceptance_releases_the_source_cursor() {
    let (tg, mut gateway, config, server) = intake_fixture().await;
    let running = tokio::spawn(run(config));
    let mut ws = attach(&mut gateway).await;
    FakeTelegram::text(&tg, OWNER, "keep until accepted");
    let message = recv_raw(&mut ws).await;
    assert_eq!(message["id"], "tg:9000:42:1");
    assert_eq!(message["source"]["previous_cursor"], 0);
    let valid = intake_ack(&message, 2);
    let mut invalid = vec![
        json!({"type":"ack","id":message["id"],"status":"accepted"}),
        json!({"type":"ack","id":message["id"],"status":"accepted","durable":true}),
    ];
    for (field, value) in [
        ("durable", json!(false)),
        ("intake_version", json!(2)),
        ("status", json!("new_semantics")),
        ("state", json!("new_semantics")),
        ("id", json!("tg:9000:42:999")),
    ] {
        let mut frame = valid.clone();
        frame[field] = value;
        invalid.push(frame);
    }
    for (field, value) in [
        ("namespace", json!("telegram:9999:42")),
        ("update_id", json!(999)),
    ] {
        let mut frame = valid.clone();
        frame["source"][field] = value;
        invalid.push(frame);
    }
    for frame in invalid {
        raw_send(&mut ws, frame).await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        no_source_confirmation(&tg);
    }
    assert_eq!(tg.lock().unwrap().updates.len(), 1);
    raw_send(&mut ws, valid).await;
    FakeTelegram::wait_for(&tg, "getUpdates", |body| body["offset"] == 2).await;
    assert!(tg.lock().unwrap().updates.is_empty());
    running.abort();
    server.abort();
}

#[tokio::test]
async fn out_of_order_acceptance_keeps_the_gap_and_uses_the_gateway_cursor() {
    let (tg, mut gateway, config, server) = intake_fixture().await;
    let running = tokio::spawn(run(config));
    let mut ws = attach(&mut gateway).await;
    FakeTelegram::text(&tg, OWNER, "first");
    tg.lock().unwrap().next_update = 20; // Numeric gaps are valid source order.
    FakeTelegram::text(&tg, OWNER, "second");
    let first = recv_raw(&mut ws).await;
    let second = recv_raw(&mut ws).await;
    assert_eq!(second["source"]["previous_cursor"], 2);
    raw_send(&mut ws, intake_ack(&second, 0)).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    no_source_confirmation(&tg);
    assert_eq!(tg.lock().unwrap().updates.len(), 2);
    raw_send(&mut ws, intake_ack(&first, 22)).await;
    FakeTelegram::wait_for(&tg, "getUpdates", |body| body["offset"] == 22).await;
    assert!(tg.lock().unwrap().updates.is_empty());
    running.abort();
    server.abort();
}

#[tokio::test]
async fn bridge_restart_before_acceptance_replays_the_same_source_identity() {
    let (tg, mut gateway, config, server) = intake_fixture().await;
    let first_run = tokio::spawn(run(config.clone()));
    let mut first_socket = attach(&mut gateway).await;
    FakeTelegram::text(&tg, OWNER, "survive the process");
    let first = recv_raw(&mut first_socket).await;
    no_source_confirmation(&tg);
    first_run.abort();
    let _ = first_run.await;
    drop(first_socket);
    let second_run = tokio::spawn(run(config));
    let mut second_socket = attach(&mut gateway).await;
    let replay = recv_raw(&mut second_socket).await;
    assert_eq!(replay, first);
    raw_send(&mut second_socket, intake_ack(&replay, 2)).await;
    FakeTelegram::wait_for(&tg, "getUpdates", |body| body["offset"] == 2).await;
    second_run.abort();
    server.abort();
}

#[tokio::test]
async fn durable_control_disposition_precedes_execution_and_duplicate_does_not_repeat_it() {
    let (tg, mut gateway, config, server) = intake_fixture().await;
    let running = tokio::spawn(run(config.clone()));
    let mut ws = attach(&mut gateway).await;
    FakeTelegram::text(&tg, OWNER, "/cancel");
    let disposition = recv_raw(&mut ws).await;
    assert_eq!(disposition["type"], "source_disposition");
    assert_eq!(disposition["disposition"], "control");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), ws.next())
            .await
            .is_err()
    );
    let mut ack = intake_ack(&disposition, 0);
    ack["state"] = json!("control");
    raw_send(&mut ws, ack.clone()).await;
    assert_eq!(recv_raw(&mut ws).await["type"], "cancel");
    ack["status"] = json!("duplicate");
    raw_send(&mut ws, ack).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), ws.next())
            .await
            .is_err()
    );
    running.abort();
    let _ = running.await;
    drop(ws);

    // The process died before Telegram confirmed the control. Its persisted
    // source row is restored, so source replay must not cancel a new turn.
    let restarted = tokio::spawn(run(config));
    let ready = json!({"type":"source_ready","intake_version":1,"source":"telegram:9000:42","cursor":2,"unknown":[],
        "receipts":[{"update_id":1,"previous_cursor":0,"id":disposition["id"],"state":"control"}]});
    let mut ws = attach_ready(&mut gateway, ready).await;
    FakeTelegram::wait_for(&tg, "getUpdates", |body| body["offset"] == 2).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), ws.next())
            .await
            .is_err(),
        "a restored control cannot execute again"
    );
    restarted.abort();
    server.abort();
}

#[tokio::test]
async fn an_unsupported_source_contract_never_starts_telegram_polling() {
    let (tg, mut gateway, config, server) = intake_fixture().await;
    let running = tokio::spawn(run(config));
    let mut ws = attach_ready(&mut gateway, json!({"type":"source_ready","intake_version":2,"source":"telegram:9000:42","cursor":900,"unknown":[],"receipts":[]})).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !tg.lock()
            .unwrap()
            .calls
            .iter()
            .any(|(method, _)| method == "getUpdates")
    );
    // A socket close is transport cleanup, never an input confirmation.
    let _ = ws.close(None).await;
    running.abort();
    server.abort();
}

#[tokio::test]
async fn restored_attachment_receipts_are_checked_before_any_download() {
    let (tg, mut gateway, config, server) = intake_fixture().await;
    FakeTelegram::push(
        &tg,
        json!({"message":{"message_id":1,"from":{"id":OWNER},"chat":{"id":OWNER,"type":"private"},
        "document":{"file_id":"must-not-download","file_name":"note.txt","mime_type":"text/plain"}}}),
    );
    let running = tokio::spawn(run(config));
    let ready = json!({"type":"source_ready","intake_version":1,"source":"telegram:9000:42","cursor":2,"unknown":[],
        "receipts":[{"update_id":1,"previous_cursor":0,"id":"tg:9000:42:1","state":"pending"}]});
    let mut ws = attach_ready(&mut gateway, ready).await;
    FakeTelegram::wait_for(&tg, "getUpdates", |body| body["offset"] == 2).await;
    assert!(
        !tg.lock()
            .unwrap()
            .calls
            .iter()
            .any(|(method, _)| method == "getFile")
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), ws.next())
            .await
            .is_err()
    );
    running.abort();
    server.abort();
}

#[tokio::test]
async fn an_unknown_lower_update_holds_the_old_cursor_after_the_startup_probe() {
    let (tg, mut gateway, config, server) = intake_fixture().await;
    FakeTelegram::text(&tg, OWNER, "new source sequence after long idle");
    let running = tokio::spawn(run(config));
    let ready = json!({"type":"source_ready","intake_version":1,"source":"telegram:9000:42","cursor":900,"unknown":[],"receipts":[]});
    let mut ws = attach_ready(&mut gateway, ready).await;
    let input = recv_raw(&mut ws).await;
    assert_eq!(input["source"]["previous_cursor"], 900);
    raw_send(&mut ws, json!({"type":"error","id":input["id"],"code":"SOURCE_SEQUENCE","message":"unknown lower update"})).await;
    FakeTelegram::wait_for(&tg, "getUpdates", |body| body["offset"] == 1).await;
    assert!(
        tg.lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(method, _)| method == "getUpdates")
            .all(|(_, body)| body["offset"].as_i64().unwrap() <= 1)
    );
    assert_eq!(tg.lock().unwrap().updates.len(), 1);
    running.abort();
    server.abort();
}

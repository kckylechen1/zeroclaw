//! `/ws/bridge`: the control socket of a channel bridge.
//!
//! A bridge (a separate process relaying a messaging platform) keeps one
//! control socket open. The gateway sends proactive messages queued in the
//! bridge outbox (cron output, heartbeat alerts, the `notify` tool) as
//! `deliver {id, to, thread_id?, content}` frames, and the bridge answers
//! `delivered {id}` once the platform accepted the message; the row is then
//! deleted.
//!
//! - The bridge is identified by its token (`[gateway.bridges.<name>]`),
//!   never by a query parameter. The token is always required, whatever
//!   `require_pairing` says, and paired tokens are not accepted.
//! - On connect every unacknowledged row is replayed oldest first; rows
//!   queued later follow in the same order, so replay always precedes live
//!   delivery.
//! - One control socket per bridge: a new connection closes the old one.

use super::AppState;
use axum::{
    extract::{
        Query, State, WebSocketUpgrade,
        ws::{CloseFrame, Message, Utf8Bytes, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use zeroclaw_infra::bridge_outbox::BridgeOutbox;

/// The sub-protocol echoed when the client offers it.
const BRIDGE_PROTOCOL: &str = "zeroclaw.bridge.v1";
/// Close code sent to a control socket that a newer connection replaced.
const CLOSE_REPLACED: u16 = 4000;
/// How often a socket re-reads the outbox without a local wake-up; picks up
/// rows written by other processes (for example `zeroclaw cron run`).
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Rows read per outbox query.
const BATCH: usize = 100;

/// The live control socket of each bridge.
#[derive(Default)]
pub struct BridgeSockets {
    next_id: AtomicU64,
    live: Mutex<HashMap<String, (u64, CancellationToken)>>,
}

impl BridgeSockets {
    /// Register a new socket for `bridge`, closing the previous one.
    fn claim(&self, bridge: &str) -> (u64, CancellationToken) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let token = CancellationToken::new();
        if let Some((_, old)) = self
            .live
            .lock()
            .insert(bridge.to_string(), (id, token.clone()))
        {
            old.cancel();
        }
        (id, token)
    }

    /// Drop the registration if it is still this socket's.
    fn release(&self, bridge: &str, id: u64) {
        let mut live = self.live.lock();
        if live.get(bridge).is_some_and(|(current, _)| *current == id) {
            live.remove(bridge);
        }
    }
}

#[derive(serde::Deserialize)]
pub struct BridgeQuery {
    pub token: Option<String>,
}

/// GET /ws/bridge — a bridge's control socket.
pub async fn handle_ws_bridge(
    State(state): State<AppState>,
    Query(query): Query<BridgeQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let token = crate::ws::extract_ws_token(&headers, query.token.as_deref()).unwrap_or("");
    let (bridge, data_dir) = {
        let config = state.config.read();
        let bridge = config
            .gateway
            .bridge_for_token(token)
            .map(|(name, _)| name.to_string());
        (bridge, config.data_dir.clone())
    };
    let Some(bridge) = bridge else {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure),
            "bridge control socket refused: no bridge token matched"
        );
        return (
            StatusCode::UNAUTHORIZED,
            "Unauthorized: /ws/bridge needs a bridge token (zeroclaw gateway bridge add <name>)",
        )
            .into_response();
    };
    let outbox = match BridgeOutbox::shared(&data_dir) {
        Ok(outbox) => outbox,
        Err(e) => {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "bridge": bridge,
                        "error": format!("{e:#}"),
                    })),
                "bridge outbox unavailable"
            );
            return (StatusCode::SERVICE_UNAVAILABLE, "bridge outbox unavailable").into_response();
        }
    };
    let offers_protocol = headers
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|protos| protos.split(',').any(|p| p.trim() == BRIDGE_PROTOCOL));
    let ws = if offers_protocol {
        ws.protocols([BRIDGE_PROTOCOL])
    } else {
        ws
    };
    let sockets = Arc::clone(&state.bridge_sockets);
    ws.on_upgrade(move |socket| run_control_socket(socket, bridge, outbox, sockets))
        .into_response()
}

async fn run_control_socket(
    socket: WebSocket,
    bridge: String,
    outbox: Arc<BridgeOutbox>,
    sockets: Arc<BridgeSockets>,
) {
    let (conn_id, replaced) = sockets.claim(&bridge);
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Connect)
            .with_outcome(::zeroclaw_log::EventOutcome::Success)
            .with_attrs(::serde_json::json!({"bridge": bridge})),
        "bridge control socket attached"
    );
    let reason = serve(socket, &bridge, &outbox, &replaced).await;
    sockets.release(&bridge, conn_id);
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Disconnect)
            .with_attrs(::serde_json::json!({"bridge": bridge, "reason": reason})),
        "bridge control socket detached"
    );
}

/// Drive one control socket until it closes; returns why it ended.
async fn serve(
    socket: WebSocket,
    bridge: &str,
    outbox: &BridgeOutbox,
    replaced: &CancellationToken,
) -> &'static str {
    let (mut sender, mut receiver) = socket.split();
    // Subscribe before the first read so a write that lands between the
    // read and the wait still wakes this socket.
    let mut changed = outbox.subscribe();
    let _ = outbox.purge_expired();
    let start = serde_json::json!({ "type": "bridge_start", "bridge": bridge });
    if sender
        .send(Message::Text(start.to_string().into()))
        .await
        .is_err()
    {
        return "send failed";
    }

    // Highest row sent on this socket. Starting at 0 replays every row
    // not yet acknowledged, oldest first.
    let mut sent_through = 0_i64;
    let mut poll = tokio::time::interval(POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        match send_pending(&mut sender, bridge, outbox, &mut sent_through).await {
            Ok(()) => {}
            Err(reason) => return reason,
        }
        tokio::select! {
            () = replaced.cancelled() => {
                let _ = sender
                    .send(Message::Close(Some(CloseFrame {
                        code: CLOSE_REPLACED,
                        reason: Utf8Bytes::from_static("replaced by a newer connection"),
                    })))
                    .await;
                return "replaced";
            }
            message = receiver.next() => match message {
                Some(Ok(Message::Text(text))) => on_client_frame(bridge, outbox, &text),
                Some(Ok(Message::Close(_)) | Err(_)) | None => return "closed by the bridge",
                Some(Ok(_)) => {}
            },
            result = changed.changed() => {
                if result.is_err() {
                    return "outbox closed";
                }
            }
            _ = poll.tick() => {}
        }
    }
}

type Sender = futures_util::stream::SplitSink<WebSocket, Message>;

async fn send_pending(
    sender: &mut Sender,
    bridge: &str,
    outbox: &BridgeOutbox,
    sent_through: &mut i64,
) -> Result<(), &'static str> {
    loop {
        let rows = match outbox.pending(bridge, *sent_through, BATCH) {
            Ok(rows) => rows,
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Receive)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "bridge": bridge,
                            "error": format!("{e:#}"),
                        })),
                    "reading the bridge outbox failed"
                );
                return Ok(());
            }
        };
        let full = rows.len() == BATCH;
        for row in rows {
            let mut frame = serde_json::json!({
                "type": "deliver",
                "id": row.id,
                "to": row.to,
                "content": row.content,
            });
            if let Some(thread_id) = row.thread_id {
                frame["thread_id"] = serde_json::Value::String(thread_id);
            }
            if sender
                .send(Message::Text(frame.to_string().into()))
                .await
                .is_err()
            {
                return Err("send failed");
            }
            *sent_through = row.seq;
        }
        if !full {
            return Ok(());
        }
    }
}

fn on_client_frame(bridge: &str, outbox: &BridgeOutbox, text: &str) {
    let Ok(frame) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    if frame["type"] != "delivered" {
        return;
    }
    let Some(id) = frame["id"].as_str() else {
        return;
    };
    match outbox.ack(bridge, id) {
        Ok(acked) => {
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Send)
                    .with_outcome(::zeroclaw_log::EventOutcome::Success)
                    .with_attrs(::serde_json::json!({
                        "bridge": bridge,
                        "id": id,
                        "was_queued": acked,
                    })),
                "bridge acknowledged a delivery"
            );
        }
        Err(e) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Send)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "bridge": bridge,
                        "id": id,
                        "error": format!("{e:#}"),
                    })),
                "recording a bridge acknowledgement failed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use zeroclaw_config::pairing::PairingGuard;
    use zeroclaw_config::schema::GatewayBridgeConfig;
    use zeroclaw_gateway_client::{BridgeClient, Deliver, Rejected};

    const TOKEN: &str = "zcb_test_bridge_token";
    const WAIT: Duration = Duration::from_secs(10);

    fn bridge_state(tmp: &tempfile::TempDir, require_pairing: bool) -> AppState {
        let state = crate::tests::admin_paircode_state(tmp, require_pairing, false);
        state.config.write().gateway.bridges.insert(
            "tg".into(),
            GatewayBridgeConfig {
                token_hash: PairingGuard::token_hash(TOKEN),
                session_prefix: Some("tg:".into()),
                sessions: vec!["main".into()],
            },
        );
        state
    }

    async fn serve(state: AppState) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route("/ws/bridge", axum::routing::get(handle_ws_bridge))
            .route("/ws/chat", axum::routing::get(crate::ws::handle_ws_chat))
            .with_state(state);
        zeroclaw_spawn::spawn!(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        format!("ws://{addr}")
    }

    fn outbox(state: &AppState) -> Arc<BridgeOutbox> {
        BridgeOutbox::shared(&state.config.read().data_dir).unwrap()
    }

    async fn next(client: &mut BridgeClient) -> Option<Deliver> {
        tokio::time::timeout(WAIT, client.next_deliver())
            .await
            .expect("a frame in time")
            .unwrap()
    }

    async fn refusal(gateway: &str, token: &str) -> u16 {
        let err = BridgeClient::connect(gateway, token).await.err().unwrap();
        err.downcast_ref::<Rejected>().expect("refused").status
    }

    #[tokio::test]
    async fn the_control_socket_needs_a_bridge_token_even_without_pairing() {
        let tmp = tempfile::tempdir().unwrap();
        let gateway = serve(bridge_state(&tmp, false)).await;
        assert_eq!(refusal(&gateway, "").await, 401);
        assert_eq!(refusal(&gateway, "zc_paired_or_anything").await, 401);
        let client = BridgeClient::connect(&gateway, TOKEN).await.unwrap();
        assert_eq!(client.bridge(), "tg");
    }

    #[tokio::test]
    async fn queued_rows_replay_before_live_ones_and_acks_delete_them() {
        let tmp = tempfile::tempdir().unwrap();
        let state = bridge_state(&tmp, true);
        let outbox = outbox(&state);
        let gateway = serve(state).await;
        outbox.enqueue("tg", "42", None, "queued 1").unwrap();
        outbox.enqueue("other", "7", None, "not ours").unwrap();
        outbox.enqueue("tg", "42", Some("5"), "queued 2").unwrap();

        let mut client = BridgeClient::connect(&gateway, TOKEN).await.unwrap();
        let first = next(&mut client).await.unwrap();
        // Written while the replay is in flight: it must come after it.
        outbox.enqueue("tg", "43", None, "live").unwrap();
        let second = next(&mut client).await.unwrap();
        let third = next(&mut client).await.unwrap();
        assert_eq!(
            [&first.content, &second.content, &third.content],
            ["queued 1", "queued 2", "live"]
        );
        assert_eq!(second.thread_id.as_deref(), Some("5"));
        assert_eq!(third.to, "43");

        client.ack(&first.id).await.unwrap();
        client.ack(&third.id).await.unwrap();
        // Acks are processed in order with later frames; wait for them.
        let deadline = tokio::time::Instant::now() + WAIT;
        while outbox.len("tg").unwrap() != 1 {
            assert!(tokio::time::Instant::now() < deadline, "acks applied");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        client.close().await.unwrap();

        // The unacked row is replayed on the next connection.
        let mut again = BridgeClient::connect(&gateway, TOKEN).await.unwrap();
        let replayed = next(&mut again).await.unwrap();
        assert_eq!(replayed.id, second.id);
        assert_eq!(outbox.len("other").unwrap(), 1);
    }

    #[tokio::test]
    async fn a_new_control_socket_replaces_the_old_one() {
        let tmp = tempfile::tempdir().unwrap();
        let state = bridge_state(&tmp, true);
        let outbox = outbox(&state);
        let gateway = serve(state).await;

        let mut old = BridgeClient::connect(&gateway, TOKEN).await.unwrap();
        let mut new = BridgeClient::connect(&gateway, TOKEN).await.unwrap();
        assert!(next(&mut old).await.is_none(), "the old socket is closed");
        outbox.enqueue("tg", "42", None, "to the new one").unwrap();
        assert_eq!(next(&mut new).await.unwrap().content, "to the new one");
    }

    #[tokio::test]
    async fn a_bridge_token_opens_only_its_chat_sessions() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let tmp = tempfile::tempdir().unwrap();
        let gateway = serve(bridge_state(&tmp, true)).await;
        let addr = gateway.trim_start_matches("ws://").to_string();
        let status = |query: &'static str, token: &'static str| {
            let addr = addr.clone();
            async move {
                let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
                let request = format!(
                    "GET /ws/chat?{query} HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\n\
                     Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
                     Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                     Authorization: Bearer {token}\r\n\r\n"
                );
                stream.write_all(request.as_bytes()).await.unwrap();
                let mut buf = vec![0u8; 1024];
                let n = stream.read(&mut buf).await.unwrap();
                String::from_utf8_lossy(&buf[..n])
                    .split_whitespace()
                    .nth(1)
                    .and_then(|code| code.parse::<u16>().ok())
                    .unwrap_or(0)
            }
        };
        // In scope: auth passes and the request fails later, on the agent.
        assert_eq!(status("agent=nobody&session_id=main", TOKEN).await, 400);
        assert_eq!(status("agent=nobody&session_id=tg:-100", TOKEN).await, 400);
        // Out of scope, or no session at all: refused.
        assert_eq!(status("agent=nobody&session_id=work", TOKEN).await, 403);
        assert_eq!(status("agent=nobody", TOKEN).await, 403);
        // Neither a bridge nor a paired token.
        assert_eq!(
            status("agent=nobody&session_id=main", "zcb_nope").await,
            401
        );
    }
}

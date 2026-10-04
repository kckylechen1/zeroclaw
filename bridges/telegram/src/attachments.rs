//! HTTP upload transport only. No payload ledger or runtime dependencies.
use crate::telegram::{Api, File};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use zeroclaw_gateway_client::ConnectOptions;

pub async fn upload(api: &Api, options: &ConnectOptions, file: &File) -> Result<String> {
    let token = options
        .token
        .as_deref()
        .filter(|t| !t.is_empty())
        .context("attachments need a Gateway token")?;
    let session = options
        .session_id
        .as_deref()
        .context("attachments need an explicit session")?;
    let mut url = reqwest::Url::parse(&options.gateway).context("invalid Gateway URL")?;
    let scheme = match url.scheme() {
        "ws" => "http",
        "wss" => "https",
        _ => bail!("unsupported Gateway URL scheme"),
    };
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("invalid Gateway scheme"))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("unsupported Gateway URL authority or query");
    }
    let path = format!("{}/api/attachments", url.path().trim_end_matches('/'));
    url.set_path(&path);
    let name = file.file_name.as_deref().unwrap_or("attachment");
    let mime = file
        .mime_type
        .as_deref()
        .filter(|m| *m != "application/octet-stream")
        .unwrap_or_else(|| match name.rsplit('.').next().unwrap_or("") {
            "txt" => "text/plain",
            "md" => "text/markdown",
            "csv" => "text/csv",
            "json" => "application/json",
            _ => "application/octet-stream",
        });
    if !matches!(
        mime,
        "text/plain"
            | "text/markdown"
            | "text/csv"
            | "application/json"
            | "image/jpeg"
            | "image/png"
            | "image/gif"
            | "image/webp"
    ) {
        bail!("this attachment type is not supported");
    }
    let bytes = api.download_file(file).await?;
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()?
        .post(url)
        .query(&[
            ("session_id", session),
            ("agent", options.agent.as_str()),
            ("file_name", name),
        ])
        .bearer_auth(token)
        .header("content-type", mime)
        .timeout(std::time::Duration::from_secs(20))
        .body(bytes)
        .send()
        .await
        .map_err(reqwest::Error::without_url)?;
    if response.status() != reqwest::StatusCode::CREATED {
        bail!(
            "Gateway attachment upload refused ({})",
            response.status().as_u16()
        );
    }
    let body = crate::telegram::bounded_bytes(response, 4096).await?;
    let info: Value = serde_json::from_slice(&body).context("invalid attachment receipt")?;
    let id = info["id"]
        .as_str()
        .filter(|id| id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
        .context("missing attachment handle")?;
    Ok(id.to_string())
}

#[cfg(test)]
mod tests {
    // Fixture servers have no production attribution context.
    #![allow(clippy::disallowed_methods)]
    use super::*;
    use axum::{
        Json, Router,
        body::Bytes,
        extract::{
            State, WebSocketUpgrade,
            ws::{Message, WebSocket},
        },
        http::{HeaderMap, StatusCode},
        routing::{get, post},
    };
    use serde_json::json;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tokio::sync::{Semaphore, mpsc};
    struct Telegram {
        updates: Mutex<VecDeque<Value>>,
        downloads: std::sync::atomic::AtomicUsize,
        gate: Semaphore,
    }
    async fn bot(
        State(tg): State<Arc<Telegram>>,
        axum::extract::Path(method): axum::extract::Path<String>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        match method.as_str() {
            "getUpdates" => {
                tokio::time::sleep(Duration::from_millis(10)).await;
                Json(
                    json!({"ok":true,"result":tg.updates.lock().unwrap().drain(..).collect::<Vec<_>>() }),
                )
            }
            "getFile" => {
                assert_eq!(body["file_id"], "synthetic-file");
                tg.downloads
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Json(
                    json!({"ok":true,"result":{"file_id":"synthetic-file","file_path":"documents/file.txt"}}),
                )
            }
            "sendMessage" => Json(json!({"ok":true,"result":{"message_id":12}})),
            _ => Json(json!({"ok":true,"result":true})),
        }
    }
    async fn file(State(tg): State<Arc<Telegram>>) -> &'static str {
        tg.gate.acquire().await.unwrap().forget();
        "hello attachment"
    }
    async fn upload_http(
        headers: HeaderMap,
        axum::extract::Query(query): axum::extract::Query<
            std::collections::HashMap<String, String>,
        >,
        body: Bytes,
    ) -> (StatusCode, Json<Value>) {
        assert_eq!(headers["authorization"], "Bearer synthetic-token");
        assert_eq!(body, "hello attachment");
        assert_eq!(query["session_id"], "main");
        assert_eq!(query["agent"], "assistant");
        assert_eq!(query["file_name"], "note.txt");
        (
            StatusCode::CREATED,
            Json(json!({"id":"a0000000-0000-4000-8000-000000000001"})),
        )
    }
    async fn chat(
        State(tx): State<mpsc::Sender<WebSocket>>,
        ws: WebSocketUpgrade,
    ) -> axum::response::Response {
        ws.protocols(["zeroclaw.v1"])
            .on_upgrade(move |socket| async move {
                tx.send(socket).await.unwrap();
            })
    }
    async fn value(ws: &mut WebSocket) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match ws.recv().await {
                    Some(Ok(Message::Text(text))) => return serde_json::from_str(&text).unwrap(),
                    Some(Ok(Message::Close(_))) | None => {
                        panic!("fixture socket closed before expected frame")
                    }
                    Some(Err(e)) => panic!("fixture socket failed: {e}"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap()
    }
    async fn send(ws: &mut WebSocket, v: Value) {
        ws.send(Message::Text(v.to_string().into())).await.unwrap();
    }
    fn push(tg: &Telegram, from: i64, file: bool) {
        let mut message =
            json!({"message_id":10,"from":{"id":from},"chat":{"id":from,"type":"private"}});
        if file {
            message["document"] =
                json!({"file_id":"synthetic-file","file_name":"note.txt","mime_type":"text/plain"});
            message["caption"] = json!("read this");
        } else {
            message["text"] = json!("/cancel");
        }
        tg.updates.lock().unwrap().push_back(json!({"update_id":tg.downloads.load(std::sync::atomic::Ordering::Relaxed) as i64 + 1,"message":message}));
    }
    #[tokio::test]
    async fn owner_file_upload_uses_http_handles_and_does_not_block_cancel() {
        let tg = Arc::new(Telegram {
            updates: Mutex::default(),
            downloads: std::sync::atomic::AtomicUsize::new(0),
            gate: Semaphore::new(0),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/botTEST/{method}", post(bot))
            .route("/file/botTEST/documents/file.txt", get(file))
            .with_state(tg.clone());
        let bot_server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway = format!("ws://{}", listener.local_addr().unwrap());
        let (tx, mut rx) = mpsc::channel(1);
        let app = Router::new()
            .route("/ws/chat", get(chat))
            .route("/api/attachments", post(upload_http))
            .with_state(tx);
        let gw_server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let running = tokio::spawn(crate::run(crate::BridgeConfig {
            telegram_token: "TEST".into(),
            owner_id: 42,
            telegram_api: api_url,
            gateway: ConnectOptions {
                gateway,
                agent: "assistant".into(),
                session_id: Some("main".into()),
                token: Some("synthetic-token".into()),
            },
            poll_wait: Duration::from_millis(10),
        }));
        let mut ws = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        send(&mut ws, json!({"type":"session_start","session_id":"main"})).await;
        assert_eq!(value(&mut ws).await["type"], "connect");
        send(&mut ws, json!({"type":"connected"})).await;
        push(&tg, 7, true);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(tg.downloads.load(std::sync::atomic::Ordering::Relaxed), 0);
        push(&tg, 42, true);
        tokio::time::timeout(Duration::from_secs(5), async {
            while tg.downloads.load(std::sync::atomic::Ordering::Relaxed) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        push(&tg, 42, false);
        assert_eq!(
            value(&mut ws).await["type"],
            "cancel",
            "upload must not block controls"
        );
        tg.updates.lock().unwrap().push_back(json!({"update_id":3,"message":{"message_id":11,"from":{"id":42},"chat":{"id":42,"type":"private"},"text":"follow-up after file"}}));
        tokio::time::sleep(Duration::from_millis(50)).await;
        tg.gate.add_permits(1);
        let frame = value(&mut ws).await;
        assert_eq!(frame["content"], "read this");
        assert_eq!(
            frame["attachments"][0],
            "a0000000-0000-4000-8000-000000000001"
        );
        assert!(!frame.to_string().contains("hello attachment"));
        let following = value(&mut ws).await;
        assert_eq!(following["content"], "follow-up after file");
        send(
            &mut ws,
            json!({"type":"ack","id":following["id"],"status":"accepted"}),
        )
        .await;
        // Lose the ACK: reconnect must retain both request identity and
        // the exact HTTP handles, without downloading/uploading again.
        ws.send(Message::Close(None)).await.unwrap();
        drop(ws);
        let mut again = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        send(
            &mut again,
            json!({"type":"session_start","session_id":"main"}),
        )
        .await;
        assert_eq!(value(&mut again).await["type"], "connect");
        send(&mut again, json!({"type":"connected"})).await;
        let replay = value(&mut again).await;
        assert_eq!(replay, frame);
        assert_eq!(tg.downloads.load(std::sync::atomic::Ordering::Relaxed), 1);
        send(
            &mut again,
            json!({"type":"error","id":frame["id"],"code":"UNAUTHORIZED_ATTACHMENTS","message":"fixture rejection"}),
        )
        .await;
        // A definitive rejection ends retries. Reconnecting after permission
        // restoration must not silently submit that refused attachment again.
        again.send(Message::Close(None)).await.unwrap();
        drop(again);
        let mut third = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        send(
            &mut third,
            json!({"type":"session_start","session_id":"main"}),
        )
        .await;
        assert_eq!(value(&mut third).await["type"], "connect");
        send(&mut third, json!({"type":"connected"})).await;
        push(&tg, 42, false);
        assert_eq!(
            value(&mut third).await["type"],
            "cancel",
            "definitively rejected requests must not be replayed"
        );
        assert_eq!(tg.downloads.load(std::sync::atomic::Ordering::Relaxed), 1);
        running.abort();
        bot_server.abort();
        gw_server.abort();
    }
}

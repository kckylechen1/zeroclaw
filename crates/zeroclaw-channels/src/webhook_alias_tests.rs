use super::*;
use tokio::sync::mpsc;

pub(crate) struct TestServer {
    #[cfg(feature = "heavy-tests")]
    pub(crate) channel: std::sync::Arc<WebhookChannel>,
    port: u16,
    rx: mpsc::Receiver<ChannelMessage>,
    task: tokio::task::JoinHandle<Result<()>>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) async fn spawn_webhook(alias: &str, secret: &str) -> TestServer {
    // Exercise the public listen path without changing production code for the
    // RED run. Reserve an ephemeral port, then release it for listen() to bind.
    // A competing bind fails this fixture; it never silently selects a new port.
    let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = reservation.local_addr().unwrap().port();
    let channel = WebhookChannel::new(
        alias.into(),
        port,
        None,
        None,
        None,
        None,
        Some(secret.into()),
        None,
        None,
        None,
    );
    let (tx, rx) = mpsc::channel(8);
    drop(reservation);
    let channel = std::sync::Arc::new(channel);
    let listener_channel = std::sync::Arc::clone(&channel);
    let task = zeroclaw_spawn::spawn!(async move { listener_channel.listen(tx).await });
    let server = TestServer {
        #[cfg(feature = "heavy-tests")]
        channel,
        port,
        rx,
        task,
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            assert!(
                !server.task.is_finished(),
                "webhook listener exited before readiness"
            );
            if tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("webhook listener readiness timeout");
    server
}

pub(crate) async fn post_signed(server: &TestServer, secret: &str, body: serde_json::Value) -> u16 {
    use hmac::{Hmac, Mac};
    let body = serde_json::to_vec(&body).unwrap();
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(&body);
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
        .post(format!("http://127.0.0.1:{}/webhook", server.port))
        .header("content-type", "application/json")
        .header(
            "x-webhook-signature",
            hex::encode(mac.finalize().into_bytes()),
        )
        .body(body)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

pub(crate) async fn receive(server: &mut TestServer) -> ChannelMessage {
    tokio::time::timeout(Duration::from_secs(5), server.rx.recv())
        .await
        .expect("message timeout")
        .expect("message stream closed")
}

#[tokio::test]
async fn inbound_message_carries_configured_alias() {
    let mut server = spawn_webhook("support", "fixture-secret").await;
    assert_eq!(
        post_signed(
            &server,
            "fixture-secret",
            serde_json::json!({
                "sender": "same-sender", "content": "hello", "channel_alias": "spoofed"
            })
        )
        .await,
        200
    );
    let msg = receive(&mut server).await;
    assert_eq!(msg.channel_alias.as_deref(), Some("support"));
    assert_eq!(msg.channel, "webhook");
    assert_eq!(msg.sender, "same-sender");
    assert_eq!(msg.reply_target, "same-sender");
    assert_eq!(msg.thread_ts, None);
    assert_eq!(msg.content, "hello");
    assert_eq!(msg.id, "webhook_0");
}

#[tokio::test]
async fn two_webhook_aliases_keep_distinct_conversation_routes() {
    let mut alpha = spawn_webhook("alpha", "alpha-secret").await;
    let mut beta = spawn_webhook("beta", "beta-secret").await;
    // Use identical sender, thread and payload: only the listener alias can
    // distinguish these sessions. The old None collapsed both to bare webhook.
    let payload = serde_json::json!({"sender": "same-sender", "content": "hello", "thread_id": "same-thread"});
    assert_eq!(
        post_signed(&alpha, "alpha-secret", payload.clone()).await,
        200
    );
    assert_eq!(
        post_signed(&beta, "beta-secret", payload.clone()).await,
        200
    );
    let alpha_msg = receive(&mut alpha).await;
    let beta_msg = receive(&mut beta).await;
    let alpha_key = crate::orchestrator::conversation_history_key(&alpha_msg);
    let beta_key = crate::orchestrator::conversation_history_key(&beta_msg);
    assert_ne!(
        alpha_key, beta_key,
        "listener aliases must not share history/session routing"
    );
    assert_eq!(alpha_msg.channel_alias.as_deref(), Some("alpha"));
    assert_eq!(beta_msg.channel_alias.as_deref(), Some("beta"));
    assert_eq!(alpha_msg.reply_target, "same-thread");
    assert_eq!(beta_msg.reply_target, "same-thread");
    assert_eq!(alpha_msg.thread_ts.as_deref(), Some("same-thread"));
    // A second delivery stays on the same production route for its instance.
    assert_eq!(post_signed(&beta, "beta-secret", payload).await, 200);
    assert_eq!(
        crate::orchestrator::conversation_history_key(&receive(&mut beta).await),
        beta_key
    );
    assert!(alpha.rx.try_recv().is_err());
}

#[tokio::test]
async fn alias_delivery_keeps_authentication_and_empty_content_guards() {
    let mut server = spawn_webhook("private", "private-secret").await;
    let payload = serde_json::json!({"sender": "same-sender", "content": "hello"});
    assert_eq!(post_signed(&server, "wrong-secret", payload).await, 401);
    assert_eq!(
        post_signed(
            &server,
            "private-secret",
            serde_json::json!({"sender": "same-sender", "content": ""})
        )
        .await,
        400
    );
    assert!(server.rx.try_recv().is_err());
}

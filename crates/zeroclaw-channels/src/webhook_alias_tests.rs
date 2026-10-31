use super::*;
use tokio::sync::mpsc;

pub(crate) struct TestServer {
    #[cfg(feature = "heavy-tests")]
    pub(crate) channel: std::sync::Arc<dyn Channel>,
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
    spawn_webhook_with_replay(alias, secret, false, None, None).await
}

async fn spawn_webhook_with_replay(
    alias: &str,
    secret: &str,
    disable: bool,
    window: Option<u64>,
    capacity: Option<usize>,
) -> TestServer {
    // Exercise configuration -> production factory -> public HTTP listener.
    // Reserve an ephemeral port, then release it for listen() to bind.
    // A competing bind fails this fixture; it never silently selects a new port.
    let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = reservation.local_addr().unwrap().port();
    let mut config = zeroclaw_config::schema::Config::default();
    config.agents.clear();
    config.channels.webhook.insert(
        alias.into(),
        zeroclaw_config::schema::WebhookConfig {
            enabled: true,
            port,
            secret: Some(secret.into()),
            dangerously_disable_replay_protection: disable,
            replay_window_secs: window,
            replay_cache_capacity: capacity,
            ..Default::default()
        },
    );
    let channel = crate::orchestrator::build_channel_map(&config)
        .remove(&format!("webhook.{alias}"))
        .expect("configured webhook");
    let (tx, rx) = mpsc::channel(8);
    drop(reservation);
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

fn signed_request(
    server: &TestServer,
    secret: &str,
    body: serde_json::Value,
    timestamp: Option<&str>,
    id: Option<&str>,
    legacy: bool,
) -> reqwest::RequestBuilder {
    use hmac::{Hmac, Mac};
    let body = serde_json::to_vec(&body).unwrap();
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    if !legacy {
        mac.update(b"v1\n");
        mac.update(timestamp.unwrap_or_default().as_bytes());
        mac.update(b"\n");
        mac.update(id.unwrap_or_default().as_bytes());
        mac.update(b"\n");
    }
    mac.update(&body);
    let mut request = reqwest::Client::builder()
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
        .body(body);
    if let Some(ts) = timestamp {
        request = request.header("x-webhook-timestamp", ts);
    }
    if let Some(id) = id {
        request = request.header("x-webhook-id", id);
    }
    request
}

pub(crate) async fn post_signed(server: &TestServer, secret: &str, body: serde_json::Value) -> u16 {
    signed_request(
        server,
        secret,
        body,
        Some(&Utc::now().timestamp().to_string()),
        Some(&uuid::Uuid::new_v4().to_string()),
        false,
    )
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

#[tokio::test]
async fn replay_contract_legacy_missing_timestamp_is_rejected() {
    let mut server = spawn_webhook("replay", "fixture-secret").await;
    let response = signed_request(
        &server,
        "fixture-secret",
        replay_payload(),
        None,
        None,
        true,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(
        response.status(),
        401,
        "legacy body-only signatures must require migration"
    );
    let error: serde_json::Value = response.json().await.unwrap();
    assert_eq!(error["error"], "webhook_replay_migration_required");
    assert_eq!(error["required_headers"][0], "X-Webhook-Timestamp");
    assert!(
        error["signature_format"]
            .as_str()
            .unwrap()
            .contains("HMAC-SHA256")
    );
    assert!(server.rx.try_recv().is_err());
}

fn replay_payload() -> serde_json::Value {
    serde_json::json!({"sender": "s", "content": "hello"})
}

#[tokio::test]
async fn replay_contract_accepts_once_and_rejects_replay() {
    let mut server = spawn_webhook("replay", "fixture-secret").await;
    let ts = Utc::now().timestamp().to_string();
    for expected in [200, 409, 409] {
        let response = signed_request(
            &server,
            "fixture-secret",
            replay_payload(),
            Some(&ts),
            Some("event-1"),
            false,
        )
        .send()
        .await
        .unwrap();
        assert_eq!(response.status(), expected);
    }
    assert_eq!(receive(&mut server).await.content, "hello");
    assert!(server.rx.try_recv().is_err());
}

#[tokio::test]
async fn replay_contract_rejects_expired_future_missing_and_malformed_headers() {
    let mut server = spawn_webhook("replay", "fixture-secret").await;
    let now = Utc::now().timestamp();
    for ts in [
        (now - 301).to_string(),
        (now + 600).to_string(),
        "".into(),
        "-1".into(),
        "18446744073709551616".into(),
        "garbage".into(),
    ] {
        let response = signed_request(
            &server,
            "fixture-secret",
            replay_payload(),
            Some(&ts),
            Some("event-1"),
            false,
        )
        .send()
        .await
        .unwrap();
        assert_eq!(response.status(), 401, "timestamp: {ts}");
    }
    let ts = now.to_string();
    for id in [None, Some(""), Some("has space")] {
        let response = signed_request(
            &server,
            "fixture-secret",
            replay_payload(),
            Some(&ts),
            id,
            false,
        )
        .send()
        .await
        .unwrap();
        assert_eq!(response.status(), 401);
    }
    let long_id = "x".repeat(129);
    assert_eq!(
        signed_request(
            &server,
            "fixture-secret",
            replay_payload(),
            Some(&ts),
            Some(&long_id),
            false
        )
        .send()
        .await
        .unwrap()
        .status(),
        401
    );
    assert_eq!(
        signed_request(
            &server,
            "fixture-secret",
            replay_payload(),
            None,
            Some("event-1"),
            false
        )
        .send()
        .await
        .unwrap()
        .status(),
        401
    );
    assert!(server.rx.try_recv().is_err());
}

#[tokio::test]
async fn replay_contract_headers_are_authenticated_and_failures_do_not_poison_cache() {
    let mut server = spawn_webhook("replay", "fixture-secret").await;
    let ts = Utc::now().timestamp().to_string();
    // Replacing either signed header cannot reuse the original MAC.
    for (key, value) in [
        ("x-webhook-id", "other".to_string()),
        (
            "x-webhook-timestamp",
            (Utc::now().timestamp() + 1).to_string(),
        ),
    ] {
        let mut request = signed_request(
            &server,
            "fixture-secret",
            replay_payload(),
            Some(&ts),
            Some("event-1"),
            false,
        )
        .build()
        .unwrap();
        request.headers_mut().insert(key, value.parse().unwrap());
        assert_eq!(
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .execute(request)
                .await
                .unwrap()
                .status(),
            401
        );
    }
    // Body-only MACs with fresh headers are also invalid.
    assert_eq!(
        signed_request(
            &server,
            "fixture-secret",
            replay_payload(),
            Some(&ts),
            Some("event-1"),
            true
        )
        .send()
        .await
        .unwrap()
        .status(),
        401
    );
    assert_eq!(
        signed_request(
            &server,
            "wrong",
            replay_payload(),
            Some(&ts),
            Some("event-1"),
            false
        )
        .send()
        .await
        .unwrap()
        .status(),
        401
    );
    assert_eq!(
        signed_request(
            &server,
            "fixture-secret",
            replay_payload(),
            Some(&ts),
            Some("event-1"),
            false
        )
        .send()
        .await
        .unwrap()
        .status(),
        200
    );
    receive(&mut server).await;
    assert!(server.rx.try_recv().is_err());
}

#[tokio::test]
async fn replay_contract_concurrent_duplicate_delivers_once() {
    let mut server = spawn_webhook("replay", "fixture-secret").await;
    let ts = Utc::now().timestamp().to_string();
    let a = signed_request(
        &server,
        "fixture-secret",
        replay_payload(),
        Some(&ts),
        Some("event-1"),
        false,
    );
    let b = signed_request(
        &server,
        "fixture-secret",
        replay_payload(),
        Some(&ts),
        Some("event-1"),
        false,
    );
    let (a, b) = tokio::join!(a.send(), b.send());
    let mut statuses = [a.unwrap().status().as_u16(), b.unwrap().status().as_u16()];
    statuses.sort();
    assert_eq!(statuses, [200, 409]);
    receive(&mut server).await;
    assert!(server.rx.try_recv().is_err());
}

#[tokio::test]
async fn replay_contract_cache_saturation_does_not_evict_live_events() {
    let mut server =
        spawn_webhook_with_replay("replay", "fixture-secret", false, None, Some(1)).await;
    let ts = Utc::now().timestamp().to_string();
    for (id, status) in [("one", 200), ("two", 503), ("one", 409)] {
        assert_eq!(
            signed_request(
                &server,
                "fixture-secret",
                replay_payload(),
                Some(&ts),
                Some(id),
                false
            )
            .send()
            .await
            .unwrap()
            .status(),
            status
        );
    }
    receive(&mut server).await;
    assert!(server.rx.try_recv().is_err());
}

#[tokio::test]
async fn replay_contract_custom_window_and_explicit_legacy_opt_out() {
    let mut strict =
        spawn_webhook_with_replay("strict", "fixture-secret", false, Some(10), None).await;
    let ts = (Utc::now().timestamp() - 30).to_string();
    assert_eq!(
        signed_request(
            &strict,
            "fixture-secret",
            replay_payload(),
            Some(&ts),
            Some("one"),
            false
        )
        .send()
        .await
        .unwrap()
        .status(),
        401
    );
    assert!(strict.rx.try_recv().is_err());
    let mut legacy = spawn_webhook_with_replay("legacy", "fixture-secret", true, None, None).await;
    for _ in 0..2 {
        assert_eq!(
            signed_request(
                &legacy,
                "fixture-secret",
                replay_payload(),
                None,
                None,
                true
            )
            .send()
            .await
            .unwrap()
            .status(),
            200
        );
        receive(&mut legacy).await;
    }
    assert_eq!(
        signed_request(&legacy, "wrong", replay_payload(), None, None, true)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
}

#[tokio::test]
async fn replay_contract_rejects_duplicate_headers_without_poisoning_cache() {
    let mut server = spawn_webhook("replay", "fixture-secret").await;
    let ts = Utc::now().timestamp().to_string();
    for header in ["x-webhook-id", "x-webhook-timestamp", "x-webhook-signature"] {
        let mut request = signed_request(
            &server,
            "fixture-secret",
            replay_payload(),
            Some(&ts),
            Some("event"),
            false,
        )
        .build()
        .unwrap();
        let value = request.headers().get(header).unwrap().clone();
        request.headers_mut().append(header, value);
        assert_eq!(
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .execute(request)
                .await
                .unwrap()
                .status(),
            401
        );
    }
    assert_eq!(
        signed_request(
            &server,
            "fixture-secret",
            replay_payload(),
            Some(&ts),
            Some("event"),
            false
        )
        .send()
        .await
        .unwrap()
        .status(),
        200
    );
    receive(&mut server).await;
    assert!(server.rx.try_recv().is_err());
}

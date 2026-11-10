use super::*;

#[tokio::test]
async fn done_reports_vision_request_route_without_changing_next_turn() {
    use axum::{Json, Router, response::IntoResponse, routing::post};

    let requests = Arc::new(parking_lot::Mutex::new(Vec::<serde_json::Value>::new()));
    let captured = Arc::clone(&requests);
    let app = Router::new().route("/chat/completions", post(move |Json(body): Json<serde_json::Value>| {
        let captured = Arc::clone(&captured);
        async move {
            captured.lock().push(body.clone());
            if body["stream"] == true {
                let delta = serde_json::json!({"choices":[{"index":0,"delta":{"content":"vision reply"},"finish_reason":null}]});
                let end = serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
                ([("content-type", "text/event-stream")], format!("data: {delta}\n\ndata: {end}\n\ndata: [DONE]\n\n")).into_response()
            } else {
                Json(serde_json::json!({"choices":[{"message":{"content":"vision reply"},"finish_reason":"stop"}]})).into_response()
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = zeroclaw_spawn::spawn!(async move { axum::serve(listener, app).await.unwrap() });

    let mut chat = SharedChat::new();
    let mut config = zeroclaw_config::schema::Config {
        data_dir: chat._tmp.path().to_path_buf(),
        ..Default::default()
    };
    let entry = config.providers.models.ensure("custom", "vision").unwrap();
    entry.uri = Some(format!("http://{address}"));
    entry.model = Some("vision-model".into());
    entry.api_key = Some("completion-fixture".into());
    entry.vision = Some(true);
    config.multimodal.vision_model_provider = Some("custom.vision".into());
    chat.owner_config = Some(Arc::new(config));
    let mut a = chat.attach().await;
    let mut events = chat.state.event_tx.subscribe();
    chat.send(
        &a,
        message("inspect [IMAGE:data:image/png;base64,iVBORw0KGgo=]"),
    );

    let done = frames_until_end(&mut a).await.pop().unwrap();
    assert_eq!(done["type"], "done");
    assert_eq!(done["full_response"], "vision reply");
    assert_eq!(
        done["provider"], "scripted",
        "configured field stays compatible"
    );
    assert_eq!(done["provider_ref"], "custom.vision");
    assert_eq!(done["model"], "vision-model");
    assert!(
        done["input_tokens"].is_null(),
        "route metadata must not depend on usage"
    );
    assert_eq!(requests.lock().len(), 1);
    assert_eq!(requests.lock()[0]["model"], "vision-model");
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        if event["type"] == "agent_end" {
            assert_eq!(event["model_provider"], "scripted");
            assert_eq!(event["provider_ref"], "custom.vision");
            assert_eq!(event["model"], "vision-model");
            break;
        }
    }

    {
        let mut session = a.conversation.agent.lock().await;
        assert_eq!(session.agent.attribution_fields().2, "test-model");
        assert_eq!(
            session.agent.side_query_handle().snapshot().model,
            "test-model"
        );
        session.agent.clear_history();
    }
    chat.gate.add_permits(1);
    chat.send(&a, message("ordinary text"));
    let next = frames_until_end(&mut a).await.pop().unwrap();
    assert_eq!(next["type"], "done");
    assert_eq!(next["provider"], "scripted");
    assert_eq!(next["provider_ref"], "scripted");
    assert_eq!(next["model"], "test-model");
    assert_eq!(
        requests.lock().len(),
        1,
        "next text turn uses the default provider"
    );
    server.abort();
}

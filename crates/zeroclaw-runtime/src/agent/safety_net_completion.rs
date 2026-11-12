//! Completion metadata follows runtime dispatch without becoming Agent state.

use super::*;
use axum::{Json, Router, response::IntoResponse, routing::post};
use zeroclaw_config::schema::{Config, ModelRouteConfig};

type Models = Arc<parking_lot::Mutex<Vec<String>>>;

struct ModelCapture {
    models: Models,
    scripted: ScriptedProvider,
}

impl zeroclaw_api::attribution::Attributable for ModelCapture {
    fn role(&self) -> zeroclaw_api::attribution::Role {
        zeroclaw_api::attribution::Attributable::role(&self.scripted)
    }
    fn alias(&self) -> &str {
        "completion-fixture"
    }
}

#[async_trait]
impl ModelProvider for ModelCapture {
    async fn chat_with_system(
        &self,
        system: Option<&str>,
        message: &str,
        model: &str,
        temperature: Option<f64>,
    ) -> Result<String> {
        zeroclaw_providers::ProviderDispatch::from_ref(&self.scripted)
            .chat_with_system(system, message, model, temperature)
            .await
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: Option<f64>,
    ) -> Result<ChatResponse> {
        self.models.lock().push(model.into());
        zeroclaw_providers::ProviderDispatch::from_ref(&self.scripted)
            .chat(request, model, temperature)
            .await
    }
}

fn capture(models: &Models, responses: Vec<ChatResponse>) -> Box<dyn ModelProvider> {
    Box::new(ModelCapture {
        models: Arc::clone(models),
        scripted: ScriptedProvider::new(responses),
    })
}

async fn complete(agent: &mut Agent, message: &str) -> StreamedTurnSuccess {
    let (tx, _rx) = mpsc::channel(256);
    agent
        .turn_streamed_with_steering_state(message, tx, None, None)
        .await
        .expect("fixture turn completes")
}

#[tokio::test]
async fn completion_route_classification_cache_and_next_turn() {
    let base_models = Models::default();
    let fast_models = Models::default();
    let route = ModelRouteConfig {
        hint: "fast".into(),
        model_provider: "custom.fast".into(),
        model: "fast-model".into(),
        ..Default::default()
    };
    let provider = zeroclaw_providers::router::RouterModelProvider::new(
        "custom.text",
        vec![
            ("custom.text".into(), capture(&base_models, vec![])),
            (
                "custom.fast".into(),
                capture(&fast_models, vec![text_response("fast reply")]),
            ),
        ],
        vec![(
            route.hint.clone(),
            zeroclaw_providers::router::Route {
                provider_name: route.model_provider.clone(),
                model: route.model.clone(),
            },
        )],
    );
    let mut agent = build_agent(Box::new(provider), vec![]);
    agent.model_provider_name = "custom.text".into();
    agent.model_name = "text-model".into();
    agent.temperature = Some(0.0);
    agent.available_hints = vec!["fast".into()];
    agent
        .route_model_by_hint
        .insert("fast".into(), "fast-model".into());
    agent.classification_config = zeroclaw_config::schema::QueryClassificationConfig {
        enabled: true,
        rules: vec![zeroclaw_config::schema::ClassificationRule {
            hint: "fast".into(),
            keywords: vec!["quick".into()],
            ..Default::default()
        }],
    };
    agent.provider_switch_config = Some(ProviderSwitchConfig {
        config: Some(Arc::new(Config {
            data_dir: agent._workspace.path().to_path_buf(),
            model_routes: vec![route],
            ..Default::default()
        })),
    });
    agent.response_cache = Some(Arc::new(
        zeroclaw_memory::response_cache::ResponseCache::new(agent._workspace.path(), 60, 10)
            .unwrap(),
    ));
    let fixed = chrono::DateTime::parse_from_rfc3339("2026-01-01T12:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Local);
    agent.turn_datetime = Some(Arc::new(move || fixed));

    for _ in 0..2 {
        agent.clear_history();
        let outcome = complete(&mut agent, "quick question").await;
        assert_eq!(outcome.response, "fast reply");
        assert_eq!(outcome.provider_name, "custom.fast");
        assert_eq!(outcome.model, "fast-model");
    }
    assert_eq!(fast_models.lock().as_slice(), ["fast-model"]);
    assert!(
        base_models.lock().is_empty(),
        "second turn must hit the cache"
    );

    agent.clear_history();
    let outcome = complete(&mut agent, "ordinary question").await;
    assert_eq!(outcome.provider_name, "custom.text");
    assert_eq!(outcome.model, "text-model");
    assert_eq!(base_models.lock().as_slice(), ["text-model"]);
    assert_eq!(agent.side_query.snapshot().model, "text-model");
}

/// A loopback OpenAI-compatible fixture, including the streaming tool shape.
async fn endpoint(message: serde_json::Value) -> (String, Models, tokio::task::JoinHandle<()>) {
    let models = Models::default();
    let captured = Arc::clone(&models);
    let app = Router::new().route("/chat/completions", post(move |Json(body): Json<serde_json::Value>| {
        let captured = Arc::clone(&captured);
        let mut message = message.clone();
        async move {
            captured.lock().push(body["model"].as_str().unwrap().into());
            if body["stream"] == true {
                if let Some(calls) = message["tool_calls"].as_array_mut() {
                    for (index, call) in calls.iter_mut().enumerate() {
                        call["index"] = index.into();
                    }
                }
                let reason = if message.get("tool_calls").is_some() { "tool_calls" } else { "stop" };
                let delta = serde_json::json!({"choices":[{"index":0,"delta":message,"finish_reason":null}]});
                let end = serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}]});
                ([("content-type", "text/event-stream")], format!("data: {delta}\n\ndata: {end}\n\ndata: [DONE]\n\n")).into_response()
            } else {
                Json(serde_json::json!({"choices":[{"message":message,"finish_reason":"stop"}]})).into_response()
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = zeroclaw_spawn::spawn!(async move { axum::serve(listener, app).await.unwrap() });
    (url, models, server)
}

fn endpoint_config(workspace: &Path, alias: &str, url: String, model: &str) -> Config {
    let mut config = Config {
        data_dir: workspace.to_path_buf(),
        ..Default::default()
    };
    config.reliability.provider_retries = 0;
    let entry = config.providers.models.ensure("custom", alias).unwrap();
    entry.uri = Some(url);
    entry.model = Some(model.into());
    entry.api_key = Some("completion-fixture".into());
    entry.vision = Some(true);
    config
}

struct SwitchAndSteer {
    steering: mpsc::Sender<String>,
}

zeroclaw_api::tool_attribution!(
    SwitchAndSteer,
    ::zeroclaw_api::attribution::ToolKind::Plugin
);

#[async_trait]
impl Tool for SwitchAndSteer {
    fn name(&self) -> &str {
        "switch_and_steer"
    }
    fn description(&self) -> &str {
        "switch fixture route and queue steering"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    async fn execute(&self, _: serde_json::Value) -> Result<crate::tools::ToolResult> {
        let pending = crate::agent::turn::current_model_switch_state()?;
        *pending.lock().unwrap() = Some(("custom.next".into(), "next-model".into()));
        self.steering
            .send("continue with this correction".into())
            .await?;
        Ok(crate::tools::ToolResult {
            success: true,
            output: "switch queued".into(),
            error: None,
        })
    }
}

#[tokio::test]
async fn completion_route_follows_model_switch_and_steering() {
    let (url, next_models, server) =
        endpoint(serde_json::json!({"role":"assistant","content":"switched reply"})).await;
    let base_models = Models::default();
    let (steering_tx, mut steering_rx) = mpsc::channel(4);
    let mut agent = build_agent(
        capture(
            &base_models,
            vec![tool_response(vec![tool_call(
                "switch-1",
                "switch_and_steer",
            )])],
        ),
        vec![Box::new(SwitchAndSteer {
            steering: steering_tx,
        })],
    );
    agent.model_provider_name = "custom.text".into();
    agent.model_name = "text-model".into();
    agent.provider_switch_config = Some(ProviderSwitchConfig {
        config: Some(Arc::new(endpoint_config(
            agent._workspace.path(),
            "next",
            url,
            "next-model",
        ))),
    });
    let (tx, _rx) = mpsc::channel(256);
    let outcome = agent
        .turn_streamed_with_steering_state("switch please", tx, None, Some(&mut steering_rx))
        .await
        .unwrap();
    assert_eq!(outcome.response, "switched reply");
    assert_eq!(outcome.provider_name, "custom.next");
    assert_eq!(outcome.model, "next-model");
    assert_eq!(base_models.lock().as_slice(), ["text-model"]);
    assert_eq!(next_models.lock().as_slice(), ["next-model"]);
    assert!(outcome.new_messages.iter().any(|message| matches!(message, ConversationMessage::Chat(message) if message.role == "user" && message.content.contains("continue with this correction"))));
    server.abort();
}

#[tokio::test]
async fn completion_route_summary_overrides_previous_vision_call() {
    let (url, vision_models, server) = endpoint(serde_json::json!({
        "role":"assistant", "content":null,
        "tool_calls":[{"id":"echo-1","type":"function","function":{"name":"echo","arguments":"{}"}}]
    }))
    .await;
    let base_models = Models::default();
    let mut agent = build_agent_with_runtime(
        capture(&base_models, vec![text_response("base summary")]),
        vec![Box::new(CountingTool {
            name: "echo",
            calls: Arc::default(),
        })],
        zeroclaw_config::schema::ResolvedRuntime {
            max_tool_iterations: 1,
            ..Default::default()
        },
    );
    agent.model_provider_name = "custom.text".into();
    agent.model_name = "text-model".into();
    let config = endpoint_config(agent._workspace.path(), "vision", url, "vision-model");
    agent.multimodal_config.vision_model_provider = Some("custom.vision".into());
    agent.provider_switch_config = Some(ProviderSwitchConfig {
        config: Some(Arc::new(config)),
    });
    let outcome = complete(
        &mut agent,
        "inspect [IMAGE:data:image/png;base64,iVBORw0KGgo=]",
    )
    .await;
    assert!(outcome.response.starts_with("base summary"));
    assert_eq!(vision_models.lock().as_slice(), ["vision-model"]);
    assert_eq!(base_models.lock().as_slice(), ["text-model"]);
    assert_eq!(outcome.provider_name, "custom.text");
    assert_eq!(outcome.model, "text-model");
    server.abort();
}

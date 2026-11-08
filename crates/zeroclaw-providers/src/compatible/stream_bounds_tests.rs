use super::*;
use serde_json::json;
use std::time::Duration;

fn response(chunks: Vec<Vec<u8>>) -> reqwest::Response {
    let body = reqwest::Body::wrap_stream(stream::iter(
        chunks.into_iter().map(Ok::<_, std::io::Error>),
    ));
    reqwest::Response::from(axum::http::Response::new(body))
}

fn frame(value: serde_json::Value) -> Vec<u8> {
    format!("data: {value}\n\n").into_bytes()
}

async fn events(chunks: Vec<Vec<u8>>) -> Vec<StreamResult<StreamEvent>> {
    tokio::time::timeout(
        Duration::from_secs(2),
        sse_bytes_to_events(response(chunks), false).collect(),
    )
    .await
    .expect("parser must finish instead of retaining an invalid prefix")
}

fn assert_rejected(events: &[StreamResult<StreamEvent>], reason: &str) {
    assert!(
        matches!(events.last(), Some(Err(StreamError::ModelProvider(message))) if message.contains(reason)),
        "expected {reason} error, got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(StreamEvent::ToolCall(_) | StreamEvent::Final))),
        "a rejected tool response must not emit executable calls or Final"
    );
}

#[tokio::test]
async fn sparse_and_overflowing_tool_indices_are_rejected_before_allocation() {
    for index in [MAX_STREAM_TOOL_CALLS, 100_000_000, usize::MAX] {
        let result = events(vec![frame(json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": index, "id": "fixture-call",
                "function": {"name": "fixture", "arguments": "{}"}
            }]}}]
        }))])
        .await;
        assert_rejected(&result, "index");
    }
}

#[tokio::test]
async fn omitted_indices_cannot_bypass_the_tool_count_limit() {
    let calls: Vec<_> = (0..=MAX_STREAM_TOOL_CALLS)
        .map(|_| json!({"function": {"name": "fixture", "arguments": "{}"}}))
        .collect();
    let result = events(vec![frame(json!({
        "choices": [{"delta": {"tool_calls": calls}, "finish_reason": "tool_calls"}]
    }))])
    .await;
    assert_rejected(&result, "index");
}

#[tokio::test]
async fn last_admitted_tool_index_remains_valid() {
    let result = events(vec![frame(json!({
        "choices": [{"delta": {"tool_calls": [{
            "index": MAX_STREAM_TOOL_CALLS - 1,
            "function": {"name": "fixture", "arguments": "{}"}
        }]}, "finish_reason": "tool_calls"}]
    }))])
    .await;
    assert_eq!(
        result
            .iter()
            .filter(|event| matches!(event, Ok(StreamEvent::ToolCall(_))))
            .count(),
        1
    );
    assert!(matches!(result.last(), Some(Ok(StreamEvent::Final))));
}

#[tokio::test]
async fn fragmented_arguments_have_a_response_wide_budget() {
    let fragment = frame(json!({
        "choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "function": {"name": "fixture", "arguments": "x".repeat(16_384)}
        }]}}]
    }));
    assert!(fragment.len() < MAX_SSE_LINE_BYTES);
    let result = events(vec![
        fragment.clone();
        MAX_STREAM_TOOL_BYTES / (fragment.len() - 2) + 1
    ])
    .await;
    assert_rejected(&result, "tool-call deltas");
}

#[tokio::test]
async fn oversized_unterminated_lines_fail_in_both_parsers() {
    let oversized = vec![b'x'; MAX_SSE_LINE_BYTES + 1];
    assert_rejected(&events(vec![oversized.clone()]).await, "line exceeds");
    let result: Vec<_> = sse_bytes_to_chunks(response(vec![oversized]), false)
        .collect()
        .await;
    assert!(
        matches!(result.as_slice(), [Err(StreamError::ModelProvider(message))] if message.contains("line exceeds"))
    );
}

#[tokio::test]
async fn line_budget_counts_fragments_before_copying() {
    let result = events(vec![
        vec![b':'; MAX_SSE_LINE_BYTES],
        b"x\n\ndata: [DONE]\n\n".to_vec(),
    ])
    .await;
    assert_rejected(&result, "line exceeds");

    let admitted = events(vec![
        vec![b':'; MAX_SSE_LINE_BYTES],
        b"\n\ndata: [DONE]\n\n".to_vec(),
    ])
    .await;
    assert!(matches!(admitted.as_slice(), [Ok(StreamEvent::Final)]));
}

#[tokio::test]
async fn a_large_http_chunk_of_small_lines_is_not_rejected() {
    let line = frame(json!({"choices": [{"delta": {"content": "x".repeat(8192)}}]}));
    let count = MAX_SSE_LINE_BYTES / line.len() + 1;
    let mut body = line.repeat(count);
    assert!(body.len() > MAX_SSE_LINE_BYTES);
    body.extend_from_slice(b"data: [DONE]\n\n");
    let result = events(vec![body]).await;
    assert!(result.iter().all(Result::is_ok));
    assert_eq!(result.len(), count + 1);
    assert!(matches!(result.last(), Some(Ok(StreamEvent::Final))));
}

#[tokio::test]
async fn invalid_utf8_fails_even_when_the_server_keeps_the_body_open() {
    for text_only in [false, true] {
        let first = stream::once(async { Ok::<_, std::io::Error>(vec![0xff]) });
        let body = reqwest::Body::wrap_stream(first.chain(stream::pending()));
        let response = reqwest::Response::from(axum::http::Response::new(body));
        let error = if text_only {
            tokio::time::timeout(
                Duration::from_secs(2),
                sse_bytes_to_chunks(response, false).next(),
            )
            .await
            .expect("invalid UTF-8 must fail without waiting for EOF")
            .expect("parser must report an error")
            .unwrap_err()
        } else {
            tokio::time::timeout(
                Duration::from_secs(2),
                sse_bytes_to_events(response, false).next(),
            )
            .await
            .expect("invalid UTF-8 must fail without waiting for EOF")
            .expect("parser must report an error")
            .unwrap_err()
        };
        assert!(matches!(error, StreamError::ModelProvider(message) if message.contains("UTF-8")));
    }
}

#[tokio::test]
async fn incomplete_utf8_at_eof_is_not_success() {
    let chunks = vec![b"data: ".to_vec(), vec![0xe4, 0xbd]];
    assert_rejected(&events(chunks.clone()).await, "UTF-8");
    let result: Vec<_> = sse_bytes_to_chunks(response(chunks), false).collect().await;
    assert!(
        matches!(result.as_slice(), [Err(StreamError::ModelProvider(message))] if message.contains("UTF-8"))
    );
}

#[tokio::test]
async fn split_utf8_parallel_tools_and_argument_fragments_survive() {
    let frames = [
        json!({"choices": [{"delta": {"content": "你好", "reasoning_content": "思考"}}]}),
        json!({"choices": [{"delta": {"tool_calls": [
            {"index": 1, "id": "fixture-b", "function": {"name": "second", "arguments": "{\"text\":\""}},
            {"index": 0, "id": "fixture-a", "function": {"name": "first", "arguments": "{\"n\":"}}
        ]}}]}),
        json!({"choices": [{"delta": {"tool_calls": [
            {"index": 0, "function": {"arguments": "1}"}},
            {"index": 1, "function": {"arguments": "世界\"}"}}
        ]}, "finish_reason": "tool_calls"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 5}}),
    ];
    let mut body = Vec::new();
    for value in frames {
        body.extend_from_slice(format!("data: {value}\r\n\r\n").as_bytes());
    }
    body.extend_from_slice(b"data: [DONE]\r\n\r\n");
    // Every multibyte scalar crosses byte-stream item boundaries.
    let result = events(body.into_iter().map(|byte| vec![byte]).collect()).await;
    assert!(result.iter().all(Result::is_ok), "got {result:?}");
    let calls: Vec<_> = result
        .iter()
        .filter_map(|event| match event {
            Ok(StreamEvent::ToolCall(call)) => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].id, "fixture-a");
    assert_eq!(calls[0].arguments, "{\"n\":1}");
    assert_eq!(calls[1].id, "fixture-b");
    assert_eq!(calls[1].arguments, "{\"text\":\"世界\"}");
    assert!(
        result.iter().any(
            |event| matches!(event, Ok(StreamEvent::TextDelta(chunk)) if chunk.delta == "你好")
        )
    );
    assert!(
        result
            .iter()
            .any(|event| matches!(event, Ok(StreamEvent::Usage(_))))
    );
    assert!(matches!(result.last(), Some(Ok(StreamEvent::Final))));
}

#[tokio::test]
async fn text_chunks_preserve_utf8_split_at_each_byte() {
    let mut body = frame(json!({"choices": [{"delta": {"content": "你好🌱"}}]}));
    body.extend_from_slice(b"data: [DONE]\n\n");
    let result: Vec<_> = sse_bytes_to_chunks(
        response(body.into_iter().map(|byte| vec![byte]).collect()),
        false,
    )
    .collect()
    .await;
    assert_eq!(result[0].as_ref().unwrap().delta, "你好🌱");
    assert!(result[1].as_ref().unwrap().is_final);
}

#[tokio::test]
async fn provider_http_stream_surfaces_the_index_limit() {
    use axum::{Router, response::IntoResponse, routing::post};

    let body = frame(json!({"choices": [{"delta": {"tool_calls": [{
        "index": 100_000_000, "function": {"name": "fixture", "arguments": "{}"}
    }]}}]}));
    let app = Router::new().route(
        "/chat/completions",
        post(move || {
            let body = body.clone();
            async move { ([("content-type", "text/event-stream")], body).into_response() }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = zeroclaw_spawn::spawn!(async move { axum::serve(listener, app).await.unwrap() });
    let _server_guard = AbortOnDrop::new(server.abort_handle());
    let provider = OpenAiCompatibleModelProvider::builder("fixture")
        .display_name("fixture")
        .base_url(&format!("http://{addr}"))
        .credential(None)
        .auth_style(AuthStyle::Bearer)
        .build();
    let result: Vec<_> = tokio::time::timeout(
        Duration::from_secs(2),
        crate::ProviderDispatch::from_ref(&provider)
            .stream_chat(
                ProviderChatRequest {
                    messages: &[ChatMessage::user("fixture")],
                    tools: None,
                    thinking: None,
                },
                "fixture-model",
                None,
                StreamOptions {
                    enabled: true,
                    count_tokens: false,
                },
            )
            .collect(),
    )
    .await
    .expect("provider must forward parser errors over the production HTTP path");
    assert_rejected(&result, "index");
}

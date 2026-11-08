use super::*;
use crate::helpers::http_body::test_support::stalled_response;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn tool(endpoint: &str, max_bytes: usize) -> WebFetchTool {
    WebFetchTool::new(
        Arc::new(SecurityPolicy::default()),
        vec!["*".into()],
        vec![],
        max_bytes,
        30,
        FirecrawlConfig {
            enabled: true,
            api_url: endpoint.into(),
            ..FirecrawlConfig::default()
        },
        vec![],
    )
    .unwrap()
}

#[tokio::test]
async fn firecrawl_error_preview_stops_before_an_unfinished_body() {
    let server = stalled_response(
        "503 Service Unavailable",
        None,
        "世界".repeat(2048).into_bytes(),
    )
    .await;
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        tool(&server.url, 512)
            .fetch_via_firecrawl_with_key("https://example.invalid/page", "synthetic-key"),
    )
    .await
    .expect("diagnostic cap must not wait for body EOF")
    .unwrap();
    assert!(!result.success);
    let error = result.error.unwrap();
    assert!(error.contains("503") && error.contains("[truncated]"));
    assert!(error.len() < ERROR_BODY_PREVIEW_BYTES + 128);
}

#[tokio::test]
async fn firecrawl_known_oversize_json_is_rejected_before_waiting_for_body() {
    let cap = tool("http://unused.invalid", 128)
        .firecrawl_body_limit()
        .unwrap();
    let server = stalled_response("200 OK", Some(cap + 1000), Vec::new()).await;
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        tool(&server.url, 128)
            .fetch_via_firecrawl_with_key("https://example.invalid/page", "synthetic-key"),
    )
    .await
    .expect("known oversize JSON must fail from headers");
    assert!(result.unwrap_err().to_string().contains("size limit"));
}

#[tokio::test]
async fn firecrawl_unknown_oversize_json_stops_at_the_wire_budget() {
    let cap = tool("http://unused.invalid", 128)
        .firecrawl_body_limit()
        .unwrap();
    let server = stalled_response("200 OK", None, vec![b'x'; cap + 100]).await;
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        tool(&server.url, 128)
            .fetch_via_firecrawl_with_key("https://example.invalid/page", "synthetic-key"),
    )
    .await
    .expect("unknown-length JSON must stop without EOF")
    .unwrap();
    assert!(!result.success);
    assert!(result.error.unwrap().contains("JSON body limit"));
}

#[tokio::test]
async fn firecrawl_valid_unicode_markdown_respects_the_output_byte_budget() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/scrape"))
        .and(header("Authorization", "Bearer synthetic-key"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data":{"markdown":"世界世界世界"}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let result = tool(&server.uri(), 5)
        .fetch_via_firecrawl_with_key("https://example.invalid/page", "synthetic-key")
        .await
        .unwrap();
    assert!(result.success);
    assert!(result.output.starts_with('世'));
    assert!(!result.output.starts_with("世界"));
    assert!(result.output.contains("[Response truncated"));
}

#[tokio::test]
async fn firecrawl_complete_chunked_json_exactly_at_budget_is_accepted() {
    let server = MockServer::start().await;
    let tool = tool(&server.uri(), 128);
    let cap = tool.firecrawl_body_limit().unwrap();
    let mut body = json!({"data":{"markdown":"complete markdown"},"metadata":""});
    let padding = cap - serde_json::to_vec(&body).unwrap().len();
    body["metadata"] = "x".repeat(padding).into();
    let bytes = serde_json::to_vec(&body).unwrap();
    assert_eq!(bytes.len(), cap);
    Mock::given(method("POST"))
        .and(path("/scrape"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Transfer-Encoding", "chunked")
                .set_body_bytes(bytes),
        )
        .expect(1)
        .mount(&server)
        .await;
    let result = tool
        .fetch_via_firecrawl_with_key("https://example.invalid/page", "synthetic-key")
        .await
        .unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output, "complete markdown");
}

#[tokio::test]
async fn firecrawl_zero_limit_preserves_explicit_unlimited_content() {
    let server = MockServer::start().await;
    let markdown = "x".repeat(FIRECRAWL_JSON_ENVELOPE_BYTES + 1024);
    Mock::given(method("POST"))
        .and(path("/scrape"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data":{"markdown":markdown}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let result = tool(&server.uri(), 0)
        .fetch_via_firecrawl_with_key("https://example.invalid/page", "synthetic-key")
        .await
        .unwrap();
    assert!(result.success);
    assert_eq!(result.output.len(), FIRECRAWL_JSON_ENVELOPE_BYTES + 1024);
    assert!(!result.output.contains("[Response truncated"));
}

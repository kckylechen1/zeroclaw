use super::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zeroclaw_config::autonomy::AutonomyLevel;

fn tool() -> WebFetchTool {
    WebFetchTool::new(
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..Default::default()
        }),
        vec!["*".into()],
        vec![],
        500_000,
        5,
        FirecrawlConfig::default(),
        vec!["*".into()],
    )
    .unwrap()
}

#[tokio::test]
async fn cross_host_redirect_cannot_open_an_unpinned_connection() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "Location",
            format!("http://localhost:{}/target", server.address().port()),
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/target"))
        .respond_with(ResponseTemplate::new(200).set_body_string("unpinned target"))
        .mount(&server)
        .await;
    let result = tool()
        .execute(json!({"url": format!("{}/start", server.uri())}))
        .await
        .unwrap();
    assert!(
        !result.success,
        "cross-host redirect must be rejected: {result:?}"
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "redirect must not open a second, unpinned connection"
    );
}

#[tokio::test]
async fn validated_address_is_the_connection_address() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("validated endpoint"))
        .mount(&server)
        .await;
    let url = format!(
        "http://validated-endpoint.invalid:{}/",
        server.address().port()
    );
    let validation_calls = std::sync::atomic::AtomicUsize::new(0);
    let decoy = MockServer::start().await;
    let connection_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let target = resolve_target_url_with_resolver(
        &url,
        &["*".into()],
        &[],
        &["validated-endpoint.invalid".into()],
        |host| {
            assert_eq!(host, "validated-endpoint.invalid");
            validation_calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![*server.address()])
        },
    )
    .unwrap();
    let resolver = Arc::new(DecoyResolver {
        address: *decoy.address(),
        calls: connection_calls.clone(),
    });
    let (client, _) = tool()
        .client_for_target(reqwest::Client::builder().dns_resolver(resolver), &target)
        .unwrap();
    let result = tool().standard_fetch(&client, &target.url).await;
    assert_eq!(validation_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        connection_calls.load(Ordering::SeqCst),
        0,
        "must not re-resolve during connection"
    );
    assert!(decoy.received_requests().await.unwrap().is_empty());
    assert!(
        result.success,
        "must connect to the validated endpoint: {result:?}"
    );
    assert_eq!(result.output.as_str(), "validated endpoint");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

struct DecoyResolver {
    address: SocketAddr,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}
impl reqwest::dns::Resolve for DecoyResolver {
    fn resolve(&self, _: reqwest::dns::Name) -> reqwest::dns::Resolving {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let address = self.address;
        Box::pin(async move { Ok(Box::new(std::iter::once(address)) as reqwest::dns::Addrs) })
    }
}

#[tokio::test]
async fn same_host_redirect_uses_pinned_address_and_preserves_host_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(ResponseTemplate::new(302).insert_header("Location", "/target"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/target"))
        .respond_with(ResponseTemplate::new(200).set_body_string("same host endpoint"))
        .expect(1)
        .mount(&server)
        .await;
    let target = resolve_target_url_with_resolver(
        &format!("http://pinned.invalid:{}/start", server.address().port()),
        &["*".into()],
        &[],
        &["pinned.invalid".into()],
        |_| Ok(vec![*server.address()]),
    )
    .unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let resolver = Arc::new(DecoyResolver {
        address: "127.0.0.2:1".parse().unwrap(),
        calls: calls.clone(),
    });
    let (client, rejected) = tool()
        .client_for_target(reqwest::Client::builder().dns_resolver(resolver), &target)
        .unwrap();
    let result = tool().standard_fetch(&client, &target.url).await;
    assert!(result.success, "{result:?}");
    assert!(!rejected.load(Ordering::Relaxed));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests {
        assert_eq!(
            request.headers.get("host").unwrap().to_str().unwrap(),
            format!("pinned.invalid:{}", server.address().port())
        );
    }
}

#[test]
fn resolution_rejects_empty_private_and_mixed_addresses_before_connecting() {
    for addresses in [
        vec![],
        vec!["127.0.0.1:0".parse().unwrap()],
        vec!["1.1.1.1:0".parse().unwrap(), "127.0.0.1:0".parse().unwrap()],
    ] {
        assert!(
            resolve_target_url_with_resolver(
                "https://example.com",
                &["*".into()],
                &[],
                &[],
                |_| Ok(addresses)
            )
            .is_err()
        );
    }
    assert!(
        resolve_target_url_with_resolver("https://example.com", &["*".into()], &[], &[], |_| Ok(
            vec!["1.1.1.1:0".parse().unwrap()]
        ))
        .is_ok()
    );
    assert!(
        resolve_target_url_with_resolver(
            "https://example.com",
            &["*".into()],
            &[],
            &["*".into()],
            |_| Ok(vec![])
        )
        .is_err()
    );
}

#[test]
fn canonical_host_is_shared_by_policy_resolution_and_connection() {
    for (url, host) in [
        ("https://EXAMPLE.com./path", "example.com"),
        ("https://bücher.de/", "xn--bcher-kva.de"),
    ] {
        let target = resolve_target_url_with_resolver(url, &[host.into()], &[], &[], |name| {
            assert_eq!(name, host);
            Ok(vec!["1.1.1.1:0".parse().unwrap()])
        })
        .unwrap();
        assert_eq!(target.host, host);
        assert_eq!(
            reqwest::Url::parse(&target.url).unwrap().host_str(),
            Some(host)
        );
    }
}

#[test]
fn configured_proxy_cannot_bypass_the_pin() {
    let mut proxy = ProxyConfig {
        enabled: true,
        all_proxy: Some("http://proxy.invalid:8080".into()),
        ..Default::default()
    };
    assert!(proxy_conflicts_with_dns_pinning(&proxy));
    proxy.scope = ProxyScope::Environment;
    assert!(proxy_conflicts_with_dns_pinning(&proxy));
    proxy.scope = ProxyScope::Services;
    proxy.services = vec!["provider.openai".into()];
    assert!(!proxy_conflicts_with_dns_pinning(&proxy));
    proxy.services = vec!["tool.web_fetch".into()];
    assert!(proxy_conflicts_with_dns_pinning(&proxy));
    assert!(!proxy_conflicts_with_dns_pinning(&ProxyConfig::default()));
}

#[tokio::test]
async fn redirect_rejection_suppresses_firecrawl_but_normal_fallback_still_works() {
    // Scope the fixture API key to a child, never mutate parallel test globals.
    const KEY: &str = "ZEROCLAW_PIN_FIRECRAWL_FIXTURE";
    if std::env::var_os(KEY).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "web_fetch::pin_contract::redirect_rejection_suppresses_firecrawl_but_normal_fallback_still_works", "--nocapture"])
            .env(KEY, "fixture-only")
            .env("NO_PROXY", "*")
            .env("no_proxy", "*")
            .output().unwrap();
        assert!(
            output.status.success(),
            "child contract failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let origin = MockServer::start().await;
    let firecrawl = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/redirect"))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "Location",
            format!("http://localhost:{}/target", origin.address().port()),
        ))
        .mount(&origin)
        .await;
    Mock::given(method("GET"))
        .and(path("/short"))
        .respond_with(ResponseTemplate::new(200).set_body_string("short"))
        .mount(&origin)
        .await;
    Mock::given(method("POST"))
        .and(path("/scrape"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"success": true, "data": {"markdown": "fixture Firecrawl content"}}),
        ))
        .expect(1)
        .mount(&firecrawl)
        .await;
    let mut tool = tool();
    tool.firecrawl = FirecrawlConfig {
        enabled: true,
        api_key_env: KEY.into(),
        api_url: firecrawl.uri(),
        ..Default::default()
    };
    let rejected = tool
        .execute(json!({"url": format!("{}/redirect", origin.uri())}))
        .await
        .unwrap();
    assert!(
        !rejected.success,
        "policy rejection must not fall back: {rejected:?}"
    );
    assert!(firecrawl.received_requests().await.unwrap().is_empty());
    let fallback = tool
        .execute(json!({"url": format!("{}/short", origin.uri())}))
        .await
        .unwrap();
    assert!(
        fallback.success,
        "ordinary fallback remains available: {fallback:?}"
    );
    assert!(fallback.output.contains("fixture Firecrawl content"));
    assert_eq!(firecrawl.received_requests().await.unwrap().len(), 1);
    assert_eq!(origin.received_requests().await.unwrap().len(), 2);
}

use super::*;

async fn env_override_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

fn runtime_proxy_cache_contains(cache_key: &str) -> bool {
    match runtime_proxy_client_cache().read() {
        Ok(guard) => guard.contains_key(cache_key),
        Err(poisoned) => poisoned.into_inner().contains_key(cache_key),
    }
}

#[tokio::test]
async fn runtime_proxy_client_cache_reuses_default_profile_key() {
    let _env_guard = env_override_lock().await;
    let service_key = format!(
        "model_provider.cache_test.{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos()
    );
    let cache_key = runtime_proxy_cache_key(&service_key, None, None);

    clear_runtime_proxy_client_cache();
    assert!(!runtime_proxy_cache_contains(&cache_key));

    let _ = build_runtime_proxy_client(&service_key);
    assert!(runtime_proxy_cache_contains(&cache_key));

    let _ = build_runtime_proxy_client(&service_key);
    assert!(runtime_proxy_cache_contains(&cache_key));
}

#[tokio::test]
async fn proxy_reload_applies_new_config_through_rwlock() {
    let _env_guard = env_override_lock().await;
    set_runtime_proxy_config(ProxyConfig {
        enabled: true,
        http_proxy: Some("http://boot.example:3128".to_string()),
        ..Default::default()
    });
    assert_eq!(
        runtime_proxy_config().http_proxy.as_deref(),
        Some("http://boot.example:3128")
    );

    set_runtime_proxy_config(ProxyConfig {
        enabled: true,
        http_proxy: Some("http://reloaded.example:8080".to_string()),
        ..Default::default()
    });
    assert_eq!(
        runtime_proxy_config().http_proxy.as_deref(),
        Some("http://reloaded.example:8080"),
        "RwLock-backed runtime config must reflect the reloaded value"
    );

    set_runtime_proxy_config(ProxyConfig::default());
}

#[tokio::test]
async fn set_runtime_proxy_config_clears_runtime_proxy_client_cache() {
    let _env_guard = env_override_lock().await;
    let service_key = format!(
        "model_provider.cache_timeout_test.{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos()
    );
    let cache_key = runtime_proxy_cache_key(&service_key, Some(30), Some(5));

    clear_runtime_proxy_client_cache();
    let _ = build_runtime_proxy_client_with_timeouts(&service_key, 30, 5);
    assert!(runtime_proxy_cache_contains(&cache_key));

    set_runtime_proxy_config(ProxyConfig::default());
    assert!(!runtime_proxy_cache_contains(&cache_key));
}

// Restart-equivalence for the retired live-env proxy actions: the
// persisted canonical `[proxy]` config must take effect again at
// process startup without any model-driven action.
#[tokio::test]
async fn boot_proxy_application_seeds_runtime_global_and_env_for_environment_scope() {
    let _env_guard = env_override_lock().await;
    let snapshot = snapshot_proxy_env();

    let proxy = ProxyConfig {
        enabled: true,
        scope: ProxyScope::Environment,
        http_proxy: Some("http://persisted.example:3128".to_string()),
        ..Default::default()
    };
    apply_persisted_proxy_on_boot(&proxy);

    assert_eq!(
        runtime_proxy_config().http_proxy.as_deref(),
        Some("http://persisted.example:3128"),
        "startup must reseed the runtime proxy global from persisted config"
    );
    assert_eq!(
        std::env::var("HTTP_PROXY").ok().as_deref(),
        Some("http://persisted.example:3128"),
        "enabled environment-scope proxy must be broadcast to process env at startup"
    );

    restore_proxy_env(&snapshot);
    set_runtime_proxy_config(ProxyConfig::default());
}

#[tokio::test]
async fn boot_proxy_application_seeds_global_without_env_outside_environment_scope() {
    let _env_guard = env_override_lock().await;
    let snapshot = snapshot_proxy_env();

    let proxy = ProxyConfig {
        enabled: true,
        scope: ProxyScope::Zeroclaw,
        http_proxy: Some("http://internal.example:3128".to_string()),
        ..Default::default()
    };
    apply_persisted_proxy_on_boot(&proxy);

    assert_eq!(
        runtime_proxy_config().http_proxy.as_deref(),
        Some("http://internal.example:3128"),
        "the runtime global is seeded for every scope"
    );
    assert_eq!(
        std::env::var("HTTP_PROXY").ok(),
        snapshot
            .prev
            .iter()
            .find(|(key, _)| *key == "HTTP_PROXY")
            .and_then(|(_, value)| value.clone()),
        "non-environment scope must not touch process env at startup"
    );

    restore_proxy_env(&snapshot);
    set_runtime_proxy_config(ProxyConfig::default());
}

#[tokio::test]
async fn boot_proxy_application_disabled_seeds_explicit_default_without_env() {
    let _env_guard = env_override_lock().await;
    let snapshot = snapshot_proxy_env();

    apply_persisted_proxy_on_boot(&ProxyConfig::default());

    assert!(!runtime_proxy_config().enabled);
    assert_eq!(
        std::env::var("HTTP_PROXY").ok(),
        snapshot
            .prev
            .iter()
            .find(|(key, _)| *key == "HTTP_PROXY")
            .and_then(|(_, value)| value.clone()),
        "disabled proxy must not touch process env at startup"
    );

    restore_proxy_env(&snapshot);
}

#[tokio::test]
async fn boot_proxy_application_invalid_config_applies_disabled_without_env() {
    let _env_guard = env_override_lock().await;
    let snapshot = snapshot_proxy_env();

    // scope=services with an empty services list fails validate().
    let invalid = ProxyConfig {
        enabled: true,
        scope: ProxyScope::Environment,
        http_proxy: Some("http://persisted.example:3128".to_string()),
        services: vec!["model_provider.openai".to_string()],
        ..Default::default()
    };
    // Force a validate() failure without relying on private field
    // invariants: enabled environment scope with no proxy URL at all
    // is valid, so instead use a scope/services contradiction.
    let invalid = {
        let mut p = invalid;
        p.scope = ProxyScope::Services;
        p.services.clear();
        p
    };
    assert!(
        invalid.validate().is_err(),
        "fixture must be invalid or this test proves nothing"
    );
    apply_persisted_proxy_on_boot(&invalid);

    assert!(
        !runtime_proxy_config().enabled,
        "invalid persisted proxy must be applied disabled"
    );
    assert_eq!(
        std::env::var("HTTP_PROXY").ok(),
        snapshot
            .prev
            .iter()
            .find(|(key, _)| *key == "HTTP_PROXY")
            .and_then(|(_, value)| value.clone()),
        "invalid persisted proxy must not touch process env at startup"
    );

    restore_proxy_env(&snapshot);
}

/// `apply_to_process_env` writes all four proxy variable pairs (upper
/// and lowercase), so tests that trigger it must snapshot and restore
/// every pair — restoring only `HTTP_PROXY` would permanently drop an
/// inherited `HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY` for later tests.
struct ProxyEnvSnapshot {
    prev: Vec<(&'static str, Option<String>)>,
}

fn snapshot_proxy_env() -> ProxyEnvSnapshot {
    let mut prev = Vec::new();
    for key in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        prev.push((key, std::env::var(key).ok()));
    }
    ProxyEnvSnapshot { prev }
}

fn restore_proxy_env(snapshot: &ProxyEnvSnapshot) {
    for (key, value) in &snapshot.prev {
        // SAFETY: test-only restore under the env override lock.
        unsafe {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

async fn assert_http_response(
    listener: tokio::net::TcpListener,
    client: reqwest::Client,
    url: &str,
    body: &str,
) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let server = async {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let byte = socket.read_u8().await.unwrap();
            request.push(byte);
            assert!(request.len() < 16_384, "request headers must be bounded");
        }
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        String::from_utf8(request).unwrap()
    };
    let request = async {
        assert_eq!(
            client.get(url).send().await.unwrap().text().await.unwrap(),
            body
        );
    };
    let (request_text, ()) = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        tokio::join!(server, request)
    })
    .await
    .expect("loopback HTTP request must finish");
    request_text
}

#[tokio::test]
async fn reloaded_proxy_routes_the_next_cached_client_to_the_new_endpoint() {
    let _guard = env_override_lock().await;
    for body in ["before reload", "after reload"] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        set_runtime_proxy_config(ProxyConfig {
            enabled: true,
            all_proxy: Some(format!("http://{}", listener.local_addr().unwrap())),
            ..Default::default()
        });
        let request = assert_http_response(
            listener,
            build_runtime_proxy_client_with_timeouts("model_provider.openai", 2, 1),
            "http://proxy-target.invalid/probe",
            body,
        )
        .await;
        assert!(request.starts_with("GET http://proxy-target.invalid/probe HTTP/1.1\r\n"));
    }
    set_runtime_proxy_config(ProxyConfig::default());
}

#[tokio::test]
async fn service_selectors_and_no_proxy_keep_direct_requests_direct() {
    let _guard = env_override_lock().await;
    // A valid but unreachable proxy makes accidental routing fail the request.
    let unused_proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = ProxyConfig {
        enabled: true,
        all_proxy: Some(format!("http://{}", unused_proxy.local_addr().unwrap())),
        scope: ProxyScope::Services,
        services: vec!["memory.*".into()],
        ..Default::default()
    };
    set_runtime_proxy_config(config.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/direct", listener.local_addr().unwrap());
    let client =
        apply_runtime_proxy_to_builder(reqwest::Client::builder().no_proxy(), "tool.http_request")
            .build()
            .unwrap();
    assert!(
        assert_http_response(listener, client, &url, "service excluded")
            .await
            .starts_with("GET /direct HTTP/1.1\r\n")
    );

    config.scope = ProxyScope::Zeroclaw;
    config.no_proxy = vec!["127.0.0.1".into()];
    set_runtime_proxy_config(config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/bypass", listener.local_addr().unwrap());
    assert!(
        assert_http_response(
            listener,
            build_runtime_proxy_client("memory.embeddings"),
            &url,
            "host excluded",
        )
        .await
        .starts_with("GET /bypass HTTP/1.1\r\n")
    );
    set_runtime_proxy_config(ProxyConfig::default());
}

#[tokio::test]
async fn channel_override_uses_its_own_proxy_instead_of_the_runtime_proxy() {
    let _guard = env_override_lock().await;
    let unused_proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    set_runtime_proxy_config(ProxyConfig {
        enabled: true,
        all_proxy: Some(format!("http://{}", unused_proxy.local_addr().unwrap())),
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let override_url = format!("http://{}", listener.local_addr().unwrap());
    let request = assert_http_response(
        listener,
        build_channel_proxy_client_with_timeouts("channel.telegram", Some(&override_url), 2, 1),
        "http://channel-target.invalid/probe",
        "channel override",
    )
    .await;
    assert!(request.starts_with("GET http://channel-target.invalid/probe HTTP/1.1\r\n"));
    set_runtime_proxy_config(ProxyConfig::default());
}

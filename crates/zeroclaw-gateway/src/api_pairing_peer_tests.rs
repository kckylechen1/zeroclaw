//! Adapted contract tests for upstream 47adb9863e4fe689c12c4267be682b071677796d.
use super::*;
use crate::{GatewayRateLimiter, api::test_state, auth_rate_limit::MAX_ATTEMPTS};
use axum::{Router, body::Body, extract::ConnectInfo, http::Request, routing::post};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tower::ServiceExt;
use zeroclaw_config::schema::Config;
use zeroclaw_runtime::security::pairing::PairingGuard;

fn state(trust_forwarded_headers: bool, pair_limit: u32) -> AppState {
    let mut state = test_state(Config::default());
    state.pairing = Arc::new(PairingGuard::new(true, &[]));
    state.trust_forwarded_headers = trust_forwarded_headers;
    state.rate_limiter = Arc::new(GatewayRateLimiter::new(pair_limit, 100, 100));
    state
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/pair", post(submit_pairing_enhanced))
        .with_state(state)
}

async fn request(
    state: &AppState,
    peer: &str,
    headers: &[(&str, &str)],
) -> axum::response::Response {
    let mut request = Request::builder()
        .method("POST")
        .uri("/api/pair")
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let mut request = request.body(Body::from(r#"{"code":"wrong"}"#)).unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    router(state.clone()).oneshot(request).await.unwrap()
}

#[tokio::test]
async fn rotating_untrusted_forwarded_headers_cannot_reset_lockout() {
    let state = state(false, 100);
    for i in 0..5 {
        let forwarded = format!("192.0.2.{i}");
        assert_eq!(
            request(
                &state,
                "203.0.113.7:40000",
                &[("x-forwarded-for", &forwarded)]
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        request(
            &state,
            "203.0.113.7:40001",
            &[("x-forwarded-for", "198.51.100.9")]
        )
        .await
        .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    // A different peer retains its own bucket, even without a forwarding header.
    assert_eq!(
        request(&state, "203.0.113.8:40000", &[]).await.status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn peer_addresses_have_independent_buckets_without_forwarded_headers() {
    let state = state(false, 100);
    for _ in 0..5 {
        assert_eq!(
            request(&state, "203.0.113.7:40000", &[]).await.status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        request(&state, "203.0.113.8:40000", &[]).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&state, "203.0.113.7:40001", &[]).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn shared_pair_request_limit_ignores_untrusted_x_real_ip() {
    let state = state(false, 1);
    assert_eq!(
        request(&state, "203.0.113.7:40000", &[("x-real-ip", "192.0.2.1")])
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&state, "203.0.113.7:40001", &[("x-real-ip", "192.0.2.2")])
            .await
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        request(&state, "203.0.113.8:40000", &[]).await.status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn trusted_forwarded_clients_have_independent_shared_rate_buckets() {
    let state = state(true, 1);
    for client in ["198.51.100.10", "198.51.100.11"] {
        assert_eq!(
            request(&state, "192.0.2.1:40000", &[("x-forwarded-for", client)])
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        request(
            &state,
            "192.0.2.1:40001",
            &[("x-forwarded-for", "198.51.100.10")]
        )
        .await
        .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn missing_empty_or_invalid_trusted_headers_fall_back_to_peer() {
    for headers in [
        vec![],
        vec![("x-forwarded-for", "")],
        vec![("x-forwarded-for", "not-an-ip")],
    ] {
        let state = state(true, 1);
        assert_eq!(
            request(&state, "203.0.113.7:40000", &headers)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            request(&state, "203.0.113.7:40001", &headers)
                .await
                .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            request(&state, "203.0.113.8:40000", &headers)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
}

#[tokio::test]
async fn invalid_pairing_records_into_and_obeys_shared_auth_limiter() {
    let state = state(false, 100);
    for _ in 0..MAX_ATTEMPTS - 1 {
        state.auth_limiter.record_attempt("203.0.113.7");
    }
    assert_eq!(
        request(&state, "203.0.113.7:40000", &[]).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert!(state.auth_limiter.check_rate_limit("203.0.113.7").is_err());
    // PairingGuard has seen only one failure, so this rejection is the shared limiter.
    assert_eq!(
        request(&state, "203.0.113.7:40001", &[]).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn real_http_peer_cannot_rotate_untrusted_forwarded_headers() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let state = state(false, 100);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = zeroclaw_spawn::spawn!(async move {
        axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    struct AbortServer(tokio::task::JoinHandle<()>);
    impl Drop for AbortServer {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _server = AbortServer(server);
    let statuses = tokio::time::timeout(Duration::from_secs(15), async {
        let mut statuses = Vec::new();
        for i in 0..6 {
            let body = r#"{"code":"wrong"}"#;
            let request = format!("POST /api/pair HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-For: 192.0.2.{i}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            let response = String::from_utf8(response).unwrap();
            statuses.push(response.split_whitespace().nth(1).unwrap().parse::<u16>().unwrap());
        }
        statuses
    }).await.unwrap();
    assert_eq!(statuses, vec![400, 400, 400, 400, 400, 429]);
}

#[tokio::test]
async fn disabled_request_cap_preserves_pairing_lockout() {
    let state = state(false, 0);
    for _ in 0..5 {
        assert_eq!(
            request(&state, "203.0.113.7:40000", &[]).await.status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        request(&state, "203.0.113.7:40001", &[]).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn legacy_pair_and_api_pair_share_request_budget() {
    let state = state(false, 1);
    let peer = "203.0.113.7:40000".parse::<SocketAddr>().unwrap();
    let legacy = crate::handle_pair(State(state.clone()), ConnectInfo(peer), HeaderMap::new())
        .await
        .into_response();
    assert_eq!(legacy.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        request(&state, "203.0.113.7:40001", &[]).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

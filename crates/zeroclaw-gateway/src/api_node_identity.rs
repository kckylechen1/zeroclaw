//! Production management API for cryptographic Node identities.
//!
//! Client bearer pairing (`/api/pair`, `devices.db`) is unchanged. These
//! routes enroll and revoke Ed25519 identities in `device_identities.db`.
//! Operator bearer is mandatory and cannot be disabled by `require_pairing`.

use super::AppState;
use crate::device_identity::validate_ceiling;
use crate::operator_auth::gate_operator_identity;
use axum::{
    body::Bytes,
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json},
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

#[derive(Debug, Deserialize, Serialize)]
pub struct IssuePairingBody {
    #[serde(default)]
    pub capability_ceiling: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct EnrollBody {
    pub code: String,
    pub public_key: String,
    #[serde(default)]
    capability_ceiling: Option<serde_json::Value>,
}

/// Operator bearer is required even when `gateway.require_pairing=false`.
fn identity_unavailable() -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "identity store unavailable",
    )
        .into_response()
}

/// Operator bearer is classified before JSON extraction and before the 429
/// lockout gate. Missing bearer is always 401. Wrong bearer records a strict
/// (no loopback exemption) attempt and returns 401 until lockout, then 429.
fn parse_json<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, StatusCode> {
    serde_json::from_slice(body).map_err(|_| StatusCode::BAD_REQUEST)
}

/// POST /api/node-identities/pairing — operator issues a one-time enroll code
/// bound to the operator-approved capability ceiling.
pub async fn issue_pairing(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if let Some(err) = gate_operator_identity(&state, peer, &headers) {
        return err;
    }
    let body = match parse_json::<IssuePairingBody>(&body) {
        Ok(body) => body,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid json").into_response(),
    };
    if let Err(err) = validate_ceiling(&body.capability_ceiling) {
        return (StatusCode::BAD_REQUEST, err.to_string()).into_response();
    }
    let Some(store) = state.node_registry.identities() else {
        return identity_unavailable();
    };
    match store.issue_pairing_code(body.capability_ceiling) {
        Ok(code) => Json(serde_json::json!({ "pairing_code": code })).into_response(),
        Err(crate::device_identity::IdentityError::Capacity) => {
            (StatusCode::SERVICE_UNAVAILABLE, "pairing map at capacity").into_response()
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to issue pairing code",
        )
            .into_response(),
    }
}

/// POST /api/node-identities — enroll a public key against an issued code.
/// The device must not declare a ceiling; the node inherits the code's ceiling.
pub async fn enroll_identity(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if let Some(err) = gate_operator_identity(&state, peer, &headers) {
        return err;
    }
    let body = match parse_json::<EnrollBody>(&body) {
        Ok(body) => body,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid json").into_response(),
    };
    if body.capability_ceiling.is_some() {
        return (
            StatusCode::BAD_REQUEST,
            "capability_ceiling is bound at pairing issue; enroll must not set it",
        )
            .into_response();
    }
    let Some(store) = state.node_registry.identities() else {
        return identity_unavailable();
    };
    match store.enroll(&body.code, &body.public_key) {
        Ok(identity) => Json(serde_json::json!({
            "device_id": identity.device_id,
            "key_fingerprint": identity.key_fingerprint,
            "identity_epoch": identity.identity_epoch,
            "capability_ceiling": identity.capability_ceiling,
        }))
        .into_response(),
        Err(crate::device_identity::IdentityError::InvalidPairingCode) => {
            (StatusCode::BAD_REQUEST, "invalid or expired pairing code").into_response()
        }
        Err(crate::device_identity::IdentityError::InvalidPublicKey) => {
            (StatusCode::BAD_REQUEST, "invalid public key").into_response()
        }
        Err(crate::device_identity::IdentityError::FingerprintConflict) => {
            (StatusCode::CONFLICT, "public key is already bound").into_response()
        }
        Err(crate::device_identity::IdentityError::Capacity) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "identity table at capacity",
        )
            .into_response(),
        Err(crate::device_identity::IdentityError::PersistFailed) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "enroll persist failed; pairing code unchanged",
        )
            .into_response(),
        Err(crate::device_identity::IdentityError::Unavailable) => identity_unavailable(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "enroll failed").into_response(),
    }
}

/// DELETE /api/node-identities/{id} — operator revoke + live socket teardown.
pub async fn revoke_identity(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(device_id): Path<String>,
) -> impl IntoResponse {
    if let Some(err) = gate_operator_identity(&state, peer, &headers) {
        return err;
    }
    let Some(store) = state.node_registry.identities() else {
        return identity_unavailable();
    };
    match state.node_registry.revoke_device(&device_id) {
        Ok(torn) => {
            if !store.contains(&device_id) {
                return (StatusCode::NOT_FOUND, "Device not found").into_response();
            }
            Json(serde_json::json!({
                "revoked": true,
                "device_id": device_id,
                "torn_connections": torn,
            }))
            .into_response()
        }
        Err(crate::device_identity::IdentityError::PersistFailed) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "revoke persist failed; identity unchanged",
        )
            .into_response(),
        Err(crate::device_identity::IdentityError::Unavailable) => identity_unavailable(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "revoke failed").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::test_state;
    use crate::device_identity::DeviceKeyPair;
    use axum::body::Bytes;
    use axum::http::HeaderMap;
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use zeroclaw_runtime::security::pairing::PairingGuard;

    fn loopback_peer() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 9))
    }

    fn remote_peer() -> SocketAddr {
        SocketAddr::from(([203, 0, 113, 50], 9))
    }

    fn operator_state() -> crate::AppState {
        pairing_state(true)
    }

    fn pairing_state(require_pairing: bool) -> crate::AppState {
        let mut config = zeroclaw_config::schema::Config::default();
        config.nodes.enabled = true;
        config.nodes.auth_token = Some("secret".into());
        let mut state = test_state(config);
        state.pairing = Arc::new(PairingGuard::new(require_pairing, &["op-token".into()]));
        state
    }

    fn operator_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer op-token".parse().unwrap());
        headers
    }

    async fn json_of(response: axum::response::Response) -> (StatusCode, serde_json::Value) {
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| serde_json::json!({ "raw": String::from_utf8_lossy(&bytes) }));
        (status, json)
    }

    fn json_bytes<T: serde::Serialize>(value: &T) -> Bytes {
        Bytes::from(serde_json::to_vec(value).unwrap())
    }

    fn enroll_bytes(body: EnrollBody) -> Bytes {
        json_bytes(&body)
    }

    async fn issue(
        state: crate::AppState,
        headers: HeaderMap,
        ceiling: Vec<String>,
    ) -> (StatusCode, serde_json::Value) {
        json_of(
            issue_pairing(
                State(state),
                ConnectInfo(loopback_peer()),
                headers,
                json_bytes(&IssuePairingBody {
                    capability_ceiling: ceiling,
                }),
            )
            .await
            .into_response(),
        )
        .await
    }

    #[tokio::test]
    async fn enroll_and_revoke_go_through_http_handlers() {
        let state = operator_state();
        let (status, issued) = issue(
            state.clone(),
            operator_headers(),
            vec!["system.notify".into()],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let code = issued["pairing_code"].as_str().expect("pairing_code");
        let keys = DeviceKeyPair::generate().unwrap();
        let (status, enrolled) = json_of(
            enroll_identity(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                enroll_bytes(EnrollBody {
                    code: code.to_string(),
                    public_key: keys.public_key_hex().to_string(),
                    capability_ceiling: None,
                }),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "enroll body={enrolled}");
        let device_id = enrolled["device_id"].as_str().unwrap().to_string();
        assert_eq!(enrolled["key_fingerprint"], keys.fingerprint());
        assert_eq!(
            enrolled["capability_ceiling"],
            serde_json::json!(["system.notify"])
        );
        let (conn, close_rx) = state.node_registry.try_reserve().unwrap();
        state
            .node_registry
            .bind_identity(
                &conn.connection_id,
                device_id.clone(),
                keys.fingerprint().to_string(),
            )
            .expect("test bind");
        let (status, revoked) = json_of(
            revoke_identity(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                Path(device_id.clone()),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "revoke body={revoked}");
        assert_eq!(revoked["revoked"], true);
        assert_eq!(
            revoked["torn_connections"],
            serde_json::json!([conn.connection_id])
        );
        assert!(*close_rx.borrow());
        assert!(
            state
                .node_registry
                .identities()
                .expect("test identity store")
                .active_identity(&device_id, keys.fingerprint())
                .is_none()
        );
    }

    #[tokio::test]
    async fn enroll_and_revoke_require_operator_bearer() {
        let state = operator_state();
        let (status, _) = json_of(
            enroll_identity(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                HeaderMap::new(),
                enroll_bytes(EnrollBody {
                    code: "x".into(),
                    public_key: "aa".into(),
                    capability_ceiling: None,
                }),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = issue(state.clone(), HeaderMap::new(), vec![]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = json_of(
            revoke_identity(
                State(state),
                ConnectInfo(loopback_peer()),
                HeaderMap::new(),
                Path("missing".into()),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn require_pairing_false_still_rejects_anonymous_enroll_and_revoke() {
        let state = pairing_state(false);
        let (status, _) = json_of(
            enroll_identity(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                HeaderMap::new(),
                enroll_bytes(EnrollBody {
                    code: "x".into(),
                    public_key: "aa".into(),
                    capability_ceiling: None,
                }),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = issue(state.clone(), HeaderMap::new(), vec![]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = json_of(
            revoke_identity(
                State(state),
                ConnectInfo(loopback_peer()),
                HeaderMap::new(),
                Path("missing".into()),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn enroll_rejects_device_declared_ceiling_and_inherits_code_ceiling() {
        let state = operator_state();
        let (status, issued) = issue(
            state.clone(),
            operator_headers(),
            vec!["system.notify".into()],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let code = issued["pairing_code"].as_str().unwrap().to_string();
        let keys = DeviceKeyPair::generate().unwrap();
        let (status, body) = json_of(
            enroll_identity(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                enroll_bytes(EnrollBody {
                    code: code.clone(),
                    public_key: keys.public_key_hex().to_string(),
                    capability_ceiling: Some(serde_json::json!(["camera.snap"])),
                }),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
        let (status, enrolled) = json_of(
            enroll_identity(
                State(state),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                enroll_bytes(EnrollBody {
                    code,
                    public_key: keys.public_key_hex().to_string(),
                    capability_ceiling: None,
                }),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "enroll body={enrolled}");
        assert_eq!(
            enrolled["capability_ceiling"],
            serde_json::json!(["system.notify"])
        );
        assert_ne!(
            enrolled["capability_ceiling"],
            serde_json::json!(["camera.snap"])
        );
    }

    #[tokio::test]
    async fn enroll_rejects_unknown_pairing_code() {
        let state = operator_state();
        let keys = DeviceKeyPair::generate().unwrap();
        let (status, _) = json_of(
            enroll_identity(
                State(state),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                enroll_bytes(EnrollBody {
                    code: "missing".into(),
                    public_key: keys.public_key_hex().to_string(),
                    capability_ceiling: None,
                }),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn malformed_key_http_enroll_does_not_consume_pairing_code() {
        let state = operator_state();
        let (status, issued) = issue(
            state.clone(),
            operator_headers(),
            vec!["system.notify".into()],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let code = issued["pairing_code"].as_str().unwrap().to_string();
        let (status, _) = json_of(
            enroll_identity(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                enroll_bytes(EnrollBody {
                    code: code.clone(),
                    public_key: "zz".into(),
                    capability_ceiling: None,
                }),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let keys = DeviceKeyPair::generate().unwrap();
        let (status, enrolled) = json_of(
            enroll_identity(
                State(state),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                enroll_bytes(EnrollBody {
                    code,
                    public_key: keys.public_key_hex().to_string(),
                    capability_ceiling: None,
                }),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "code must survive malformed key");
        assert_eq!(
            enrolled["capability_ceiling"],
            serde_json::json!(["system.notify"])
        );
    }

    #[tokio::test]
    async fn identity_handlers_share_pair_rate_limiter() {
        let mut state = operator_state();
        state.rate_limiter = Arc::new(crate::GatewayRateLimiter::new(1, 100, 100));
        let (first, _) = json_of(
            enroll_identity(
                State(state.clone()),
                ConnectInfo(remote_peer()),
                operator_headers(),
                enroll_bytes(EnrollBody {
                    code: "x".into(),
                    public_key: "aa".into(),
                    capability_ceiling: None,
                }),
            )
            .await
            .into_response(),
        )
        .await;
        assert_ne!(first, StatusCode::TOO_MANY_REQUESTS);
        let (second, body) = json_of(
            enroll_identity(
                State(state),
                ConnectInfo(remote_peer()),
                operator_headers(),
                enroll_bytes(EnrollBody {
                    code: "x".into(),
                    public_key: "aa".into(),
                    capability_ceiling: None,
                }),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(second, StatusCode::TOO_MANY_REQUESTS, "body={body}");
    }

    #[tokio::test]
    async fn revoke_unknown_device_is_not_found() {
        let state = operator_state();
        let (status, _) = json_of(
            revoke_identity(
                State(state),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                Path("missing".into()),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn malformed_json_without_bearer_is_unauthorized_not_bad_request() {
        let state = operator_state();
        let (status, _) = json_of(
            enroll_identity(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                HeaderMap::new(),
                Bytes::from_static(b"{not json"),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = json_of(
            issue_pairing(
                State(state),
                ConnectInfo(loopback_peer()),
                HeaderMap::new(),
                Bytes::from_static(b"{not json"),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn consecutive_wrong_bearers_lock_out_remote_caller() {
        let state = operator_state();
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer wrong-token".parse().unwrap());
        let body = enroll_bytes(EnrollBody {
            code: "x".into(),
            public_key: "aa".into(),
            capability_ceiling: None,
        });
        for attempt in 0..crate::auth_rate_limit::MAX_ATTEMPTS {
            let (status, _) = json_of(
                enroll_identity(
                    State(state.clone()),
                    ConnectInfo(remote_peer()),
                    headers.clone(),
                    body.clone(),
                )
                .await
                .into_response(),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "attempt {attempt} must stay 401 until lockout"
            );
        }
        let (status, body) = json_of(
            enroll_identity(State(state), ConnectInfo(remote_peer()), headers, body)
                .await
                .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "body={body}");
    }

    #[tokio::test]
    async fn consecutive_wrong_bearers_lock_out_loopback_peer() {
        let state = operator_state();
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer wrong-token".parse().unwrap());
        let body = enroll_bytes(EnrollBody {
            code: "x".into(),
            public_key: "aa".into(),
            capability_ceiling: None,
        });
        for attempt in 0..crate::auth_rate_limit::MAX_ATTEMPTS {
            let (status, _) = json_of(
                enroll_identity(
                    State(state.clone()),
                    ConnectInfo(loopback_peer()),
                    headers.clone(),
                    body.clone(),
                )
                .await
                .into_response(),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "loopback attempt {attempt} must stay 401 until lockout"
            );
        }
        let (status, body) = json_of(
            enroll_identity(State(state), ConnectInfo(loopback_peer()), headers, body)
                .await
                .into_response(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "reverse-proxy-shaped loopback peer must lock out, body={body}"
        );
    }

    #[tokio::test]
    async fn locked_peer_without_bearer_stays_unauthorized() {
        let state = operator_state();
        let mut wrong = HeaderMap::new();
        wrong.insert("authorization", "Bearer wrong-token".parse().unwrap());
        let body = enroll_bytes(EnrollBody {
            code: "x".into(),
            public_key: "aa".into(),
            capability_ceiling: None,
        });
        for _ in 0..crate::auth_rate_limit::MAX_ATTEMPTS {
            let (status, _) = json_of(
                enroll_identity(
                    State(state.clone()),
                    ConnectInfo(loopback_peer()),
                    wrong.clone(),
                    body.clone(),
                )
                .await
                .into_response(),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
        let (locked, _) = json_of(
            enroll_identity(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                wrong,
                body.clone(),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(locked, StatusCode::TOO_MANY_REQUESTS);
        let (status, _) = json_of(
            enroll_identity(
                State(state),
                ConnectInfo(loopback_peer()),
                HeaderMap::new(),
                Bytes::from_static(b"{not json"),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "anonymous callers must stay 401 even after the IP is locked"
        );
    }

    #[tokio::test]
    async fn identity_store_unavailable_enroll_returns_500_without_consuming_code() {
        let mut state = operator_state();
        let working = Arc::clone(&state.node_registry);
        let (status, issued) = issue(
            state.clone(),
            operator_headers(),
            vec!["system.notify".into()],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let code = issued["pairing_code"].as_str().unwrap().to_string();
        assert_eq!(
            working
                .identities()
                .expect("test identity store")
                .pairing_len(),
            1
        );
        state.node_registry = Arc::new(crate::nodes::NodeRegistry::new(8).without_identities());
        let keys = DeviceKeyPair::generate().unwrap();
        let (status, _) = json_of(
            enroll_identity(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                enroll_bytes(EnrollBody {
                    code: code.clone(),
                    public_key: keys.public_key_hex().to_string(),
                    capability_ceiling: None,
                }),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            working
                .identities()
                .expect("test identity store")
                .pairing_len(),
            1,
            "unavailable enroll must not consume the issued pairing code"
        );
        let (status, _) = issue(state, operator_headers(), vec!["system.notify".into()]).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }
}

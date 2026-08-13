//! Production management API for cryptographic Node identities.
//!
//! Client bearer pairing (`/api/pair`, `devices.db`) is unchanged. These
//! routes enroll and revoke Ed25519 identities in `device_identities.db`.
//! Operator bearer is mandatory and cannot be disabled by `require_pairing`.

use super::AppState;
use crate::device_identity::validate_ceiling;
use axum::{
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Json},
};
use serde::Deserialize;
use std::net::SocketAddr;
use zeroclaw_runtime::security::pairing::{PairingGuard, constant_time_eq};

#[derive(Debug, Deserialize)]
pub struct IssuePairingBody {
    #[serde(default)]
    pub capability_ceiling: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct EnrollBody {
    pub code: String,
    pub public_key: String,
    #[serde(default)]
    capability_ceiling: Option<serde_json::Value>,
}

fn extract_bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|auth| auth.strip_prefix("Bearer "))
}

/// Operator bearer is required even when `gateway.require_pairing=false`.
fn require_operator(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, &'static str)> {
    let token = extract_bearer(headers).unwrap_or("");
    if token.is_empty() {
        return Err((StatusCode::UNAUTHORIZED, "Unauthorized"));
    }
    let hashed = PairingGuard::token_hash(token);
    let known = state.pairing.tokens();
    if !known.iter().any(|stored| constant_time_eq(stored, &hashed)) {
        return Err((StatusCode::UNAUTHORIZED, "Unauthorized"));
    }
    Ok(())
}

fn rate_limit_identity(
    state: &AppState,
    peer: SocketAddr,
    headers: &HeaderMap,
) -> Option<axum::response::Response> {
    let rate_key =
        crate::client_key_from_request(Some(peer), headers, state.trust_forwarded_headers);
    if !state.rate_limiter.allow_pair(&rate_key) {
        let err = serde_json::json!({
            "error": "Too many pairing requests. Please retry later.",
            "retry_after": crate::RATE_LIMIT_WINDOW_SECS,
        });
        return Some((StatusCode::TOO_MANY_REQUESTS, Json(err)).into_response());
    }
    if let Err(e) = state.auth_limiter.check_rate_limit(&rate_key) {
        let err = serde_json::json!({
            "error": format!("Too many auth attempts. Try again in {}s.", e.retry_after_secs),
            "retry_after": e.retry_after_secs,
        });
        return Some((StatusCode::TOO_MANY_REQUESTS, Json(err)).into_response());
    }
    None
}

/// POST /api/node-identities/pairing — operator issues a one-time enroll code
/// bound to the operator-approved capability ceiling.
pub async fn issue_pairing(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<IssuePairingBody>,
) -> impl IntoResponse {
    if let Some(err) = rate_limit_identity(&state, peer, &headers) {
        return err;
    }
    if let Err(err) = require_operator(&state, &headers) {
        return err.into_response();
    }
    if let Err(err) = validate_ceiling(&body.capability_ceiling) {
        return (StatusCode::BAD_REQUEST, err.to_string()).into_response();
    }
    match state
        .node_registry
        .identities()
        .issue_pairing_code(body.capability_ceiling)
    {
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
    Json(body): Json<EnrollBody>,
) -> impl IntoResponse {
    if let Some(err) = rate_limit_identity(&state, peer, &headers) {
        return err;
    }
    if let Err(err) = require_operator(&state, &headers) {
        return err.into_response();
    }
    if body.capability_ceiling.is_some() {
        return (
            StatusCode::BAD_REQUEST,
            "capability_ceiling is bound at pairing issue; enroll must not set it",
        )
            .into_response();
    }
    match state
        .node_registry
        .identities()
        .enroll(&body.code, &body.public_key)
    {
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
    if let Some(err) = rate_limit_identity(&state, peer, &headers) {
        return err;
    }
    if let Err(err) = require_operator(&state, &headers) {
        return err.into_response();
    }
    match state.node_registry.revoke_device(&device_id) {
        Ok(torn) => {
            if !state.node_registry.identities().contains(&device_id) {
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
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "revoke failed").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::test_state;
    use crate::device_identity::DeviceKeyPair;
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
                Json(IssuePairingBody {
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
                Json(EnrollBody {
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
        state.node_registry.bind_identity(
            &conn.connection_id,
            device_id.clone(),
            keys.fingerprint().to_string(),
        );
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
                Json(EnrollBody {
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
                Json(EnrollBody {
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
                Json(EnrollBody {
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
                Json(EnrollBody {
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
                Json(EnrollBody {
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
                Json(EnrollBody {
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
                Json(EnrollBody {
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
                Json(EnrollBody {
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
                Json(EnrollBody {
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
}

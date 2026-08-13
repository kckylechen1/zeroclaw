//! Production management API for cryptographic Node identities.
//!
//! Client bearer pairing (`/api/pair`, `devices.db`) is unchanged. These
//! routes enroll and revoke Ed25519 identities in `device_identities.db`.

use super::AppState;
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Json},
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct EnrollBody {
    pub code: String,
    pub public_key: String,
    #[serde(default)]
    pub capability_ceiling: Vec<String>,
}

fn extract_bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|auth| auth.strip_prefix("Bearer "))
}

fn require_operator(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, &'static str)> {
    if state.pairing.require_pairing() {
        let token = extract_bearer(headers).unwrap_or("");
        if !state.pairing.is_authenticated(token) {
            return Err((StatusCode::UNAUTHORIZED, "Unauthorized"));
        }
    }
    Ok(())
}

/// POST /api/node-identities/pairing — operator issues a one-time enroll code.
pub async fn issue_pairing(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(err) = require_operator(&state, &headers) {
        return err.into_response();
    }
    match state.node_registry.identities().issue_pairing_code() {
        Ok(code) => Json(serde_json::json!({ "pairing_code": code })).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to issue pairing code",
        )
            .into_response(),
    }
}

/// POST /api/node-identities — device presents the one-time code and public key.
pub async fn enroll_identity(
    State(state): State<AppState>,
    Json(body): Json<EnrollBody>,
) -> impl IntoResponse {
    match state.node_registry.identities().enroll(
        &body.code,
        &body.public_key,
        body.capability_ceiling,
    ) {
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
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "enroll failed").into_response(),
    }
}

/// DELETE /api/node-identities/{id} — operator revoke + live socket teardown.
pub async fn revoke_identity(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(device_id): Path<String>,
) -> impl IntoResponse {
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

    fn operator_state() -> crate::AppState {
        let mut config = zeroclaw_config::schema::Config::default();
        config.nodes.enabled = true;
        config.nodes.auth_token = Some("secret".into());
        let mut state = test_state(config);
        state.pairing = Arc::new(PairingGuard::new(true, &["op-token".into()]));
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
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::json!({}));
        (status, json)
    }

    #[tokio::test]
    async fn enroll_and_revoke_go_through_http_handlers() {
        let state = operator_state();
        let (status, issued) = json_of(
            issue_pairing(State(state.clone()), operator_headers())
                .await
                .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let code = issued["pairing_code"].as_str().expect("pairing_code");
        let keys = DeviceKeyPair::generate().unwrap();
        let (status, enrolled) = json_of(
            enroll_identity(
                State(state.clone()),
                Json(EnrollBody {
                    code: code.to_string(),
                    public_key: keys.public_key_hex().to_string(),
                    capability_ceiling: vec!["system.notify".into()],
                }),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "enroll body={enrolled}");
        let device_id = enrolled["device_id"].as_str().unwrap().to_string();
        assert_eq!(enrolled["key_fingerprint"], keys.fingerprint());
        let (conn, close_rx) = state.node_registry.try_reserve().unwrap();
        state.node_registry.bind_identity(
            &conn.connection_id,
            device_id.clone(),
            keys.fingerprint().to_string(),
        );
        let (status, revoked) = json_of(
            revoke_identity(
                State(state.clone()),
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
    async fn enroll_rejects_unknown_pairing_code() {
        let state = operator_state();
        let keys = DeviceKeyPair::generate().unwrap();
        let (status, _) = json_of(
            enroll_identity(
                State(state),
                Json(EnrollBody {
                    code: "missing".into(),
                    public_key: keys.public_key_hex().to_string(),
                    capability_ceiling: vec![],
                }),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn pairing_and_revoke_require_operator_bearer() {
        let state = operator_state();
        let (status, _) = json_of(
            issue_pairing(State(state.clone()), HeaderMap::new())
                .await
                .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = json_of(
            revoke_identity(State(state), HeaderMap::new(), Path("missing".into()))
                .await
                .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn revoke_unknown_device_is_not_found() {
        let state = operator_state();
        let (status, _) = json_of(
            revoke_identity(State(state), operator_headers(), Path("missing".into()))
                .await
                .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}

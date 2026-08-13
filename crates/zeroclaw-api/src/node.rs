//! Device-Node WebSocket wire types (`zeroclaw.nodes.v2`).
//!
//! Serde-only shapes shared by the gateway and future node hosts. This
//! module does not authenticate, authorize, or execute anything.

use serde::{Deserialize, Serialize};

/// HTTP `Sec-WebSocket-Protocol` token for the retired Register-only handshake.
pub const WS_NODES_V1: &str = "zeroclaw.nodes.v1";

/// HTTP `Sec-WebSocket-Protocol` token admitted by the v2 handler.
pub const WS_NODES_V2: &str = "zeroclaw.nodes.v2";

/// Current v2 minor. `Hello.protocol_versions` lists values in this space only.
pub const NODE_V2_MINOR_2_0: &str = "2.0";

/// Minors the gateway will admit. Highest mutually offered value wins.
pub const SUPPORTED_NODE_V2_MINORS: &[&str] = &[NODE_V2_MINOR_2_0];

/// Machine-readable codes for HTTP admission failures and in-band `error` frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeErrorCode {
    /// Client did not offer `zeroclaw.nodes.v2`, or sent the v1 Register handshake.
    ProtocolUnsupported,
    /// `Hello.protocol_versions` had no intersection with [`SUPPORTED_NODE_V2_MINORS`].
    VersionMismatch,
    /// In-band only. HTTP admission never emits this code: a non-loopback
    /// peer without a verified device identity is closed after upgrade.
    LoopbackRequired,
    /// In-band only. Unknown device, revoked device, and signature failure
    /// share this code so responses do not leak device existence.
    IdentityRejected,
}

impl NodeErrorCode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProtocolUnsupported => "protocol_unsupported",
            Self::VersionMismatch => "version_mismatch",
            Self::LoopbackRequired => "loopback_required",
            Self::IdentityRejected => "identity_rejected",
        }
    }
}

/// Reserved grant-proof forms on `Invoke`. Verification is out of scope here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GrantProof {
    Envelope {
        grant: serde_json::Value,
        signature: String,
        key_id: String,
    },
    IntrospectHandle {
        grant_id: String,
        nonce: String,
    },
}

/// Frames a node may send on `/ws/nodes` after a v2 upgrade.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeToGateway {
    Hello {
        protocol_versions: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key_fingerprint: Option<String>,
    },
    Auth {
        signature: String,
        identity_epoch: u64,
    },
    Result {
        call_id: String,
        connection_id: String,
        success: bool,
        output: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Error {
        code: NodeErrorCode,
        retryable: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

/// Frames the gateway may send on `/ws/nodes` after a v2 upgrade.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GatewayToNode {
    HelloAck {
        protocol_version: String,
        connection_id: String,
        generation: u64,
    },
    Challenge {
        nonce: String,
        expires_at: String,
    },
    Invoke {
        call_id: String,
        connection_id: String,
        cap: String,
        cap_revision: u64,
        args: serde_json::Value,
        args_digest: String,
        deadline: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        grant_proof: Option<GrantProof>,
    },
    Error {
        code: NodeErrorCode,
        retryable: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

/// Pick the highest mutually supported v2 minor. `v1` tokens never match.
#[must_use]
pub fn negotiate_v2_minor(offered: &[String]) -> Option<&'static str> {
    let mut best: Option<((u32, u32), &'static str)> = None;
    for offered in offered {
        for &supported in SUPPORTED_NODE_V2_MINORS {
            if offered != supported {
                continue;
            }
            let Some(rank) = parse_v2_minor(supported) else {
                continue;
            };
            if best.is_none_or(|(current, _)| rank > current) {
                best = Some((rank, supported));
            }
        }
    }
    best.map(|(_, version)| version)
}

fn parse_v2_minor(version: &str) -> Option<(u32, u32)> {
    let (major, minor) = version.split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// True when the payload is the retired Register-only handshake.
#[must_use]
pub fn is_v1_register_frame(value: &serde_json::Value) -> bool {
    value.get("type").and_then(serde_json::Value::as_str) == Some("register")
        && value.get("node_id").is_some()
        && value.get("capabilities").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn assert_literal<T>(value: &T, expected: &str)
    where
        T: Serialize,
    {
        let actual = serde_json::to_value(value).expect("serialize");
        let expected: Value = serde_json::from_str(expected).expect("expected json");
        assert_eq!(actual, expected);
    }

    #[test]
    fn hello_literal() {
        let frame = NodeToGateway::Hello {
            protocol_versions: vec!["2.0".into()],
            device_id: None,
            key_fingerprint: None,
        };
        assert_literal(&frame, r#"{"type":"hello","protocol_versions":["2.0"]}"#);
    }

    #[test]
    fn hello_optional_identity_fields_literal() {
        let frame = NodeToGateway::Hello {
            protocol_versions: vec!["2.0".into()],
            device_id: Some("dev-1".into()),
            key_fingerprint: Some("fp-1".into()),
        };
        assert_literal(
            &frame,
            r#"{"type":"hello","protocol_versions":["2.0"],"device_id":"dev-1","key_fingerprint":"fp-1"}"#,
        );
    }

    #[test]
    fn challenge_and_auth_literals() {
        let challenge = GatewayToNode::Challenge {
            nonce: "aa".into(),
            expires_at: "2026-08-13T00:00:00Z".into(),
        };
        assert_literal(
            &challenge,
            r#"{"type":"challenge","nonce":"aa","expires_at":"2026-08-13T00:00:00Z"}"#,
        );
        let auth = NodeToGateway::Auth {
            signature: "bb".into(),
            identity_epoch: 1,
        };
        assert_literal(
            &auth,
            r#"{"type":"auth","signature":"bb","identity_epoch":1}"#,
        );
    }

    #[test]
    fn hello_ack_literal() {
        let frame = GatewayToNode::HelloAck {
            protocol_version: "2.0".into(),
            connection_id: "conn-1".into(),
            generation: 3,
        };
        assert_literal(
            &frame,
            r#"{"type":"hello_ack","protocol_version":"2.0","connection_id":"conn-1","generation":3}"#,
        );
    }

    #[test]
    fn invoke_without_grant_proof_literal() {
        let frame = GatewayToNode::Invoke {
            call_id: "call-1".into(),
            connection_id: "conn-1".into(),
            cap: "system.notify".into(),
            cap_revision: 1,
            args: json!({"title": "hi"}),
            args_digest: "digest-1".into(),
            deadline: "2026-08-13T00:00:00Z".into(),
            grant_proof: None,
        };
        assert_literal(
            &frame,
            r#"{"type":"invoke","call_id":"call-1","connection_id":"conn-1","cap":"system.notify","cap_revision":1,"args":{"title":"hi"},"args_digest":"digest-1","deadline":"2026-08-13T00:00:00Z"}"#,
        );
    }

    #[test]
    fn invoke_grant_proof_envelope_literal() {
        let frame = GatewayToNode::Invoke {
            call_id: "call-1".into(),
            connection_id: "conn-1".into(),
            cap: "system.notify".into(),
            cap_revision: 1,
            args: json!({}),
            args_digest: "digest-1".into(),
            deadline: "2026-08-13T00:00:00Z".into(),
            grant_proof: Some(GrantProof::Envelope {
                grant: json!({"action":"system.notify"}),
                signature: "sig".into(),
                key_id: "key-1".into(),
            }),
        };
        assert_literal(
            &frame,
            r#"{"type":"invoke","call_id":"call-1","connection_id":"conn-1","cap":"system.notify","cap_revision":1,"args":{},"args_digest":"digest-1","deadline":"2026-08-13T00:00:00Z","grant_proof":{"kind":"envelope","grant":{"action":"system.notify"},"signature":"sig","key_id":"key-1"}}"#,
        );
    }

    #[test]
    fn invoke_grant_proof_introspect_handle_literal() {
        let proof = GrantProof::IntrospectHandle {
            grant_id: "grant-1".into(),
            nonce: "nonce-1".into(),
        };
        assert_literal(
            &proof,
            r#"{"kind":"introspect_handle","grant_id":"grant-1","nonce":"nonce-1"}"#,
        );
    }

    #[test]
    fn result_carries_connection_id_literal() {
        let frame = NodeToGateway::Result {
            call_id: "call-1".into(),
            connection_id: "conn-1".into(),
            success: true,
            output: "ok".into(),
            error: None,
        };
        assert_literal(
            &frame,
            r#"{"type":"result","call_id":"call-1","connection_id":"conn-1","success":true,"output":"ok"}"#,
        );
    }

    #[test]
    fn error_codes_are_snake_case_literals() {
        assert_literal(
            &NodeErrorCode::ProtocolUnsupported,
            r#""protocol_unsupported""#,
        );
        assert_literal(&NodeErrorCode::VersionMismatch, r#""version_mismatch""#);
        assert_literal(&NodeErrorCode::LoopbackRequired, r#""loopback_required""#);
        assert_literal(&NodeErrorCode::IdentityRejected, r#""identity_rejected""#);
        let frame = GatewayToNode::Error {
            code: NodeErrorCode::VersionMismatch,
            retryable: false,
            call_id: None,
            detail: None,
        };
        assert_literal(
            &frame,
            r#"{"type":"error","code":"version_mismatch","retryable":false}"#,
        );
    }

    #[test]
    fn negotiate_v2_minor_picks_supported_intersection() {
        assert_eq!(negotiate_v2_minor(&["2.0".into()]), Some(NODE_V2_MINOR_2_0));
        assert_eq!(
            negotiate_v2_minor(&["2.0".into(), "2.0".into()]),
            Some(NODE_V2_MINOR_2_0)
        );
    }

    #[test]
    fn negotiate_v2_minor_rejects_v1_and_empty_intersection() {
        assert_eq!(negotiate_v2_minor(&[]), None);
        assert_eq!(negotiate_v2_minor(&["v1".into()]), None);
        assert_eq!(negotiate_v2_minor(&["1.0".into()]), None);
        assert_eq!(negotiate_v2_minor(&[WS_NODES_V1.into()]), None);
        assert_eq!(negotiate_v2_minor(&["2.1".into()]), None);
    }

    #[test]
    fn v1_register_frame_is_detected() {
        let register = json!({
            "type": "register",
            "node_id": "phone-1",
            "capabilities": [{"name": "camera.snap", "description": "Take a photo"}]
        });
        assert!(is_v1_register_frame(&register));
        assert!(!is_v1_register_frame(
            &json!({"type":"hello","protocol_versions":["2.0"]})
        ));
        assert!(!is_v1_register_frame(&json!({"type":"register"})));
    }

    #[test]
    fn hello_roundtrip_snake_case() {
        let json = r#"{"type":"hello","protocol_versions":["2.0"]}"#;
        let frame: NodeToGateway = serde_json::from_str(json).unwrap();
        match frame {
            NodeToGateway::Hello {
                protocol_versions,
                device_id,
                key_fingerprint,
            } => {
                assert_eq!(protocol_versions, ["2.0"]);
                assert!(device_id.is_none());
                assert!(key_fingerprint.is_none());
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

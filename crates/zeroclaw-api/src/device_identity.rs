//! Versioned device-identity envelope for the Node fabric.
//!
//! Serde-only shapes. Signing, storage, and admission live in the gateway.

use serde::{Deserialize, Serialize};

/// Software key algorithm for V1 device identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKeyAlgorithm {
    Ed25519,
}

impl DeviceKeyAlgorithm {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ed25519 => "ed25519",
        }
    }
}

/// Role a paired identity may exercise. One key may cover both sockets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceRole {
    Client,
    Node,
    Both,
}

impl DeviceRole {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Node => "node",
            Self::Both => "both",
        }
    }
}

/// Local pairing projection states implemented in the first identity slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DevicePairingState {
    Unpaired,
    BootstrapIssued,
    Admitted,
    Revoked,
}

/// Durable local projection of an admitted device identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceIdentityV1 {
    pub device_id: String,
    pub public_key: String,
    pub key_fingerprint: String,
    pub algorithm: DeviceKeyAlgorithm,
    pub role: DeviceRole,
    pub identity_epoch: u64,
    pub admitted_at: String,
    pub revoked_at: Option<String>,
    pub capability_ceiling: Vec<String>,
}

impl DeviceIdentityV1 {
    #[must_use]
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }

    #[must_use]
    pub fn pairing_state(&self) -> DevicePairingState {
        if self.is_revoked() {
            DevicePairingState::Revoked
        } else {
            DevicePairingState::Admitted
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_literal_roundtrip() {
        let identity = DeviceIdentityV1 {
            device_id: "dev-1".into(),
            public_key: "aa".into(),
            key_fingerprint: "bb".into(),
            algorithm: DeviceKeyAlgorithm::Ed25519,
            role: DeviceRole::Node,
            identity_epoch: 1,
            admitted_at: "2026-08-13T00:00:00Z".into(),
            revoked_at: None,
            capability_ceiling: vec!["system.notify".into()],
        };
        let json = serde_json::to_value(&identity).unwrap();
        assert_eq!(json["algorithm"], "ed25519");
        assert_eq!(json["role"], "node");
        let back: DeviceIdentityV1 = serde_json::from_value(json).unwrap();
        assert_eq!(back, identity);
        assert_eq!(back.pairing_state(), DevicePairingState::Admitted);
    }
}

//! Explicit device and bounded owner permissions for Gateway bridge credentials.
use serde::{Deserialize, Serialize};
use zeroclaw_macros::Configurable;

/// Restricted edge access; the existing bridge owns credentials and session scope.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
pub struct GatewayDeviceBridgeConfig {
    pub agent: String,
    pub operations: Vec<String>,
}

impl GatewayDeviceBridgeConfig {
    pub fn valid_operation(operation: &str) -> bool {
        matches!(
            operation,
            "chat"
                | "answer"
                | "cancel"
                | "tachi_start"
                | "tachi_status"
                | "tachi_result"
                | "tachi_cancel"
        )
    }

    pub fn allows(&self, agent: &str, operation: &str) -> bool {
        !self.agent.is_empty()
            && self.agent == agent
            && self.operations.iter().all(|op| Self::valid_operation(op))
            && self.operations.iter().any(|op| op == operation)
    }
}

/// Explicit owner trust, bounded to one agent and once-only approvals.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
pub struct GatewayOwnerBridgeConfig {
    pub agent: String,
    pub operations: Vec<String>,
}

impl GatewayOwnerBridgeConfig {
    pub fn allows(&self, agent: &str, operation: &str) -> bool {
        !self.agent.is_empty()
            && self.agent == agent
            && self
                .operations
                .iter()
                .all(|op| matches!(op.as_str(), "chat" | "cancel" | "approval_response"))
            && self.operations.iter().any(|op| op == operation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegation_grants_are_explicit_and_do_not_add_chat_cancel_or_owner_authority() {
        let mut access = GatewayDeviceBridgeConfig {
            agent: "edge".into(),
            operations: vec!["chat".into(), "cancel".into()],
        };
        for op in [
            "tachi_start",
            "tachi_status",
            "tachi_result",
            "tachi_cancel",
        ] {
            assert!(!access.allows("edge", op));
        }
        access.operations = vec!["tachi_status".into()];
        assert!(access.allows("edge", "tachi_status"));
        assert!(!access.allows("edge", "tachi_start"));
        assert!(!access.allows("edge", "cancel"));
        assert!(!access.allows("edge", "approval_response"));
        assert!(!access.allows("other", "tachi_status"));
        access.operations.push("tachi_shell".into());
        assert!(!access.allows("edge", "tachi_status"));
    }

    #[test]
    fn missing_permissions_and_unknown_permissions_fail_closed() {
        let mut access: GatewayDeviceBridgeConfig = toml::from_str("agent = 'device'").unwrap();
        assert!(!access.allows("device", "chat"));
        access.operations = vec!["chat".into()];
        assert!(access.allows("device", "chat"));
        assert!(!access.allows("other", "chat"));
        assert!(!access.allows("device", "approval_response"));
        access.operations.push("approve".into());
        assert!(!access.allows("device", "chat"));
        assert!(
            toml::from_str::<GatewayDeviceBridgeConfig>("agent = 'device'\nallow_owner = true")
                .is_err()
        );
    }

    #[test]
    fn restriction_survives_configuration_roundtrip() {
        let bridge: crate::schema::GatewayBridgeConfig = toml::from_str(
            "sessions = ['device-session']\n[device]\nagent = 'device'\noperations = ['chat', 'cancel']",
        ).unwrap();
        let encoded = toml::to_string(&bridge).unwrap();
        let decoded: crate::schema::GatewayBridgeConfig = toml::from_str(&encoded).unwrap();
        let access = decoded.device.unwrap();
        assert!(access.allows("device", "chat"));
        assert!(access.allows("device", "cancel"));
        assert!(!access.allows("device", "answer"));
        assert!(
            crate::schema::GatewayBridgeConfig::default()
                .device
                .is_none()
        );
    }
}

#[cfg(test)]
mod owner_tests {
    use super::*;
    #[test]
    fn owner_approval_is_opt_in_and_unknown_grants_fail_closed() {
        let absent: crate::schema::GatewayBridgeConfig = toml::from_str("").unwrap();
        assert!(absent.owner.is_none());
        let mut owner: GatewayOwnerBridgeConfig = toml::from_str("agent = 'owner'").unwrap();
        assert!(!owner.allows("owner", "approval_response"));
        owner.operations = vec!["chat".into(), "approval_response".into()];
        assert!(owner.allows("owner", "approval_response"));
        assert!(!owner.allows("other", "approval_response"));
        assert!(!owner.allows("owner", "answer"));
        owner.operations.push("always".into());
        assert!(!owner.allows("owner", "chat"));
        let device = GatewayDeviceBridgeConfig {
            agent: "owner".into(),
            operations: vec!["chat".into(), "approval_response".into()],
        };
        assert!(!device.allows("owner", "approval_response"));
    }
}

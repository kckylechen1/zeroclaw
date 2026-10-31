//! Explicit operator credentials for the existing management protection surface.
use serde::{Deserialize, Serialize};
use zeroclaw_macros::Configurable;

/// Independently minted credentials; ordinary pairing never populates this list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "gateway.operator"]
#[serde(default, deny_unknown_fields)]
pub struct GatewayOperatorConfig {
    /// SHA-256 hex hashes of operator bearer tokens. Empty preserves legacy
    /// paired-token operator authority with a migration warning.
    #[secret]
    #[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
    pub token_hashes: Vec<String>,
}

impl GatewayOperatorConfig {
    pub fn is_configured(&self) -> bool {
        !self.token_hashes.is_empty()
    }

    pub fn authenticates(&self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        let hash = crate::tokens::hash_token(token);
        self.token_hashes.iter().fold(false, |found, stored| {
            crate::tokens::constant_time_eq(&hash, &stored.to_ascii_lowercase()) | found
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Config, GatewayConfig};

    #[test]
    fn operator_config_roundtrip_and_secret_metadata() {
        let hash = crate::tokens::hash_token("operator-test-only");
        let input = format!(
            "[operator]\ntoken_hashes = ['{}']",
            hash.to_ascii_uppercase()
        );
        let gateway: GatewayConfig = toml::from_str(&input).unwrap();
        assert!(gateway.operator.is_configured());
        assert!(gateway.operator.authenticates("operator-test-only"));
        assert!(!gateway.operator.authenticates("wrong"));
        assert!(!gateway.operator.authenticates(""));
        let decoded: GatewayConfig = toml::from_str(&toml::to_string(&gateway).unwrap()).unwrap();
        assert_eq!(decoded.operator, gateway.operator);
        assert!(GatewayOperatorConfig::prop_is_secret(
            "gateway.operator.token_hashes"
        ));
        assert!(
            !toml::from_str::<GatewayConfig>("")
                .unwrap()
                .operator
                .is_configured()
        );
        assert!(
            !toml::from_str::<GatewayConfig>("[operator]\ntoken_hashes = []")
                .unwrap()
                .operator
                .is_configured()
        );
        assert!(toml::from_str::<GatewayOperatorConfig>("token_hash = 'typo'").is_err());
    }

    #[test]
    fn invalid_hashes_are_rejected_without_legacy_fallback() {
        for hash in [
            "".to_owned(),
            "plaintext-token".into(),
            "g".repeat(64),
            format!(" {}", "a".repeat(64)),
        ] {
            let mut config = Config::default();
            config.gateway.operator.token_hashes = vec![hash];
            assert!(config.gateway.operator.is_configured());
            assert!(!config.gateway.operator.authenticates("plaintext-token"));
            assert!(
                config
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("gateway.operator.token_hashes")
            );
        }
    }
}

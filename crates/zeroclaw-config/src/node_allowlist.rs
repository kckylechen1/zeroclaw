//! Node tool-name allowlist grammar for config parse.
//!
//! Exact `node:<device_id>:<cap>` is always valid. A per-device prefix
//! `node:<device_id>:*` is allowed. A device wildcard (`node:*:*` or
//! `node:*:<cap>`) is rejected at parse time.

use crate::schema::Config;

/// True when `name` is a node-fabric tool name (`node:…`).
#[must_use]
pub fn is_node_tool_name(name: &str) -> bool {
    name.starts_with("node:")
}

/// Reject device-wildcard node tool names. Exact names and optional
/// per-device prefixes are accepted.
pub fn reject_device_wildcard(name: &str) -> Result<(), String> {
    let Some(rest) = name.strip_prefix("node:") else {
        return Ok(());
    };
    let mut parts = rest.splitn(2, ':');
    let device = parts.next().unwrap_or("");
    let cap = parts.next();
    if device.is_empty() || cap.is_none() {
        return Err(format!(
            "node tool {name:?} must be node:<device_id>:<cap> or node:<device_id>:*"
        ));
    }
    if device == "*" {
        return Err(format!(
            "node tool {name:?} uses a device wildcard; name an exact device id"
        ));
    }
    Ok(())
}

/// Walk risk-profile allowlists and card grants for forbidden node globs.
pub fn validate_config(config: &Config) -> Result<(), String> {
    for (alias, profile) in &config.risk_profiles {
        if let Some(tools) = &profile.allowed_tools {
            for name in tools {
                reject_device_wildcard(name)
                    .map_err(|reason| format!("risk_profiles.{alias}.allowed_tools: {reason}"))?;
            }
        }
        for name in &profile.excluded_tools {
            reject_device_wildcard(name)
                .map_err(|reason| format!("risk_profiles.{alias}.excluded_tools: {reason}"))?;
        }
    }
    for (alias, card) in &config.cards {
        for grant in &card.grants.tools {
            reject_device_wildcard(&grant.tool)
                .map_err(|reason| format!("cards.{alias}.grants.tools: {reason}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_and_per_device_prefix_are_accepted() {
        assert!(reject_device_wildcard("node:phone-1:system.notify").is_ok());
        assert!(reject_device_wildcard("node:phone-1:*").is_ok());
        assert!(reject_device_wildcard("shell").is_ok());
    }

    #[test]
    fn device_wildcard_is_rejected() {
        let err = reject_device_wildcard("node:*:*").unwrap_err();
        assert!(err.contains("device wildcard"), "{err}");
        let err = reject_device_wildcard("node:*:system.notify").unwrap_err();
        assert!(err.contains("device wildcard"), "{err}");
    }

    #[test]
    fn config_validate_rejects_star_star_in_risk_profile() {
        let mut config = Config::default();
        config.risk_profiles.insert(
            "default".into(),
            crate::schema::RiskProfileConfig {
                allowed_tools: Some(vec!["node:*:*".into()]),
                ..crate::schema::RiskProfileConfig::default()
            },
        );
        let err = validate_config(&config).unwrap_err();
        assert!(err.contains("node:*:*"), "{err}");
        assert!(err.contains("risk_profiles.default"), "{err}");
    }

    #[test]
    fn exact_node_tool_is_not_a_wildcard() {
        assert!(is_node_tool_name("node:phone-1:system.notify"));
        assert!(!is_node_tool_name("shell"));
    }
}

use serde::{Deserialize, Serialize};

/// The agent's autonomy level, ordered from least to most autonomous.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum AutonomyLevel {
    /// Read-only: can observe but not act
    ReadOnly,
    /// Supervised: acts but requires approval for risky operations
    #[default]
    Supervised,
    /// Full: autonomous execution within policy bounds
    Full,
}

/// How a non-empty `allowed_tools` list treats tools discovered at runtime
/// from MCP servers, whose names carry a `<server>__<tool>` shape.
///
/// The distinction matters because MCP tool sets are not known when the
/// allow-list is written: a server can add a tool at any time, and under
/// [`Self::AutoAdmit`] that tool is reachable the moment it appears, without
/// anyone editing config. For an agent that can move money or touch
/// production, "whatever that server offers next" is not an allow-list.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum McpDiscoveredToolPolicy {
    /// An MCP tool must be named in `allowed_tools` like any other tool.
    /// A server that grows a new tool does not gain reach until someone
    /// says so. This is the default.
    #[default]
    ExplicitOnly,
    /// Any `<server>__<tool>` name is admitted once `allowed_tools` is
    /// non-empty; the list only constrains built-in tools. Upstream's
    /// behavior, kept as an escape hatch for setups that rely on it.
    AutoAdmit,
}

impl McpDiscoveredToolPolicy {
    /// Whether `name` is admitted purely by virtue of looking like an MCP
    /// tool. Only ever true under [`Self::AutoAdmit`].
    #[must_use]
    pub fn admits_unlisted(self, name: &str) -> bool {
        self == Self::AutoAdmit && name.contains("__")
    }
}

/// What to do when a configured approver cannot be reached. Default FAIL-CLOSED.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum OnNoApprover {
    /// Fail-closed: deny when the approver is unreachable / declines / times out.
    #[default]
    Deny,
    /// Explicit opt-in: fall back to the originating channel (today's behavior).
    InheritOriginator,
}

impl crate::config::HasPropKind for OnNoApprover {
    const PROP_KIND: crate::config::PropKind = crate::config::PropKind::Enum;
}

fn default_approval_timeout_secs() -> u64 {
    120
}

/// Routes tool approvals to a distinct approver channel with fail-closed defaults.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct ApprovalRoute {
    /// A registered channel name (NOT the originator) — the distinct-approver hop.
    pub approver_channel: String,
    /// Behavior when the approver is unreachable. Fail-closed by default.
    #[serde(default)]
    pub on_no_approver: OnNoApprover,
    /// Bound the approver's response window; a timeout denies (DoS guard). Default 120s.
    #[serde(default = "default_approval_timeout_secs")]
    pub timeout_secs: u64,
}

impl crate::config::HasPropKind for ApprovalRoute {
    const PROP_KIND: crate::config::PropKind = crate::config::PropKind::Object;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_route_defaults_are_fail_closed() {
        // Absent optional fields: fail-closed policy + bounded 120s window.
        let r: ApprovalRoute = toml::from_str("approver_channel = \"ops\"").unwrap();
        assert_eq!(r.approver_channel, "ops");
        assert_eq!(
            r.on_no_approver,
            OnNoApprover::Deny,
            "default must fail closed"
        );
        assert_eq!(r.timeout_secs, 120);
    }

    #[test]
    fn approval_route_round_trips() {
        let r = ApprovalRoute {
            approver_channel: "ops".into(),
            on_no_approver: OnNoApprover::InheritOriginator,
            timeout_secs: 30,
        };
        let s = toml::to_string(&r).unwrap();
        // kebab-case enum on the wire.
        assert!(s.contains("on_no_approver = \"inherit-originator\""), "{s}");
        let back: ApprovalRoute = toml::from_str(&s).unwrap();
        assert_eq!(back.approver_channel, r.approver_channel);
        assert_eq!(back.on_no_approver, r.on_no_approver);
        assert_eq!(back.timeout_secs, r.timeout_secs);
    }

    #[test]
    fn risk_profile_has_no_route_by_default() {
        use crate::schema::RiskProfileConfig;
        assert!(
            RiskProfileConfig::default().approval_route.is_none(),
            "default profile must keep today's originating-channel behavior"
        );
    }
}

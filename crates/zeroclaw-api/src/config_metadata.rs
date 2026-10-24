//! Shared metadata produced by config introspection and read by CLI, Gateway,
//! channels and runtime surfaces. Config owns schema generation and mutations.

/// Describes a single secret field discovered via `#[derive(Configurable)]`.
#[derive(Debug, Clone)]
pub struct SecretFieldInfo {
    /// Full dotted name (e.g. `channels.matrix.access-token`)
    pub name: &'static str,
    /// Category for grouping in `zeroclaw config list`
    pub category: &'static str,
    /// Whether this field currently has a non-empty value
    pub is_set: bool,
}

/// Runtime type classification for config property values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PropKind {
    String,
    Bool,
    Integer,
    Float,
    /// An enum or other serde-serializable type (parsed as TOML string).
    Enum,
    /// A reference to a configured alias; `alias_source` names the namespace.
    AliasRef,
    /// A `Vec<String>` field; set via comma-separated input.
    StringArray,
    ObjectArray,
    Object,
}

/// Security classification for credential-shaped config surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSurfaceClass {
    EncryptedSecret,
    PathOnlyReference,
    PublicValue,
    ExternalAuthStore,
    LegacyEnvPath,
    RequiresFollowUp,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
pub enum ConfigTab {
    #[default]
    /// No tab grouping — field appears in a flat list.
    None,

    // ── Shared (providers + channels) ──
    Connection,
    Advanced,

    // ── Providers ──
    Model,

    // ── Channels ──
    Behavior,

    // ── Agents: field partitions ──
    General,
    Channels,
    Providers,
    Bundles,
    Cron,
    Tuning,
    Workspace,
    Memory,

    // ── Agents: composite (custom-component) tabs ──
    PeerGroups,
    Personality,

    // ── MCP ──
    Settings,
    Servers,

    // ── Cost ──
    Limits,
    Costs,

    // ── Skill bundles ──
    Skills,
    Aliases,
}

impl ConfigTab {
    /// Display label for the tab bar. Returns `""` for `None`.
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Connection => "Connection",
            Self::Advanced => "Advanced",
            Self::Model => "Model",
            Self::Behavior => "Behavior",
            Self::General => "General",
            Self::Channels => "Channels",
            Self::Providers => "Providers",
            Self::Bundles => "Bundles",
            Self::Cron => "Cron",
            Self::Tuning => "Tuning",
            Self::Workspace => "Workspace",
            Self::Memory => "Memory",
            Self::PeerGroups => "Peer Groups",
            Self::Personality => "Personality",
            Self::Settings => "Settings",
            Self::Servers => "Servers",
            Self::Limits => "Limits",
            Self::Costs => "Costs",
            Self::Skills => "Skills",
            Self::Aliases => "Aliases",
        }
    }

    /// `true` when this is the `None` variant (no tab grouping).
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

impl std::fmt::Display for ConfigTab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl PropKind {
    /// Stable snake_case wire name matching the serde serialization.
    /// Useful when consumers need the tag as a `&'static str` without
    /// going through serde round-trip.
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Bool => "bool",
            Self::Integer => "integer",
            Self::Float => "float",
            Self::Enum => "enum",
            Self::AliasRef => "alias_ref",
            Self::StringArray => "string_array",
            Self::ObjectArray => "object_array",
            Self::Object => "object",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct NestedOptionEntry {
    /// snake_case field name on the parent struct (e.g. `"telegram"`,
    /// `"voice_duplex"`).
    pub field: &'static str,
    /// `true` when the parent struct's field is `Some(_)`.
    pub present: bool,
    /// Display name from `#[display_name = "..."]`; falls back to a
    /// title-cased rendering of the snake_case field name when the
    /// attribute is absent.
    pub display_name: &'static str,
    /// One-line summary from `#[description = "..."]`. Empty when the
    /// attribute is absent.
    pub description: &'static str,
}

/// One row emitted by the `Configurable` derive's `integration_descriptor()`
/// method on structs annotated with `#[integration(...)]`. Used for nested
/// toggleable configs (e.g. `BrowserConfig`, `CronConfig`) where the
/// integration is "active" iff a named bool field on the struct is `true`.
#[derive(Debug, Clone, Copy)]
pub struct IntegrationDescriptor {
    pub display_name: &'static str,
    pub description: &'static str,
    /// Free-form category label (e.g. `"ToolsAutomation"`). The
    /// integrations registry maps this string to its own
    /// `IntegrationCategory` enum so the schema crate doesn't have to
    /// depend on it.
    pub category: &'static str,
    /// Snapshot of the named status field at the moment this descriptor
    /// was built (`status_field = "enabled"` ⇒ `self.enabled`).
    pub active: bool,
}

/// Metadata for one channel type, as returned by `zeroclaw_config::schema::ChannelsConfig::channels`.
#[derive(Debug, Clone)]
pub struct ChannelInfo {
    /// Canonical kebab-case identifier used in config TOML
    /// (`[channels.<kind>]`). Matches the field name on
    /// `ChannelsConfig` so Quickstart and other surfaces can
    /// reuse the schema's own labeling without a parallel map.
    pub kind: &'static str,
    pub name: &'static str,
    pub desc: &'static str,
    pub configured: bool,
}

/// The trait for describing a channel
pub trait ChannelConfig {
    /// human-readable name
    fn name() -> &'static str;
    /// short description
    fn desc() -> &'static str;
}

#[cfg(test)]
mod tests {
    use super::ConfigTab;

    #[test]
    fn tab_serialization_preserves_variant_names_separately_from_labels() {
        assert_eq!(serde_json::to_value(ConfigTab::None).unwrap(), "None");
        assert_eq!(
            serde_json::to_value(ConfigTab::PeerGroups).unwrap(),
            "PeerGroups"
        );
        assert_eq!(ConfigTab::default(), ConfigTab::None);
        assert!(ConfigTab::None.is_none());
        assert_eq!(ConfigTab::None.to_string(), "");
        assert_eq!(ConfigTab::PeerGroups.to_string(), "Peer Groups");
        assert_eq!(
            serde_json::from_str::<ConfigTab>("\"Connection\"").unwrap(),
            ConfigTab::Connection
        );
    }
}

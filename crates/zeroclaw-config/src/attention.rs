//! Owner-authored attention policy. Resolved from live config at delivery time.
use serde::{Deserialize, Serialize};
use zeroclaw_macros::Configurable;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
#[prefix = "gateway.attention"]
pub struct AttentionConfig {
    /// Explicit IANA timezone. Empty or invalid values hold all notifications.
    pub timezone: String,
    /// Local quiet interval start, HH:MM (inclusive).
    pub quiet_start: String,
    /// Local quiet interval end, HH:MM (exclusive). Equal boundaries mean all day.
    pub quiet_end: String,
    /// Exact trusted sources permitted to bypass quiet hours. Empty denies bypass.
    #[serde(default)]
    pub important_sources: Vec<ImportantSource>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ImportantSource {
    pub bridge: String,
    pub recipient: String,
    pub source_kind: String,
    pub source_id: String,
}

impl crate::traits::HasPropKind for Vec<ImportantSource> {
    const PROP_KIND: crate::traits::PropKind = crate::traits::PropKind::ObjectArray;
}

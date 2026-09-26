//! Per-agent advisor target (`[agents.<alias>] advisor = "..."`).
//!
//! An advisor is a stronger model an agent may consult, through the
//! `reasoning_subagent` tool, when a question is hard. The target is typed
//! so later phases can add other kinds without a second config key:
//!
//! - `model:<type>.<alias>` names a configured
//!   `[providers.models.<type>.<alias>]` entry (supported);
//! - `harness:<name>` names an external harness. It parses so configs can
//!   be written against the final shape, but `Config::validate()` refuses
//!   it until harness advisors land (#405 phase 2, tied to #381).

use serde::{Deserialize, Serialize};

use crate::providers::ModelProviderRef;

/// Prefix of a model advisor target.
pub const ADVISOR_MODEL_PREFIX: &str = "model:";
/// Prefix of a harness advisor target.
pub const ADVISOR_HARNESS_PREFIX: &str = "harness:";

/// Default number of advisor consultations an agent may make in one turn
/// when `advisor_max_calls_per_turn` is unset.
pub const DEFAULT_ADVISOR_MAX_CALLS_PER_TURN: u32 = 2;

/// Where an agent's advisor consultations go. Serialized as a single
/// prefixed string (`"model:openai.big"`, `"harness:codex"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum AdvisorTarget {
    /// A configured `[providers.models.<type>.<alias>]` entry, as a dotted
    /// reference. Existence is checked by `Config::validate()`.
    Model(ModelProviderRef),
    /// An external harness by name. Not supported yet: validation refuses it.
    Harness(String),
}

/// Why an advisor string did not parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("advisor must be `model:<type>.<alias>` or `harness:<name>` (got {0:?})")]
pub struct AdvisorTargetParseError(pub String);

impl AdvisorTarget {
    /// Parse `model:<type>.<alias>` or `harness:<name>`. Surrounding
    /// whitespace is ignored; an empty reference after the prefix is an
    /// error. The dotted shape of a model reference is checked by
    /// `Config::validate()` alongside the other provider refs, so the
    /// operator gets the same path-bound error for every ref field.
    pub fn parse(value: &str) -> Result<Self, AdvisorTargetParseError> {
        let trimmed = value.trim();
        if let Some(rest) = trimmed.strip_prefix(ADVISOR_MODEL_PREFIX) {
            let rest = rest.trim();
            if !rest.is_empty() {
                return Ok(Self::Model(ModelProviderRef::from(rest)));
            }
        } else if let Some(rest) = trimmed.strip_prefix(ADVISOR_HARNESS_PREFIX) {
            let rest = rest.trim();
            if !rest.is_empty() {
                return Ok(Self::Harness(rest.to_string()));
            }
        }
        Err(AdvisorTargetParseError(value.to_string()))
    }

    /// The model reference, when this is a model target.
    #[must_use]
    pub fn model_ref(&self) -> Option<&ModelProviderRef> {
        match self {
            Self::Model(model) => Some(model),
            Self::Harness(_) => None,
        }
    }
}

// Schema-export view: one prefixed string. Hand-written because the enum's
// serde shape (`try_from`/`into` String) is not what a derive would describe.
#[cfg(feature = "schema-export")]
impl schemars::JsonSchema for AdvisorTarget {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        "AdvisorTarget".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^\\s*(model|harness):\\s*\\S.*$",
            "description": "`model:<type>.<alias>` (a configured providers.models entry) or `harness:<name>` (not supported yet)."
        })
    }
}

impl std::fmt::Display for AdvisorTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Model(model) => write!(f, "{ADVISOR_MODEL_PREFIX}{model}"),
            Self::Harness(name) => write!(f, "{ADVISOR_HARNESS_PREFIX}{name}"),
        }
    }
}

impl std::str::FromStr for AdvisorTarget {
    type Err = AdvisorTargetParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<String> for AdvisorTarget {
    type Error = AdvisorTargetParseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<AdvisorTarget> for String {
    fn from(value: AdvisorTarget) -> Self {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_model_and_harness_targets() {
        assert_eq!(
            AdvisorTarget::parse("model:openai.big").unwrap(),
            AdvisorTarget::Model("openai.big".into())
        );
        assert_eq!(
            AdvisorTarget::parse("  harness:codex ").unwrap(),
            AdvisorTarget::Harness("codex".into())
        );
    }

    #[test]
    fn rejects_untyped_or_empty_targets() {
        for bad in ["openai.big", "model:", "harness:  ", "", "mode:openai.big"] {
            let err = AdvisorTarget::parse(bad).unwrap_err();
            assert!(err.to_string().contains("model:<type>.<alias>"), "{err}");
        }
    }

    #[test]
    fn display_round_trips() {
        for text in ["model:anthropic.opus", "harness:claude-code"] {
            let target = AdvisorTarget::parse(text).unwrap();
            assert_eq!(target.to_string(), text);
            assert_eq!(AdvisorTarget::parse(&target.to_string()).unwrap(), target);
        }
    }
}

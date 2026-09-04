//! Condition comparison-operator catalog. Definition-format surface only:
//! the legacy run-side evaluator was deleted with the SOP run side, and what
//! survives is the operator set authoring surfaces render when building the
//! `condition` strings stored on triggers and steps.

/// A condition comparison operator. This enum is the single source of truth for
/// the operator set: every authoring surface renders the list this enum yields
/// (via [`ConditionOp::catalog`]). Adding an operator here is the only edit
/// needed; no surface hand-lists operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum_macros::EnumIter)]
pub enum ConditionOp {
    Gt,
    Lt,
    Gte,
    Lte,
    Eq,
    Neq,
}

impl ConditionOp {
    /// The literal token as it appears in a condition string.
    pub fn token(self) -> &'static str {
        match self {
            Self::Gt => ">",
            Self::Lt => "<",
            Self::Gte => ">=",
            Self::Lte => "<=",
            Self::Eq => "==",
            Self::Neq => "!=",
        }
    }

    /// A short human label for pickers ("is", "is greater than", ...).
    pub fn label(self) -> &'static str {
        match self {
            Self::Eq => "is",
            Self::Neq => "is not",
            Self::Gt => "is greater than",
            Self::Lt => "is less than",
            Self::Gte => "is at least",
            Self::Lte => "is at most",
        }
    }

    /// The full operator catalog in canonical display order (equality first,
    /// then ordering), for authoring surfaces to render verbatim.
    pub fn catalog() -> Vec<ConditionOpSpec> {
        use strum::IntoEnumIterator;
        [
            Self::Eq,
            Self::Neq,
            Self::Gt,
            Self::Gte,
            Self::Lt,
            Self::Lte,
        ]
        .into_iter()
        .map(|op| {
            debug_assert!(Self::iter().any(|variant| variant == op));
            ConditionOpSpec {
                token: op.token().to_string(),
                label: op.label().to_string(),
            }
        })
        .collect()
    }

    /// Every operator token, in enum order.
    pub fn catalog_tokens() -> Vec<&'static str> {
        use strum::IntoEnumIterator;
        Self::iter().map(Self::token).collect()
    }
}

/// Wire shape of one operator for authoring surfaces: the literal `token` to
/// splice into a condition string, and a human `label` to show in a picker.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct ConditionOpSpec {
    pub token: String,
    pub label: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_covers_every_operator_with_matching_tokens() {
        use strum::IntoEnumIterator;
        let tokens: Vec<&'static str> = ConditionOp::iter().map(ConditionOp::token).collect();
        assert_eq!(ConditionOp::catalog_tokens(), tokens);

        let catalog = ConditionOp::catalog();
        assert_eq!(catalog.len(), tokens.len());
        for spec in &catalog {
            assert!(
                tokens.contains(&spec.token.as_str()),
                "catalog token {:?} is not an enum token",
                spec.token
            );
            assert!(!spec.label.is_empty());
        }
        let catalog_tokens: Vec<&str> = catalog.iter().map(|s| s.token.as_str()).collect();
        let mut a = catalog_tokens.clone();
        let mut b = tokens.clone();
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b, "catalog and enum must cover the same operator set");
    }
}

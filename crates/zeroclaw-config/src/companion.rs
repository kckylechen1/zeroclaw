//! Companion-memory configuration: who the owner is.
//!
//! `[companion_memory.owner]` declares an opaque `principal_id` plus an
//! explicit ingress-identity list. A list hit is the owner. Everything else
//! is shared-operator and can never produce `owner_authored`. Single-user
//! convenience: an empty list plus `trust_local = true` treats Trusted
//! CLI/stdio/pairing as owner.
//!
//! Store paths, enable flags, and capture wiring live elsewhere. This module
//! is the owner-gate section only.

use serde::{Deserialize, Serialize};
use zeroclaw_api::companion::{CompanionOwnerGate, IngressIdentity};
use zeroclaw_api::principal::PrincipalId;
use zeroclaw_macros::Configurable;

/// Top-level `[companion_memory]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
#[prefix = "companion_memory"]
pub struct CompanionMemoryConfig {
    /// Who may produce `owner_authored` companion-memory rows.
    #[nested]
    pub owner: CompanionOwnerConfig,
}

impl CompanionMemoryConfig {
    /// True when every field is at its compiled default (missing-section
    /// equivalent). Used to keep empty companion config off disk.
    #[must_use]
    pub fn is_unset(&self) -> bool {
        *self == Self::default()
    }
}

/// `[companion_memory.owner]` — declared owner plus ingress matching rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
#[prefix = "companion_memory.owner"]
pub struct CompanionOwnerConfig {
    /// Opaque owner principal id. Stamped on owner-authored rows; never used
    /// as the ingress match key.
    pub principal_id: String,
    /// Explicit ingress-identity tokens. A hit is the owner.
    pub identities: Vec<String>,
    /// When `identities` is empty, treat Trusted CLI/stdio/pairing as owner.
    pub trust_local: bool,
}

impl CompanionOwnerConfig {
    /// Domain view of this section, for owner-gate classification.
    #[must_use]
    pub fn gate(&self) -> CompanionOwnerGate {
        CompanionOwnerGate {
            principal_id: PrincipalId::from(self.principal_id.as_str()),
            identities: self
                .identities
                .iter()
                .map(|token| IngressIdentity::from(token.as_str()))
                .collect(),
            trust_local: self.trust_local,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Config;
    use zeroclaw_api::companion::{AuthorityClass, CompanionIngress, classify_companion_authority};

    fn classify(toml: &str, ingress: CompanionIngress) -> AuthorityClass {
        let parsed: CompanionMemoryConfig = toml::from_str(toml).expect("owner section parses");
        classify_companion_authority(&ingress, &parsed.owner.gate())
    }

    #[test]
    fn explicit_list_hit_from_toml_is_owner_authored() {
        let toml = r#"
[owner]
principal_id = "owner-principal"
identities = ["wechat:alice", "telegram:42"]
trust_local = false
"#;
        assert_eq!(
            classify(toml, CompanionIngress::explicit("wechat:alice")),
            AuthorityClass::OwnerAuthored
        );
    }

    #[test]
    fn explicit_list_miss_from_toml_is_never_owner_authored() {
        let toml = r#"
[owner]
principal_id = "owner-principal"
identities = ["wechat:alice"]
trust_local = false
"#;
        assert_eq!(
            classify(toml, CompanionIngress::explicit("wechat:bob")),
            AuthorityClass::SharedOperator
        );
        assert_eq!(
            classify(toml, CompanionIngress::trusted_local()),
            AuthorityClass::SharedOperator
        );
    }

    #[test]
    fn empty_list_trust_local_true_treats_trusted_cli_as_owner() {
        let toml = r#"
[owner]
principal_id = "owner-principal"
identities = []
trust_local = true
"#;
        assert_eq!(
            classify(toml, CompanionIngress::trusted_local()),
            AuthorityClass::OwnerAuthored
        );
        assert_eq!(
            classify(toml, CompanionIngress::explicit("wechat:stranger")),
            AuthorityClass::SharedOperator
        );
    }

    #[test]
    fn empty_list_trust_local_false_never_yields_owner_authored() {
        let toml = r#"
[owner]
principal_id = "owner-principal"
identities = []
trust_local = false
"#;
        assert_eq!(
            classify(toml, CompanionIngress::trusted_local()),
            AuthorityClass::SharedOperator
        );
        assert_eq!(
            classify(toml, CompanionIngress::explicit("wechat:alice")),
            AuthorityClass::SharedOperator
        );
    }

    #[test]
    fn missing_companion_memory_section_defaults_closed() {
        let cfg: Config = toml::from_str("").expect("empty config parses");
        assert!(cfg.companion_memory.is_unset());
        assert!(cfg.companion_memory.owner.principal_id.is_empty());
        assert!(cfg.companion_memory.owner.identities.is_empty());
        assert!(!cfg.companion_memory.owner.trust_local);
        assert_eq!(
            classify_companion_authority(
                &CompanionIngress::trusted_local(),
                &cfg.companion_memory.owner.gate()
            ),
            AuthorityClass::SharedOperator
        );
    }

    #[test]
    fn companion_memory_owner_parses_on_the_config_root() {
        let cfg: Config = toml::from_str(
            r#"
[companion_memory.owner]
principal_id = "owner-principal"
identities = ["wechat:alice"]
trust_local = false
"#,
        )
        .expect("config parses with companion_memory.owner");
        assert_eq!(cfg.companion_memory.owner.principal_id, "owner-principal");
        assert_eq!(
            classify_companion_authority(
                &CompanionIngress::explicit("wechat:alice"),
                &cfg.companion_memory.owner.gate()
            ),
            AuthorityClass::OwnerAuthored
        );
        assert_eq!(
            classify_companion_authority(
                &CompanionIngress::explicit("wechat:bob"),
                &cfg.companion_memory.owner.gate()
            ),
            AuthorityClass::SharedOperator
        );
    }

    #[test]
    fn unknown_owner_field_is_rejected() {
        let err = toml::from_str::<CompanionOwnerConfig>(
            r#"
principal_id = "owner-principal"
trust_local = true
extra = true
"#,
        )
        .expect_err("deny_unknown_fields");
        let msg = err.to_string();
        assert!(msg.contains("extra") || msg.contains("unknown"), "{msg}");
    }

    #[test]
    fn owner_section_roundtrips_through_toml() {
        let raw = r#"
principal_id = "owner-principal"
identities = ["wechat:alice"]
trust_local = false
"#;
        let parsed: CompanionOwnerConfig = toml::from_str(raw).expect("parses");
        assert_eq!(parsed.principal_id, "owner-principal");
        assert_eq!(parsed.identities, vec!["wechat:alice".to_string()]);
        assert!(!parsed.trust_local);
        let gate = parsed.gate();
        assert_eq!(
            gate.principal().expect("set").id.as_str(),
            "owner-principal"
        );
    }
}

//! Companion-memory configuration: store enablement, location, and owner gate.
//!
//! `[companion_memory].enable` defaults to false. The store file is
//! `{store_dir}/companion-memory.db`, and `store_dir` defaults to
//! `{data_dir}/companion`. Capture and domain logic live in later slices.
//!
//! `[companion_memory.owner]` declares an opaque `principal_id` plus an
//! explicit ingress-identity list. A list hit is the owner. Everything else
//! is shared-operator and can never produce `owner_authored`. Single-user
//! convenience: an empty list plus `trust_local = true` treats Trusted
//! CLI/stdio/pairing as owner.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zeroclaw_api::companion::{CompanionOwnerGate, IngressIdentity};
use zeroclaw_api::principal::PrincipalId;
use zeroclaw_macros::Configurable;

/// Directory name under `data_dir` when `[companion_memory].store_dir` is unset.
pub const COMPANION_STORE_DIR_NAME: &str = "companion";
/// SQLite filename inside the companion store directory.
pub const COMPANION_MEMORY_DB_FILE: &str = "companion-memory.db";

fn is_false(value: &bool) -> bool {
    !*value
}

/// Top-level `[companion_memory]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
#[prefix = "companion_memory"]
pub struct CompanionMemoryConfig {
    /// Open the companion PortableKernel store. Compiled default is `false`.
    /// Feature-off builds still parse this flag but never open memcore.
    #[serde(default, skip_serializing_if = "is_false")]
    pub enable: bool,
    /// Directory holding `companion-memory.db`. Empty/absent means
    /// `{data_dir}/companion`. Relative paths are resolved against `data_dir`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_dir: Option<PathBuf>,
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

    /// Directory that will hold `companion-memory.db`.
    #[must_use]
    pub fn resolved_store_dir(&self, data_dir: &Path) -> PathBuf {
        match self
            .store_dir
            .as_ref()
            .filter(|p| !p.as_os_str().is_empty())
        {
            Some(dir) if dir.is_absolute() => dir.clone(),
            Some(dir) => data_dir.join(dir),
            None => data_dir.join(COMPANION_STORE_DIR_NAME),
        }
    }

    /// Full path of the companion PortableKernel database file.
    #[must_use]
    pub fn db_path(&self, data_dir: &Path) -> PathBuf {
        self.resolved_store_dir(data_dir)
            .join(COMPANION_MEMORY_DB_FILE)
    }

    /// # Errors
    /// Returns a message when owner admission is enabled without a principal.
    pub fn validate(&self) -> Result<(), String> {
        self.owner.validate()
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
    fn admits_owner(&self) -> bool {
        !self.identities.is_empty() || self.trust_local
    }

    /// Refuse owner admission that has no principal to stamp.
    ///
    /// Empty `principal_id` is allowed only when nobody is admitted (empty
    /// identity list and `trust_local = false`). That closed default must not
    /// silently become owner authority.
    ///
    /// # Errors
    /// Returns a message naming `principal_id` when admission is enabled
    /// without one.
    pub fn validate(&self) -> Result<(), String> {
        if self.admits_owner() && self.principal_id.trim().is_empty() {
            return Err(
                "companion_memory.owner.principal_id must be set when identities or trust_local admits an owner"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// Domain view of this section, for owner-gate classification.
    #[must_use]
    pub fn gate(&self) -> CompanionOwnerGate {
        CompanionOwnerGate {
            principal_id: PrincipalId::from(self.principal_id.trim()),
            identities: self
                .identities
                .iter()
                .map(|token| IngressIdentity::new(token.as_str()))
                .collect(),
            trust_local: self.trust_local,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Config;
    use zeroclaw_api::companion::{
        AuthorityClass, CompanionIngress, IngressIdentity, classify_companion_authority,
    };

    fn classify(toml: &str, ingress: CompanionIngress) -> AuthorityClass {
        let parsed: CompanionMemoryConfig = toml::from_str(toml).expect("owner section parses");
        parsed.owner.validate().expect("owner section validates");
        classify_companion_authority(&ingress, &parsed.owner.gate())
    }

    fn channel(identity: &str) -> CompanionIngress {
        CompanionIngress::from_channel_identity(IngressIdentity::new(identity))
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
            classify(toml, channel("wechat:alice")),
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
            classify(toml, channel("wechat:bob")),
            AuthorityClass::SharedOperator
        );
        assert_eq!(
            classify(toml, CompanionIngress::trusted_local_entry()),
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
            classify(toml, CompanionIngress::trusted_local_entry()),
            AuthorityClass::OwnerAuthored
        );
        assert_eq!(
            classify(toml, channel("wechat:stranger")),
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
            classify(toml, CompanionIngress::trusted_local_entry()),
            AuthorityClass::SharedOperator
        );
        assert_eq!(
            classify(toml, channel("wechat:alice")),
            AuthorityClass::SharedOperator
        );
    }

    #[test]
    fn missing_companion_memory_section_defaults_closed() {
        let cfg: Config = toml::from_str("").expect("empty config parses");
        assert!(cfg.companion_memory.is_unset());
        assert!(!cfg.companion_memory.enable);
        assert!(cfg.companion_memory.store_dir.is_none());
        assert!(cfg.companion_memory.owner.principal_id.is_empty());
        assert!(cfg.companion_memory.owner.identities.is_empty());
        assert!(!cfg.companion_memory.owner.trust_local);
        assert_eq!(
            classify_companion_authority(
                &CompanionIngress::trusted_local_entry(),
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
                &channel("wechat:alice"),
                &cfg.companion_memory.owner.gate()
            ),
            AuthorityClass::OwnerAuthored
        );
        assert_eq!(
            classify_companion_authority(
                &channel("wechat:bob"),
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

    #[test]
    fn closed_default_owner_validates() {
        assert!(CompanionOwnerConfig::default().validate().is_ok());
        assert!(CompanionMemoryConfig::default().validate().is_ok());
        let cfg: Config = toml::from_str("").expect("empty config parses");
        assert!(cfg.companion_memory.validate().is_ok());
    }

    #[test]
    fn owner_admission_without_principal_is_rejected_at_validate() {
        let cfg: Config = toml::from_str(
            r#"
[companion_memory.owner]
identities = ["wechat:alice"]
trust_local = false
"#,
        )
        .expect("parses with empty principal_id");
        let err = cfg
            .companion_memory
            .validate()
            .expect_err("admission without principal must deny");
        assert!(err.contains("principal_id"), "{err}");

        let through_config = cfg.validate().expect_err("Config::validate must deny");
        let msg = through_config.to_string();
        assert!(msg.contains("principal_id"), "{msg}");
    }

    #[test]
    fn unknown_companion_memory_field_is_rejected_on_config() {
        let err = toml::from_str::<Config>(
            r#"
[companion_memory]
nope = true
"#,
        )
        .expect_err("deny_unknown_fields on companion_memory");
        let msg = err.to_string();
        assert!(msg.contains("nope") || msg.contains("unknown"), "{msg}");
    }

    #[test]
    fn enable_defaults_false_and_is_not_unset_when_true() {
        let cfg: Config = toml::from_str("").expect("empty config parses");
        assert!(!cfg.companion_memory.enable);
        assert!(cfg.companion_memory.is_unset());

        let enabled: Config = toml::from_str(
            r#"
[companion_memory]
enable = true
"#,
        )
        .expect("enable parses");
        assert!(enabled.companion_memory.enable);
        assert!(!enabled.companion_memory.is_unset());
    }

    #[test]
    fn store_dir_defaults_under_data_dir_and_honors_explicit_path() {
        let cfg: Config = toml::from_str("").expect("empty");
        let data = std::path::Path::new("/tmp/zc-data");
        assert_eq!(
            cfg.companion_memory.resolved_store_dir(data),
            data.join("companion")
        );
        assert_eq!(
            cfg.companion_memory.db_path(data),
            data.join("companion").join("companion-memory.db")
        );

        let relative: CompanionMemoryConfig = toml::from_str(
            r#"
store_dir = "alt-companion"
"#,
        )
        .expect("relative store_dir parses");
        assert_eq!(
            relative.resolved_store_dir(data),
            data.join("alt-companion")
        );

        let absolute: CompanionMemoryConfig = toml::from_str(
            r#"
store_dir = "/var/lib/zeroclaw/companion"
"#,
        )
        .expect("absolute store_dir parses");
        assert_eq!(
            absolute.resolved_store_dir(data),
            std::path::PathBuf::from("/var/lib/zeroclaw/companion")
        );
    }
}

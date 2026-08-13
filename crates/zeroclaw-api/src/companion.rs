//! Companion-memory authority vocabulary and owner-gate types.
//!
//! These tokens are the host-owned half of memcore outbox metadata
//! (`authority_class`, `source_partition`). The kernel records them and does
//! not interpret them. Full User Model lifecycle (review, heads, projection)
//! is a later slice; this module freezes the lexicon those rows will stamp.

use serde::{Deserialize, Serialize};

use crate::principal::PrincipalId;

/// Under what authority a companion-memory mutation was made.
///
/// Serialized snake_case literals are the outbox `authority_class` tokens.
/// They are a closed vocabulary: `[a-z0-9_.-]`, at most 64 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorityClass {
    /// Explicit owner statement. May become an active revision immediately
    /// when the statement is unambiguous and the caller is the admitted owner.
    OwnerAuthored,
    /// A candidate the owner has explicitly accepted, narrowed, or superseded.
    OwnerRatified,
    /// Observed correction, behavior, repetition, or model inference.
    /// Candidate only; never auto-promotes by frequency or confidence.
    AgentInferred,
    /// One task-scoped request. A scoped override by default, not a durable
    /// preference.
    TaskScoped,
    /// Unmatched ingress. The shared-operator sentinel: it can never produce
    /// [`AuthorityClass::OwnerAuthored`].
    SharedOperator,
}

impl AuthorityClass {
    /// The outbox token for this class.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OwnerAuthored => "owner_authored",
            Self::OwnerRatified => "owner_ratified",
            Self::AgentInferred => "agent_inferred",
            Self::TaskScoped => "task_scoped",
            Self::SharedOperator => "shared_operator",
        }
    }
}

impl std::fmt::Display for AuthorityClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which companion-memory partition a mutation happened in.
///
/// Serialized snake_case literals are the outbox `source_partition` tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourcePartition {
    /// Owner values, goals, preferences, and scoped active heads.
    UserModel,
    /// Agent identity, dispositions, and persona projection.
    AgentSoul,
    /// Physically isolated private relationship store. Never enters the
    /// ordinary outbox.
    PrivateDyad,
    /// Shared-lexicon store that may travel with the dyad.
    SharedLexicon,
}

impl SourcePartition {
    /// The outbox token for this partition.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserModel => "user_model",
            Self::AgentSoul => "agent_soul",
            Self::PrivateDyad => "private_dyad",
            Self::SharedLexicon => "shared_lexicon",
        }
    }
}

impl std::fmt::Display for SourcePartition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Opaque per-agent identity. Minted once per agent alias and never rewritten
/// on alias or harness change. This type is the vocabulary slot; minting lives
/// elsewhere.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentIdentityId(String);

impl AgentIdentityId {
    /// Wrap an already-minted opaque id. Does not mint.
    #[must_use]
    pub fn from_opaque(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for AgentIdentityId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl From<&str> for AgentIdentityId {
    fn from(id: &str) -> Self {
        Self(id.to_owned())
    }
}

/// The declared companion owner. Distinct from a turn's ingress principal:
/// unmatched ingress is shared-operator and is never this value.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CompanionPrincipal {
    pub id: PrincipalId,
}

impl CompanionPrincipal {
    #[must_use]
    pub fn new(id: impl Into<PrincipalId>) -> Self {
        Self { id: id.into() }
    }
}

/// An opaque ingress-identity token comparable to
/// `[companion_memory.owner].identities`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IngressIdentity(String);

impl IngressIdentity {
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for IngressIdentity {
    fn from(token: String) -> Self {
        Self(token)
    }
}

impl From<&str> for IngressIdentity {
    fn from(token: &str) -> Self {
        Self(token.to_owned())
    }
}

/// How a turn arrived, for owner-gate matching.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompanionIngress {
    /// Channel/gateway sender token, when the ingress has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<IngressIdentity>,
    /// Trusted CLI / stdio / pairing. Eligible for owner only when the
    /// configured identity list is empty and `trust_local` is true.
    #[serde(default)]
    pub trusted_local: bool,
}

impl CompanionIngress {
    /// An explicit listed identity (channel sender, gateway user, …).
    #[must_use]
    pub fn explicit(identity: impl Into<IngressIdentity>) -> Self {
        Self {
            identity: Some(identity.into()),
            trusted_local: false,
        }
    }

    /// Trusted CLI / stdio / pairing, with no listed identity token.
    #[must_use]
    pub fn trusted_local() -> Self {
        Self {
            identity: None,
            trusted_local: true,
        }
    }
}

/// The owner gate: opaque principal id plus the matching rules.
///
/// Config deserializes into this shape; classification reads it. A list hit
/// is the owner. Everything else is shared-operator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompanionOwnerGate {
    /// Declared owner principal. Stamped on owner-authored rows; not used to
    /// match ingress.
    pub principal_id: PrincipalId,
    /// Explicit ingress-identity list. A hit is the owner.
    #[serde(default)]
    pub identities: Vec<IngressIdentity>,
    /// When the identity list is empty, treat Trusted CLI/stdio/pairing as
    /// owner.
    #[serde(default)]
    pub trust_local: bool,
}

impl Default for CompanionOwnerGate {
    fn default() -> Self {
        Self {
            principal_id: PrincipalId::from(""),
            identities: Vec::new(),
            trust_local: false,
        }
    }
}

impl CompanionOwnerGate {
    /// The declared owner principal, when `principal_id` is non-empty.
    #[must_use]
    pub fn principal(&self) -> Option<CompanionPrincipal> {
        let id = self.principal_id.as_str();
        if id.is_empty() {
            None
        } else {
            Some(CompanionPrincipal::new(self.principal_id.clone()))
        }
    }
}

/// Classify the caller's authority for companion-memory writes.
///
/// A hit on the explicit identity list is [`AuthorityClass::OwnerAuthored`].
/// Empty list plus `trust_local` treats Trusted CLI/stdio/pairing as owner.
/// Every other ingress is [`AuthorityClass::SharedOperator`] and can never
/// produce `owner_authored`.
///
/// This is the owner *gate*, not User Model content classification.
/// `owner_ratified` / `agent_inferred` / `task_scoped` are frozen tokens for
/// later slices; this helper does not emit them.
#[must_use]
pub fn classify_companion_authority(
    ingress: &CompanionIngress,
    owner: &CompanionOwnerGate,
) -> AuthorityClass {
    if is_declared_owner(ingress, owner) {
        AuthorityClass::OwnerAuthored
    } else {
        AuthorityClass::SharedOperator
    }
}

fn is_declared_owner(ingress: &CompanionIngress, owner: &CompanionOwnerGate) -> bool {
    if let Some(identity) = ingress
        .identity
        .as_ref()
        .map(IngressIdentity::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        && owner
            .identities
            .iter()
            .any(|listed| listed.as_str() == identity)
    {
        return true;
    }

    owner.identities.is_empty() && owner.trust_local && ingress.trusted_local
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes a memcore classification token may contain: `[a-z0-9_.-]`.
    const fn is_outbox_token_byte(byte: u8) -> bool {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'.' | b'-')
    }

    /// Memcore bounds a classification token at 64 bytes.
    const MAX_OUTBOX_CLASS_BYTES: usize = 64;

    fn assert_outbox_token(token: &str) {
        assert!(!token.is_empty(), "token must be nonempty");
        assert!(
            token.len() <= MAX_OUTBOX_CLASS_BYTES,
            "{token} is {} bytes, over the {MAX_OUTBOX_CLASS_BYTES}-byte class bound",
            token.len()
        );
        assert!(
            token.bytes().all(is_outbox_token_byte),
            "{token:?} is not a classification token"
        );
    }

    fn gate_with(identities: &[&str], trust_local: bool) -> CompanionOwnerGate {
        CompanionOwnerGate {
            principal_id: PrincipalId::from("owner-principal"),
            identities: identities
                .iter()
                .map(|s| IngressIdentity::from(*s))
                .collect(),
            trust_local,
        }
    }

    #[test]
    fn authority_class_serializes_snake_case_literals() {
        let cases = [
            (AuthorityClass::OwnerAuthored, "owner_authored"),
            (AuthorityClass::OwnerRatified, "owner_ratified"),
            (AuthorityClass::AgentInferred, "agent_inferred"),
            (AuthorityClass::TaskScoped, "task_scoped"),
            (AuthorityClass::SharedOperator, "shared_operator"),
        ];
        for (value, literal) in cases {
            let json = serde_json::to_string(&value).expect("serialize");
            assert_eq!(json, format!("\"{literal}\""));
            let back: AuthorityClass = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, value);
            assert_eq!(value.as_str(), literal);
            assert_outbox_token(literal);
        }
    }

    #[test]
    fn source_partition_serializes_snake_case_literals() {
        let cases = [
            (SourcePartition::UserModel, "user_model"),
            (SourcePartition::AgentSoul, "agent_soul"),
            (SourcePartition::PrivateDyad, "private_dyad"),
            (SourcePartition::SharedLexicon, "shared_lexicon"),
        ];
        for (value, literal) in cases {
            let json = serde_json::to_string(&value).expect("serialize");
            assert_eq!(json, format!("\"{literal}\""));
            let back: SourcePartition = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, value);
            assert_eq!(value.as_str(), literal);
            assert_outbox_token(literal);
        }
    }

    #[test]
    fn agent_identity_id_roundtrips_as_a_string() {
        let id = AgentIdentityId::from_opaque("550e8400-e29b-41d4-a716-446655440000");
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, "\"550e8400-e29b-41d4-a716-446655440000\"");
        let back: AgentIdentityId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, id);
    }

    #[test]
    fn explicit_list_hit_is_owner_authored() {
        let owner = gate_with(&["wechat:alice"], false);
        let ingress = CompanionIngress::explicit("wechat:alice");
        assert_eq!(
            classify_companion_authority(&ingress, &owner),
            AuthorityClass::OwnerAuthored
        );
    }

    #[test]
    fn explicit_list_miss_is_never_owner_authored() {
        let owner = gate_with(&["wechat:alice"], false);
        let ingress = CompanionIngress::explicit("wechat:bob");
        assert_eq!(
            classify_companion_authority(&ingress, &owner),
            AuthorityClass::SharedOperator
        );
    }

    #[test]
    fn unmatched_trusted_local_is_never_owner_authored_when_list_is_set() {
        let owner = gate_with(&["wechat:alice"], true);
        let ingress = CompanionIngress::trusted_local();
        assert_eq!(
            classify_companion_authority(&ingress, &owner),
            AuthorityClass::SharedOperator
        );
    }

    #[test]
    fn empty_list_trust_local_treats_trusted_cli_as_owner() {
        let owner = gate_with(&[], true);
        let ingress = CompanionIngress::trusted_local();
        assert_eq!(
            classify_companion_authority(&ingress, &owner),
            AuthorityClass::OwnerAuthored
        );
    }

    #[test]
    fn empty_list_trust_local_does_not_promote_untrusted_ingress() {
        let owner = gate_with(&[], true);
        let ingress = CompanionIngress::explicit("wechat:stranger");
        assert_eq!(
            classify_companion_authority(&ingress, &owner),
            AuthorityClass::SharedOperator
        );
    }

    #[test]
    fn empty_list_without_trust_local_never_yields_owner_authored() {
        let owner = gate_with(&[], false);
        for ingress in [
            CompanionIngress::trusted_local(),
            CompanionIngress::explicit("wechat:alice"),
            CompanionIngress {
                identity: None,
                trusted_local: false,
            },
        ] {
            assert_eq!(
                classify_companion_authority(&ingress, &owner),
                AuthorityClass::SharedOperator,
                "ingress {ingress:?}"
            );
        }
    }

    #[test]
    fn default_gate_never_yields_owner_authored() {
        let owner = CompanionOwnerGate::default();
        assert!(owner.principal_id.as_str().is_empty());
        assert!(owner.identities.is_empty());
        assert!(!owner.trust_local);
        assert_eq!(
            classify_companion_authority(&CompanionIngress::trusted_local(), &owner),
            AuthorityClass::SharedOperator
        );
        assert_eq!(
            classify_companion_authority(&CompanionIngress::explicit("anyone"), &owner),
            AuthorityClass::SharedOperator
        );
    }

    #[test]
    fn blank_ingress_identity_is_not_a_list_hit() {
        let owner = gate_with(&["wechat:alice"], false);
        let ingress = CompanionIngress::explicit("   ");
        assert_eq!(
            classify_companion_authority(&ingress, &owner),
            AuthorityClass::SharedOperator
        );
    }

    #[test]
    fn owner_gate_principal_is_absent_when_id_is_empty() {
        assert!(CompanionOwnerGate::default().principal().is_none());
        let owner = gate_with(&[], true);
        assert_eq!(
            owner.principal().expect("set").id.as_str(),
            "owner-principal"
        );
    }

    #[test]
    fn companion_ingress_roundtrips_through_json() {
        let ingress = CompanionIngress::explicit("telegram:42");
        let json = serde_json::to_string(&ingress).expect("serialize");
        let back: CompanionIngress = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, ingress);
    }
}

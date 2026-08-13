//! Companion-memory authority vocabulary and owner-gate types.
//!
//! These tokens are the host-owned half of memcore outbox metadata
//! (`authority_class`, `source_partition`). The kernel records them and does
//! not interpret them. Full User Model lifecycle (review, heads, projection)
//! is a later slice; this module freezes the lexicon those rows will stamp.
//!
//! The owner-gate classifier is a pure function with no I/O, so it lives next
//! to these types. Production construction of [`CompanionIngress`] is reserved
//! for the companion-capture entry — that is the only production call site
//! allowed to mint a trusted-local ingress. Capture is what keeps the
//! "logic stays where it runs" rule without moving the classifier out of the
//! types crate.

use serde::{Deserialize, Serialize};

use crate::principal::PrincipalId;

/// Under what authority a companion-memory mutation was made.
///
/// Serialized literals are the outbox `authority_class` tokens.
/// They are a closed vocabulary: `[a-z0-9_.-]`, at most 64 bytes.
/// [`AuthorityClass::SharedOperator`] uses `shared-operator` to match the
/// existing principal sentinel; every other variant is snake_case.
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
    #[serde(rename = "shared-operator")]
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
            Self::SharedOperator => "shared-operator",
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
/// [`SourcePartition::as_str`] is the storage and path name. Ordinary outbox
/// rows use [`SourcePartition::outbox_token`], which is `None` for the
/// physically isolated private dyad — that partition must never enter the
/// ordinary outbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourcePartition {
    /// Owner values, goals, preferences, and scoped active heads.
    UserModel,
    /// Agent identity, dispositions, and persona projection.
    AgentSoul,
    /// Physically isolated private relationship store. Storage name only;
    /// [`SourcePartition::outbox_token`] is `None`.
    PrivateDyad,
    /// Shared-lexicon store that may travel with the dyad.
    SharedLexicon,
}

impl SourcePartition {
    /// Storage and path name for this partition. Not an outbox admission
    /// token; use [`Self::outbox_token`] before writing an outbox row.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserModel => "user_model",
            Self::AgentSoul => "agent_soul",
            Self::PrivateDyad => "private_dyad",
            Self::SharedLexicon => "shared_lexicon",
        }
    }

    /// Outbox `source_partition` token, if this partition may enter the
    /// ordinary outbox. The private dyad is physically isolated and returns
    /// `None`.
    #[must_use]
    pub const fn outbox_token(self) -> Option<&'static str> {
        match self {
            Self::PrivateDyad => None,
            other => Some(other.as_str()),
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
///
/// Canonical form is trimmed ASCII-lowercase. Constructors normalize; matching
/// compares this form on both sides.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IngressIdentity(String);

impl IngressIdentity {
    /// Normalize `token` to trimmed ASCII-lowercase.
    #[must_use]
    pub fn new(token: impl AsRef<str>) -> Self {
        Self(token.as_ref().trim().to_ascii_lowercase())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for IngressIdentity {
    fn from(token: String) -> Self {
        Self::new(token)
    }
}

impl From<&str> for IngressIdentity {
    fn from(token: &str) -> Self {
        Self::new(token)
    }
}

/// How a turn arrived, for owner-gate matching.
///
/// Fields are private so a caller cannot stamp `trusted_local` onto a channel
/// identity. Construct only through the named constructors.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CompanionIngress {
    identity: Option<IngressIdentity>,
    trusted_local: bool,
}

impl CompanionIngress {
    /// Channel or gateway sender identity. `trusted_local` is always false.
    #[must_use]
    pub fn from_channel_identity(identity: IngressIdentity) -> Self {
        Self {
            identity: Some(identity),
            trusted_local: false,
        }
    }

    /// Trusted CLI / stdio / pairing entry.
    ///
    /// Production code may call this only from the companion-capture trusted
    /// entry path. Tests may construct it directly. Channel and gateway
    /// ingress must use [`Self::from_channel_identity`].
    #[must_use]
    pub fn trusted_local_entry() -> Self {
        Self {
            identity: None,
            trusted_local: true,
        }
    }

    #[must_use]
    pub fn identity(&self) -> Option<&IngressIdentity> {
        self.identity.as_ref()
    }

    #[must_use]
    pub fn is_trusted_local(&self) -> bool {
        self.trusted_local
    }
}

/// The owner gate: opaque principal id plus the matching rules.
///
/// Config deserializes into this shape; classification reads it. A list hit
/// is the owner. Everything else is shared-operator. An empty
/// `principal_id` can never produce [`AuthorityClass::OwnerAuthored`], even
/// when a list or `trust_local` would otherwise admit.
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
        let id = self.principal_id.as_str().trim();
        if id.is_empty() {
            None
        } else {
            Some(CompanionPrincipal::new(id))
        }
    }
}

/// Classify the caller's authority for companion-memory writes.
///
/// A hit on the explicit identity list is [`AuthorityClass::OwnerAuthored`].
/// Empty list plus `trust_local` treats Trusted CLI/stdio/pairing as owner.
/// Every other ingress is [`AuthorityClass::SharedOperator`] and can never
/// produce `owner_authored`. A missing `principal_id` also yields
/// [`AuthorityClass::SharedOperator`]: owner admission without a principal
/// cannot mint owner authority.
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
    if owner.principal_id.as_str().trim().is_empty() {
        return false;
    }

    if let Some(identity) = ingress
        .identity()
        .map(IngressIdentity::as_str)
        .filter(|token| !token.is_empty())
        && owner
            .identities
            .iter()
            .any(|listed| listed.as_str() == identity)
    {
        return true;
    }

    owner.identities.is_empty() && owner.trust_local && ingress.is_trusted_local()
}

/// Where a companion-capture turn arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureOrigin {
    /// Messaging-channel turn.
    Channel,
    /// Gateway WebSocket turn.
    Gateway,
    /// Trusted CLI / stdio / pairing entry.
    TrustedLocal,
}

/// Typed outcome of one companion-capture close-out.
///
/// Every variant is a durable receipt row. An empty store must not mean
/// "capture never ran."
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureOutcome {
    /// Capture ran; candidate evaluation is not implemented on this slice.
    NotEvaluated,
    /// Evaluation ran and found nothing worth keeping.
    NoCandidate,
    /// A candidate was refused by policy before any local write.
    CandidateRejectedByPolicy,
    /// The local write of the decided outcome failed.
    LocalWriteFailed,
    /// A candidate was written to the companion store.
    CandidatePersistedLocal,
}

impl CaptureOutcome {
    /// Stable storage / log token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotEvaluated => "not_evaluated",
            Self::NoCandidate => "no_candidate",
            Self::CandidateRejectedByPolicy => "candidate_rejected_by_policy",
            Self::LocalWriteFailed => "local_write_failed",
            Self::CandidatePersistedLocal => "candidate_persisted_local",
        }
    }
}

impl std::fmt::Display for CaptureOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Turn-scoped input to companion capture.
///
/// Construct only through the named constructors. Those are the production
/// minting points for [`CompanionIngress`]: channel/gateway identities use
/// [`CompanionIngress::from_channel_identity`]; trusted CLI uses
/// [`CompanionIngress::trusted_local_entry`]. Owner authority is classified
/// here via [`classify_companion_authority`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CaptureContext {
    agent_identity_id: AgentIdentityId,
    principal: CompanionPrincipal,
    session_id: String,
    turn_id: String,
    authority_class: AuthorityClass,
    origin: CaptureOrigin,
    partition: SourcePartition,
}

impl CaptureContext {
    /// Messaging-channel ingress. `trusted_local` is never set.
    #[must_use]
    pub fn from_channel_identity(
        agent_identity_id: AgentIdentityId,
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
        identity: IngressIdentity,
        owner: &CompanionOwnerGate,
    ) -> Self {
        Self::from_ingress(
            agent_identity_id,
            session_id,
            turn_id,
            CompanionIngress::from_channel_identity(identity),
            owner,
            CaptureOrigin::Channel,
            SourcePartition::UserModel,
        )
    }

    /// Gateway WebSocket ingress. `trusted_local` is never set.
    #[must_use]
    pub fn from_gateway_identity(
        agent_identity_id: AgentIdentityId,
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
        identity: IngressIdentity,
        owner: &CompanionOwnerGate,
    ) -> Self {
        Self::from_ingress(
            agent_identity_id,
            session_id,
            turn_id,
            CompanionIngress::from_channel_identity(identity),
            owner,
            CaptureOrigin::Gateway,
            SourcePartition::UserModel,
        )
    }

    /// Trusted CLI / stdio / pairing ingress. Production may call this only
    /// from the companion-capture trusted entry path.
    #[must_use]
    pub fn from_trusted_local(
        agent_identity_id: AgentIdentityId,
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
        owner: &CompanionOwnerGate,
    ) -> Self {
        Self::from_ingress(
            agent_identity_id,
            session_id,
            turn_id,
            CompanionIngress::trusted_local_entry(),
            owner,
            CaptureOrigin::TrustedLocal,
            SourcePartition::UserModel,
        )
    }

    /// Override the target partition. Capture receipts for
    /// [`SourcePartition::PrivateDyad`] never enter the ordinary outbox.
    #[must_use]
    pub fn with_partition(mut self, partition: SourcePartition) -> Self {
        self.partition = partition;
        self
    }

    fn from_ingress(
        agent_identity_id: AgentIdentityId,
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
        ingress: CompanionIngress,
        owner: &CompanionOwnerGate,
        origin: CaptureOrigin,
        partition: SourcePartition,
    ) -> Self {
        let authority_class = classify_companion_authority(&ingress, owner);
        let principal = if authority_class == AuthorityClass::OwnerAuthored {
            owner
                .principal()
                .unwrap_or_else(|| CompanionPrincipal::new(PrincipalId::SHARED_OPERATOR))
        } else {
            CompanionPrincipal::new(PrincipalId::SHARED_OPERATOR)
        };
        Self {
            agent_identity_id,
            principal,
            session_id: session_id.into(),
            turn_id: turn_id.into(),
            authority_class,
            origin,
            partition,
        }
    }

    #[must_use]
    pub fn agent_identity_id(&self) -> &AgentIdentityId {
        &self.agent_identity_id
    }

    #[must_use]
    pub fn principal(&self) -> &CompanionPrincipal {
        &self.principal
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[must_use]
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    #[must_use]
    pub fn authority_class(&self) -> AuthorityClass {
        self.authority_class
    }

    #[must_use]
    pub fn origin(&self) -> CaptureOrigin {
        self.origin
    }

    #[must_use]
    pub fn partition(&self) -> SourcePartition {
        self.partition
    }
}

/// Durable (or degraded) proof that capture ran for one turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureReceipt {
    pub outcome: CaptureOutcome,
    /// Outbox event id when the partition admits an ordinary outbox row.
    pub event_id: Option<String>,
    /// `memories.revision` when the receipt row landed.
    pub local_revision: Option<i64>,
    /// RFC3339 timestamp stamped at persist time (or degrade time).
    pub persisted_at: String,
}

impl CaptureReceipt {
    /// True when a `memories` row landed. A `local_write_failed` outcome can
    /// still be durable; only a missing revision means the store refused the
    /// receipt itself.
    #[must_use]
    pub fn is_durable(&self) -> bool {
        self.local_revision.is_some()
    }
}

/// Operator-visible companion outbox health. ZeroClaw-owned: never a memcore
/// type, and never a remote-sync report.
///
/// V1 has no Tachi drain. The only configured state is
/// [`CompanionOutboxStatus::Accumulating`]. A `synchronized` variant is
/// intentionally absent so success cannot be named, deserialized, or logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompanionOutboxStatus {
    /// No live store: feature off, `[companion_memory].enable` is false, or
    /// the factory returned `None`.
    NotConfigured,
    /// A PortableKernel store is open. Pending events may be waiting. This is
    /// the resting state for every configured V1 install.
    Accumulating,
}

impl CompanionOutboxStatus {
    /// Stable storage / JSON token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotConfigured => "not_configured",
            Self::Accumulating => "accumulating",
        }
    }
}

impl std::fmt::Display for CompanionOutboxStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Snapshot of local outbox debt. Counts and age are read-only facts about
/// `pending` rows; they do not imply a consumer ran.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompanionOutboxHealth {
    pub status: CompanionOutboxStatus,
    /// Events still in `pending`. Zero when [`Self::status`] is
    /// [`CompanionOutboxStatus::NotConfigured`].
    pub pending_count: u64,
    /// Age of the oldest `pending` event in seconds, if any exist.
    pub oldest_pending_age_secs: Option<u64>,
}

impl CompanionOutboxHealth {
    /// Closed store. No debt can be observed.
    #[must_use]
    pub fn not_configured() -> Self {
        Self {
            status: CompanionOutboxStatus::NotConfigured,
            pending_count: 0,
            oldest_pending_age_secs: None,
        }
    }

    /// Open store. `pending_count == 0` is still accumulating: V1 cannot
    /// represent a completed drain.
    #[must_use]
    pub fn accumulating(pending_count: u64, oldest_pending_age_secs: Option<u64>) -> Self {
        Self {
            status: CompanionOutboxStatus::Accumulating,
            pending_count,
            oldest_pending_age_secs,
        }
    }
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
                .map(|s| IngressIdentity::new(*s))
                .collect(),
            trust_local,
        }
    }

    fn channel(identity: &str) -> CompanionIngress {
        CompanionIngress::from_channel_identity(IngressIdentity::new(identity))
    }

    #[test]
    fn authority_class_serializes_closed_literals() {
        let cases = [
            (AuthorityClass::OwnerAuthored, "owner_authored"),
            (AuthorityClass::OwnerRatified, "owner_ratified"),
            (AuthorityClass::AgentInferred, "agent_inferred"),
            (AuthorityClass::TaskScoped, "task_scoped"),
            (AuthorityClass::SharedOperator, "shared-operator"),
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
    fn source_partition_storage_names_are_snake_case() {
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
        }
    }

    #[test]
    fn private_dyad_never_yields_an_outbox_token() {
        assert_eq!(SourcePartition::PrivateDyad.outbox_token(), None);
        assert_eq!(SourcePartition::PrivateDyad.as_str(), "private_dyad");
        for part in [
            SourcePartition::UserModel,
            SourcePartition::AgentSoul,
            SourcePartition::SharedLexicon,
        ] {
            let token = part.outbox_token().expect("ordinary partitions outbox");
            assert_eq!(token, part.as_str());
            assert_outbox_token(token);
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
        assert_eq!(
            classify_companion_authority(&channel("wechat:alice"), &owner),
            AuthorityClass::OwnerAuthored
        );
    }

    #[test]
    fn identity_match_is_ascii_case_insensitive() {
        let owner = gate_with(&["telegram:42"], false);
        assert_eq!(
            classify_companion_authority(&channel("Telegram:42"), &owner),
            AuthorityClass::OwnerAuthored
        );
    }

    #[test]
    fn explicit_list_miss_is_never_owner_authored() {
        let owner = gate_with(&["wechat:alice"], false);
        assert_eq!(
            classify_companion_authority(&channel("wechat:bob"), &owner),
            AuthorityClass::SharedOperator
        );
    }

    #[test]
    fn unmatched_trusted_local_is_never_owner_authored_when_list_is_set() {
        let owner = gate_with(&["wechat:alice"], true);
        assert_eq!(
            classify_companion_authority(&CompanionIngress::trusted_local_entry(), &owner),
            AuthorityClass::SharedOperator
        );
    }

    #[test]
    fn empty_list_trust_local_treats_trusted_cli_as_owner() {
        let owner = gate_with(&[], true);
        assert_eq!(
            classify_companion_authority(&CompanionIngress::trusted_local_entry(), &owner),
            AuthorityClass::OwnerAuthored
        );
    }

    #[test]
    fn empty_list_trust_local_does_not_promote_untrusted_ingress() {
        let owner = gate_with(&[], true);
        assert_eq!(
            classify_companion_authority(&channel("wechat:stranger"), &owner),
            AuthorityClass::SharedOperator
        );
    }

    #[test]
    fn empty_list_without_trust_local_never_yields_owner_authored() {
        let owner = gate_with(&[], false);
        for ingress in [
            CompanionIngress::trusted_local_entry(),
            channel("wechat:alice"),
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
            classify_companion_authority(&CompanionIngress::trusted_local_entry(), &owner),
            AuthorityClass::SharedOperator
        );
        assert_eq!(
            classify_companion_authority(&channel("anyone"), &owner),
            AuthorityClass::SharedOperator
        );
    }

    #[test]
    fn blank_ingress_identity_is_not_a_list_hit() {
        let owner = gate_with(&["wechat:alice"], false);
        assert_eq!(
            classify_companion_authority(&channel("   "), &owner),
            AuthorityClass::SharedOperator
        );
    }

    #[test]
    fn empty_principal_never_yields_owner_authored() {
        let owner = CompanionOwnerGate {
            principal_id: PrincipalId::from(""),
            identities: vec![IngressIdentity::new("wechat:alice")],
            trust_local: true,
        };
        assert_eq!(
            classify_companion_authority(&channel("wechat:alice"), &owner),
            AuthorityClass::SharedOperator
        );
        assert_eq!(
            classify_companion_authority(&CompanionIngress::trusted_local_entry(), &owner),
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
    fn channel_constructor_cannot_stamp_trusted_local() {
        let ingress = channel("wechat:alice");
        assert!(!ingress.is_trusted_local());
        assert_eq!(
            ingress.identity().map(IngressIdentity::as_str),
            Some("wechat:alice")
        );
    }

    #[test]
    fn capture_outcome_serializes_closed_literals() {
        let cases = [
            (CaptureOutcome::NotEvaluated, "not_evaluated"),
            (CaptureOutcome::NoCandidate, "no_candidate"),
            (
                CaptureOutcome::CandidateRejectedByPolicy,
                "candidate_rejected_by_policy",
            ),
            (CaptureOutcome::LocalWriteFailed, "local_write_failed"),
            (
                CaptureOutcome::CandidatePersistedLocal,
                "candidate_persisted_local",
            ),
        ];
        for (value, literal) in cases {
            let json = serde_json::to_string(&value).expect("serialize");
            assert_eq!(json, format!("\"{literal}\""));
            let back: CaptureOutcome = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, value);
            assert_eq!(value.as_str(), literal);
        }
    }

    #[test]
    fn capture_context_from_channel_classifies_owner_and_never_trusts_local() {
        let owner = gate_with(&["wechat:alice"], false);
        let ctx = CaptureContext::from_channel_identity(
            AgentIdentityId::from_opaque("agent-1"),
            "session-1",
            "turn-1",
            IngressIdentity::new("wechat:alice"),
            &owner,
        );
        assert_eq!(ctx.authority_class(), AuthorityClass::OwnerAuthored);
        assert_eq!(ctx.principal().id.as_str(), "owner-principal");
        assert_eq!(ctx.origin(), CaptureOrigin::Channel);
        assert_eq!(ctx.partition(), SourcePartition::UserModel);
        assert_eq!(ctx.session_id(), "session-1");
        assert_eq!(ctx.turn_id(), "turn-1");
    }

    #[test]
    fn capture_context_from_gateway_miss_is_shared_operator() {
        let owner = gate_with(&["wechat:alice"], false);
        let ctx = CaptureContext::from_gateway_identity(
            AgentIdentityId::from_opaque("agent-1"),
            "session-1",
            "turn-1",
            IngressIdentity::new("wss:stranger"),
            &owner,
        );
        assert_eq!(ctx.authority_class(), AuthorityClass::SharedOperator);
        assert_eq!(ctx.principal().id.as_str(), PrincipalId::SHARED_OPERATOR);
        assert_eq!(ctx.origin(), CaptureOrigin::Gateway);
    }

    #[test]
    fn capture_context_trusted_local_is_the_only_trusted_entry() {
        let owner = gate_with(&[], true);
        let ctx = CaptureContext::from_trusted_local(
            AgentIdentityId::from_opaque("agent-1"),
            "session-1",
            "turn-1",
            &owner,
        );
        assert_eq!(ctx.authority_class(), AuthorityClass::OwnerAuthored);
        assert_eq!(ctx.origin(), CaptureOrigin::TrustedLocal);
        assert_eq!(
            ctx.with_partition(SourcePartition::PrivateDyad).partition(),
            SourcePartition::PrivateDyad
        );
    }

    #[test]
    fn companion_outbox_status_has_no_synchronized_variant() {
        // Exhaustive match: a third variant (including anything named
        // synchronized) is a compile failure. V1 must not be able to name a
        // completed remote drain.
        for status in [
            CompanionOutboxStatus::NotConfigured,
            CompanionOutboxStatus::Accumulating,
        ] {
            match status {
                CompanionOutboxStatus::NotConfigured => {
                    assert_eq!(status.as_str(), "not_configured");
                }
                CompanionOutboxStatus::Accumulating => {
                    assert_eq!(status.as_str(), "accumulating");
                }
            }
        }
        assert_eq!(
            serde_json::to_string(&CompanionOutboxStatus::NotConfigured).expect("ser"),
            "\"not_configured\""
        );
        assert_eq!(
            serde_json::to_string(&CompanionOutboxStatus::Accumulating).expect("ser"),
            "\"accumulating\""
        );
        assert!(serde_json::from_str::<CompanionOutboxStatus>("\"synchronized\"").is_err());
        assert!(serde_json::from_str::<CompanionOutboxStatus>("\"pending\"").is_err());
    }

    #[test]
    fn companion_outbox_health_json_cannot_say_synchronized() {
        let closed = CompanionOutboxHealth::not_configured();
        assert_eq!(closed.status, CompanionOutboxStatus::NotConfigured);
        assert_eq!(closed.pending_count, 0);
        assert_eq!(closed.oldest_pending_age_secs, None);

        let open = CompanionOutboxHealth::accumulating(3, Some(12));
        let json = serde_json::to_string(&open).expect("ser");
        assert!(json.contains("accumulating"), "{json}");
        assert!(!json.contains("synchronized"), "{json}");
        assert_eq!(open.pending_count, 3);
        assert_eq!(open.oldest_pending_age_secs, Some(12));
    }
}

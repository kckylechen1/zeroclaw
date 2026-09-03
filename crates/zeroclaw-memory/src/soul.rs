//! AgentSoul domain seam — identity-bound Soul storage (#52 / #187).
//!
//! Soul is the reviewed operating disposition of ONE persistent agent
//! identity, carried unchanged across replaceable model / provider /
//! harness carriers. This leaf defines only the admission seam and the
//! identity-binding storage surface:
//!
//! - the identity vocabulary type is [`AgentIdentityId`] from
//!   `zeroclaw_api::companion` (the tachi#1630 substrate's minted-once
//!   identity slot) — this module never mints or derives identities,
//!   it only admits and resolves them;
//! - every namespace read/write resolves to a stable admitted identity
//!   first and fails closed when that identity is missing, ambiguous,
//!   revoked, or malformed;
//! - the storage namespace is derived from the identity alone — carrier
//!   attributes (model, provider, harness, session, display name) are
//!   structurally irrelevant to the key;
//! - no disposition capture, review/promotion, persona projection, or
//!   reset/erase lifecycle exists here (later leaves under #52);
//! - durability is delegated to the existing [`Memory`] backend; this
//!   module introduces no new persistence, revision, or receipt engine.
//!
//! Known limitation (recorded, not fixed in this leaf): row-attribution
//! verification is only as honest as the backend. `AgentScopedMarkdown
//! Memory` stamps reads with its own bound alias instead of the stored
//! attribution, so Soul isolation over that backend rests on the
//! identity-namespaced key alone. Backends with true composite
//! attribution (SQLite) enforce it structurally.
//!
//! Verified identity evidence from Tachi (envelope contract tachi#1667)
//! is NOT wired yet: the [`IdentityRegistry`] seam keeps a provenance
//! note per admission so external evidence can attach later without
//! widening this API.

use crate::traits::{Memory, MemoryCategory};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};
use zeroclaw_api::companion::AgentIdentityId;
use zeroclaw_api::memory_traits::MemoryEntry;

/// Typed fail-closed errors for the Soul seam. No variant ever falls back
/// to an alias, model, display name, or "best guess" identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoulError {
    /// No admitted identity could be resolved from the request context.
    IdentityUnavailable,
    /// More than one active identity matched the resolution input.
    IdentityAmbiguous,
    /// The identity was admitted once but is revoked or suspended.
    IdentityRevoked(String),
    /// The identity token is malformed for Soul namespacing (empty, or
    /// containing the `::` namespace delimiter).
    InvalidIdentityToken(String),
    /// The operation targets a row bound to a different identity.
    IdentityMismatch,
    /// The memory backend rejected the operation. The identity resolved
    /// fine; durability is the failure.
    Backend(String),
}

impl SoulError {
    fn message(&self) -> &str {
        match self {
            Self::IdentityUnavailable => {
                "agent identity unavailable: no admitted identity resolved"
            }
            Self::IdentityAmbiguous => {
                "agent identity ambiguous: multiple active candidates resolved"
            }
            Self::IdentityRevoked(_) => "agent identity revoked: no active Soul access",
            Self::InvalidIdentityToken(_) => "invalid agent identity token for Soul namespacing",
            Self::IdentityMismatch => "identity mismatch: operation targets another agent identity",
            Self::Backend(_) => "soul storage backend rejected the operation",
        }
    }
}

impl fmt::Display for SoulError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IdentityRevoked(id) => write!(f, "{}: {id}", self.message()),
            Self::InvalidIdentityToken(id) => write!(f, "{}: {id}", self.message()),
            Self::Backend(detail) => write!(f, "{}: {detail}", self.message()),
            _ => f.write_str(self.message()),
        }
    }
}

impl std::error::Error for SoulError {}

/// The namespace delimiter must never appear inside an identity token,
/// so `soul::<identity>::<key>` has exactly one parse and no `(id, key)`
/// pair can collide with a different pair.
const NAMESPACE_DELIMITER: &str = "::";

fn validate_identity_token(id: &AgentIdentityId) -> Result<(), SoulError> {
    let token = id.as_str();
    if token.is_empty() {
        Err(SoulError::InvalidIdentityToken("(empty)".to_string()))
    } else if token.contains(NAMESPACE_DELIMITER) {
        Err(SoulError::InvalidIdentityToken(token.to_string()))
    } else {
        Ok(())
    }
}

/// Admission lifecycle of one identity in the local registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionStatus {
    Active,
    Revoked,
}

/// Why/how an identity was admitted. Free-form for locally admitted
/// identities today; reserved as the attachment point for verified
/// Tachi evidence envelopes (tachi#1667) without widening the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionRecord {
    pub status: AdmissionStatus,
    pub provenance: String,
}

/// Local admission registry: the fail-closed resolver every Soul
/// read/write must pass through. An identity that was never admitted,
/// or was admitted and then revoked, resolves to a typed error — never
/// to a fallback key. Lock poisoning is absorbed rather than panicking:
/// resolution continues on the last consistent snapshot.
#[derive(Debug, Default)]
pub struct IdentityRegistry {
    admitted: RwLock<HashMap<AgentIdentityId, AdmissionRecord>>,
}

impl IdentityRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock_read(
        &self,
    ) -> std::sync::RwLockReadGuard<'_, HashMap<AgentIdentityId, AdmissionRecord>> {
        self.admitted.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_write(
        &self,
    ) -> std::sync::RwLockWriteGuard<'_, HashMap<AgentIdentityId, AdmissionRecord>> {
        self.admitted
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Admit (or re-admit after revocation) a stable identity, recording
    /// how it was admitted. The token must be Soul-namespacable.
    pub fn admit(
        &self,
        id: &AgentIdentityId,
        provenance: impl Into<String>,
    ) -> Result<(), SoulError> {
        validate_identity_token(id)?;
        self.lock_write().insert(
            id.clone(),
            AdmissionRecord {
                status: AdmissionStatus::Active,
                provenance: provenance.into(),
            },
        );
        Ok(())
    }

    /// Revoke an admitted identity. The record is kept (revocation is a
    /// state, not a deletion) so re-admission is an explicit act and a
    /// stale active Soul can never be resurrected by re-insertion.
    pub fn revoke(&self, id: &AgentIdentityId) {
        if let Some(record) = self.lock_write().get_mut(id) {
            record.status = AdmissionStatus::Revoked;
        }
    }

    /// Resolve one candidate identity. `None` (the caller could not
    /// resolve any identity from its carrier context) fails closed as
    /// [`SoulError::IdentityUnavailable`]; a revoked identity fails as
    /// [`SoulError::IdentityRevoked`].
    pub fn resolve(
        &self,
        candidate: Option<&AgentIdentityId>,
    ) -> Result<AgentIdentityId, SoulError> {
        let id = candidate.ok_or(SoulError::IdentityUnavailable)?;
        self.resolve_active(id).cloned()
    }

    /// Resolve exactly one identity from a candidate set. Zero admitted
    /// candidates fail unavailable; more than one ACTIVE candidate fails
    /// ambiguous; a single revoked candidate keeps its typed revoked
    /// error instead of collapsing into "unavailable".
    pub fn resolve_exactly(
        &self,
        candidates: &[AgentIdentityId],
    ) -> Result<AgentIdentityId, SoulError> {
        let admitted = self.lock_read();
        let mut active: Vec<&AgentIdentityId> = Vec::new();
        let mut revoked: Option<&AgentIdentityId> = None;
        for id in candidates {
            match admitted.get(id).map(|r| r.status) {
                Some(AdmissionStatus::Active) => active.push(id),
                Some(AdmissionStatus::Revoked) => revoked = Some(id),
                None => {}
            }
        }
        match active.as_slice() {
            [one] => Ok((*one).clone()),
            [] => match revoked {
                Some(revoked_id) => {
                    Err(SoulError::IdentityRevoked(revoked_id.as_str().to_string()))
                }
                None => Err(SoulError::IdentityUnavailable),
            },
            _ => Err(SoulError::IdentityAmbiguous),
        }
    }

    fn resolve_active<'a>(
        &self,
        id: &'a AgentIdentityId,
    ) -> Result<&'a AgentIdentityId, SoulError> {
        let admitted = self.lock_read();
        match admitted.get(id) {
            Some(record) if record.status == AdmissionStatus::Active => Ok(id),
            Some(_) => Err(SoulError::IdentityRevoked(id.as_str().to_string())),
            None => Err(SoulError::IdentityUnavailable),
        }
    }
}

/// Carrier attributes a Soul deliberately ignores. Present in the API so
/// call sites can state their context explicitly and tests can prove the
/// namespace is invariant under carrier replacement — model, provider,
/// harness, session, and display name never enter the derived key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CarrierContext {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub harness: Option<String>,
    pub session: Option<String>,
    pub display_name: Option<String>,
}

/// Stable marker the backend-side foreign-agent refusal is recognized by.
/// The source message is stable text in `AgentScopedMemory`; if it ever
/// changes, this mapping degrades to a `Backend` error (loud, typed, and
/// greppable) rather than a wrong identity verdict.
const FOREIGN_AGENT_REFUSAL_MARKER: &str = "refuses store_with_agent for foreign agent_id";

/// Identity-bound Soul storage over an existing [`Memory`] backend.
/// Every method first resolves the caller's identity through the
/// [`IdentityRegistry`] and fails closed on unavailable / ambiguous /
/// revoked / malformed identities; rows are attributed to the resolved
/// identity and read back through the agent-scoped composite lookup, so
/// sibling identities are structurally invisible to each other.
pub struct SoulService {
    registry: Arc<IdentityRegistry>,
    backend: Arc<dyn Memory>,
}

impl SoulService {
    #[must_use]
    pub fn new(registry: Arc<IdentityRegistry>, backend: Arc<dyn Memory>) -> Self {
        Self { registry, backend }
    }

    /// Derived namespace key: identity ONLY. `CarrierContext` is accepted
    /// to make the invariance explicit at the call site and is ignored.
    /// The identity token is validated to be non-empty and delimiter-free
    /// so no `(identity, key)` pair can collide with another pair.
    #[must_use]
    pub fn namespace_key(
        identity: &AgentIdentityId,
        key: &str,
        _carrier: &CarrierContext,
    ) -> String {
        format!(
            "soul{NAMESPACE_DELIMITER}{}{NAMESPACE_DELIMITER}{key}",
            identity.as_str()
        )
    }

    /// Store one Soul row for the resolved identity.
    pub async fn store(
        &self,
        identity: &AgentIdentityId,
        key: &str,
        content: &str,
        carrier: &CarrierContext,
    ) -> Result<(), SoulError> {
        let resolved = self.registry.resolve(Some(identity))?;
        validate_identity_token(&resolved)?;
        let namespaced = Self::namespace_key(&resolved, key, carrier);
        if let Err(e) = self
            .backend
            .store_with_agent(
                &namespaced,
                content,
                MemoryCategory::Core,
                None,
                Some("soul"),
                None,
                Some(resolved.as_str()),
            )
            .await
        {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "error_key": "memory.soul_store_failed",
                        "identity": resolved.as_str(),
                        "err": e.to_string(),
                    })),
                "soul store failed through the memory backend"
            );
            return Err(classify_backend_error(&e));
        }
        Ok(())
    }

    /// Read one Soul row for the resolved identity through the
    /// agent-scoped composite lookup: rows attributed to sibling
    /// identities are invisible, and a missing row is `None`.
    pub async fn get(
        &self,
        identity: &AgentIdentityId,
        key: &str,
        carrier: &CarrierContext,
    ) -> Result<Option<MemoryEntry>, SoulError> {
        let resolved = self.registry.resolve(Some(identity))?;
        validate_identity_token(&resolved)?;
        let namespaced = Self::namespace_key(&resolved, key, carrier);
        match self
            .backend
            .get_for_agent(&namespaced, resolved.as_str())
            .await
        {
            Ok(entry) => Ok(entry),
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "error_key": "memory.soul_get_failed",
                            "identity": resolved.as_str(),
                            "err": e.to_string(),
                        })),
                    "soul get failed through the memory backend"
                );
                Err(SoulError::Backend(e.to_string()))
            }
        }
    }

    /// Remove one Soul row for the resolved identity.
    pub async fn forget(
        &self,
        identity: &AgentIdentityId,
        key: &str,
        carrier: &CarrierContext,
    ) -> Result<bool, SoulError> {
        let resolved = self.registry.resolve(Some(identity))?;
        validate_identity_token(&resolved)?;
        let namespaced = Self::namespace_key(&resolved, key, carrier);
        match self
            .backend
            .forget_for_agent(&namespaced, resolved.as_str())
            .await
        {
            Ok(removed) => Ok(removed),
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "error_key": "memory.soul_forget_failed",
                            "identity": resolved.as_str(),
                            "err": e.to_string(),
                        })),
                    "soul forget failed through the memory backend"
                );
                Err(SoulError::Backend(e.to_string()))
            }
        }
    }
}

/// A backend refusal caused by identity (a wrapper bound to another
/// agent refusing our attribution) is an identity verdict, not a
/// durability failure.
fn classify_backend_error(e: &anyhow::Error) -> SoulError {
    if e.to_string().contains(FOREIGN_AGENT_REFUSAL_MARKER) {
        SoulError::IdentityMismatch
    } else {
        SoulError::Backend(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::SqliteMemory;

    fn fresh_backend() -> (tempfile::TempDir, Arc<SqliteMemory>) {
        let tmp = tempfile::TempDir::new().unwrap();
        let mem = SqliteMemory::new("soul-test", tmp.path()).unwrap();
        (tmp, Arc::new(mem))
    }

    fn service(backend: Arc<SqliteMemory>) -> (Arc<IdentityRegistry>, SoulService) {
        let registry = Arc::new(IdentityRegistry::new());
        let soul = SoulService::new(
            Arc::clone(&registry) as Arc<IdentityRegistry>,
            backend as Arc<dyn Memory>,
        );
        (registry, soul)
    }

    /// Admit-through-backend identities: SqliteMemory rejects rows
    /// attributed to agent ids it never registered, so Soul tests use
    /// backend-minted uuids as their stable identity tokens.
    async fn identity_for(backend: &Arc<SqliteMemory>, alias: &str) -> AgentIdentityId {
        AgentIdentityId::from_opaque(backend.ensure_agent_uuid(alias).await.unwrap())
    }

    fn carriers() -> [CarrierContext; 3] {
        [
            CarrierContext {
                model: Some("glm-5.3".into()),
                provider: Some("bigmodel".into()),
                harness: Some("zeroclaw".into()),
                session: Some("s-1".into()),
                display_name: Some("Aria".into()),
            },
            CarrierContext {
                model: Some("kimi-k2.6".into()),
                provider: Some("moonshot".into()),
                harness: Some("zeroclaw-tui".into()),
                session: Some("s-2".into()),
                display_name: Some("Aria".into()),
            },
            CarrierContext {
                model: Some("deepseek-v4".into()),
                provider: Some("deepseek".into()),
                harness: Some("acp-carrier".into()),
                session: Some("s-3".into()),
                display_name: Some("Totally Different Name".into()),
            },
        ]
    }

    #[tokio::test]
    async fn same_identity_across_carriers_shares_one_namespace() {
        // Discrimination #1 (issue #187): swap model/provider/harness/
        // session while preserving the admitted identity -> same domain
        // key and state.
        let (tmp, backend) = fresh_backend();
        let (registry, soul) = service(Arc::clone(&backend));
        let id = identity_for(&backend, "identity-a").await;
        registry.admit(&id, "local bootstrap").unwrap();

        let [first, second, third] = carriers();
        soul.store(&id, "disposition", "direct and warm", &first)
            .await
            .unwrap();
        for carrier in [&second, &third] {
            let entry = soul.get(&id, "disposition", carrier).await.unwrap();
            assert_eq!(
                entry
                    .expect("same identity must see its row under any carrier")
                    .content,
                "direct and warm",
                "carrier swap must not change the Soul namespace"
            );
        }
        drop(tmp);
    }

    #[tokio::test]
    async fn different_identity_cannot_read_or_write_neighbors_namespace() {
        // Discrimination #2: a different identity has zero access to the
        // first identity's namespace, in both directions.
        let (tmp, backend) = fresh_backend();
        let (registry, soul) = service(Arc::clone(&backend));
        let a = identity_for(&backend, "identity-a").await;
        let b = identity_for(&backend, "identity-b").await;
        registry.admit(&a, "local bootstrap").unwrap();
        registry.admit(&b, "local bootstrap").unwrap();

        soul.store(
            &a,
            "disposition",
            "A's disposition",
            &CarrierContext::default(),
        )
        .await
        .unwrap();
        // B cannot see A's row under the same logical key.
        assert!(
            soul.get(&b, "disposition", &CarrierContext::default())
                .await
                .unwrap()
                .is_none(),
            "identity B must not read identity A's Soul namespace"
        );
        // B storing the same logical key writes its OWN row; A's is
        // untouched, and B's forget removes only B's row.
        soul.store(
            &b,
            "disposition",
            "B's disposition",
            &CarrierContext::default(),
        )
        .await
        .unwrap();
        let a_row = soul
            .get(&a, "disposition", &CarrierContext::default())
            .await
            .unwrap()
            .expect("A's row must survive B's write");
        assert_eq!(a_row.content, "A's disposition");
        assert!(
            soul.forget(&b, "disposition", &CarrierContext::default())
                .await
                .unwrap(),
            "B forgets its own row"
        );
        assert!(
            soul.get(&b, "disposition", &CarrierContext::default())
                .await
                .unwrap()
                .is_none(),
            "B's row is gone"
        );
        assert!(
            soul.get(&a, "disposition", &CarrierContext::default())
                .await
                .unwrap()
                .is_some(),
            "A's row survives B's forget"
        );
        // Reverse direction: A forgetting its own row leaves B's row
        // (re-stored for this half) untouched.
        soul.store(
            &b,
            "disposition",
            "B's disposition",
            &CarrierContext::default(),
        )
        .await
        .unwrap();
        assert!(
            soul.forget(&a, "disposition", &CarrierContext::default())
                .await
                .unwrap(),
            "A forgets its own row"
        );
        let b_row = soul
            .get(&b, "disposition", &CarrierContext::default())
            .await
            .unwrap()
            .expect("B's row must survive A's forget");
        assert_eq!(b_row.content, "B's disposition");
        assert!(
            soul.get(&a, "disposition", &CarrierContext::default())
                .await
                .unwrap()
                .is_none(),
            "A's row is gone after its own forget"
        );
        drop(tmp);
    }

    #[tokio::test]
    async fn same_display_name_with_different_identity_is_denied() {
        // Discrimination #3: an identical display name with a different
        // identity is a different Soul — zero access to A's state.
        let (tmp, backend) = fresh_backend();
        let (registry, soul) = service(Arc::clone(&backend));
        let a = identity_for(&backend, "identity-a").await;
        let b = identity_for(&backend, "identity-b").await;
        registry.admit(&a, "local bootstrap").unwrap();
        registry.admit(&b, "local bootstrap").unwrap();

        let a_carrier = CarrierContext {
            display_name: Some("Aria".into()),
            ..CarrierContext::default()
        };
        let b_carrier = CarrierContext {
            display_name: Some("Aria".into()),
            ..CarrierContext::default()
        };

        soul.store(&a, "disposition", "A's disposition", &a_carrier)
            .await
            .unwrap();
        let same_name_b = soul.get(&b, "disposition", &b_carrier).await.unwrap();
        assert!(
            same_name_b.is_none(),
            "same display name must not bridge two identities"
        );
        drop(tmp);
    }

    #[tokio::test]
    async fn unresolved_or_revoked_identity_fails_closed() {
        let (tmp, backend) = fresh_backend();
        let (registry, soul) = service(backend);

        // Missing identity: no fallback to alias/model/display name.
        let ghost = AgentIdentityId::from_opaque("never-admitted");
        assert_eq!(
            soul.store(&ghost, "k", "v", &CarrierContext::default())
                .await,
            Err(SoulError::IdentityUnavailable),
            "unadmitted identity must fail closed"
        );
        assert_eq!(
            soul.forget(&ghost, "k", &CarrierContext::default()).await,
            Err(SoulError::IdentityUnavailable),
            "unadmitted identity must fail closed on forget too"
        );

        // None-shaped resolution (caller could not resolve anything).
        assert_eq!(
            registry.resolve(None).unwrap_err(),
            SoulError::IdentityUnavailable
        );

        // Ambiguous multi-candidate resolution.
        let a = AgentIdentityId::from_opaque("identity-A");
        let b = AgentIdentityId::from_opaque("identity-B");
        registry.admit(&a, "local").unwrap();
        registry.admit(&b, "local").unwrap();
        assert_eq!(
            registry
                .resolve_exactly(&[a.clone(), b.clone()])
                .unwrap_err(),
            SoulError::IdentityAmbiguous
        );

        // Revoked identity projects no active Soul access — through the
        // single-candidate path AND the multi-candidate path, which must
        // keep the typed revoked error instead of degrading to
        // "unavailable".
        registry.revoke(&a);
        let revoked = soul
            .get(&a, "k", &CarrierContext::default())
            .await
            .unwrap_err();
        assert_eq!(
            revoked,
            SoulError::IdentityRevoked("identity-A".to_string()),
            "revoked identity must be denied, not fallen back"
        );
        assert_eq!(
            registry
                .resolve_exactly(std::slice::from_ref(&a))
                .unwrap_err(),
            SoulError::IdentityRevoked("identity-A".to_string()),
            "single revoked candidate keeps its typed revoked error"
        );
        assert_eq!(
            soul.store(&a, "k", "v", &CarrierContext::default()).await,
            Err(SoulError::IdentityRevoked("identity-A".to_string())),
            "revoked identity must fail closed on store"
        );
        assert_eq!(
            soul.forget(&a, "k", &CarrierContext::default()).await,
            Err(SoulError::IdentityRevoked("identity-A".to_string())),
            "revoked identity must fail closed on forget"
        );
        drop(tmp);
    }

    #[tokio::test]
    async fn malformed_identity_tokens_are_rejected_not_namespaced() {
        // The delimiter must never reach the key derivation, or
        // (id="a::b", key="c") would collide with (id="a", key="b::c").
        let (tmp, backend) = fresh_backend();
        let (registry, soul) = service(backend);

        let collision = AgentIdentityId::from_opaque("identity-a::suffix");
        assert_eq!(
            registry.admit(&collision, "local").unwrap_err(),
            SoulError::InvalidIdentityToken("identity-a::suffix".to_string()),
            "delimiter-bearing tokens cannot be admitted"
        );
        assert_eq!(
            soul.store(&collision, "k", "v", &CarrierContext::default())
                .await,
            Err(SoulError::IdentityUnavailable),
            "unadmitted malformed token still fails closed"
        );

        let empty = AgentIdentityId::from_opaque("");
        assert_eq!(
            registry.admit(&empty, "local").unwrap_err(),
            SoulError::InvalidIdentityToken("(empty)".to_string())
        );
        drop(tmp);
    }

    #[test]
    fn identity_token_validator_discriminates() {
        // The store/get/forget validators are defense-in-depth behind
        // admit(); this pins the validator itself so the depth is real,
        // not decorative.
        let ok = AgentIdentityId::from_opaque("identity-a");
        assert_eq!(validate_identity_token(&ok), Ok(()));
        let delimited = AgentIdentityId::from_opaque("a::b");
        assert_eq!(
            validate_identity_token(&delimited),
            Err(SoulError::InvalidIdentityToken("a::b".to_string()))
        );
        let empty = AgentIdentityId::from_opaque("");
        assert_eq!(
            validate_identity_token(&empty),
            Err(SoulError::InvalidIdentityToken("(empty)".to_string()))
        );
    }

    #[test]
    fn namespace_key_ignores_every_carrier_attribute() {
        let id = AgentIdentityId::from_opaque("identity-a");
        let [a, b, c] = carriers();
        assert_eq!(
            SoulService::namespace_key(&id, "disposition", &a),
            SoulService::namespace_key(&id, "disposition", &b)
        );
        assert_eq!(
            SoulService::namespace_key(&id, "disposition", &a),
            SoulService::namespace_key(&id, "disposition", &c)
        );
    }
}

use super::{ApprovalStore, DEFAULT_GRANT_TTL_SECS, args_hash};
use anyhow::{Result, ensure};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use zeroclaw_api::device_identity::{DeviceIdentityV1, DeviceRole};

/// Node-only storage variants; neither variant authenticates its authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeGrantKind {
    NodeCapability,
    TachiProjected,
}
impl NodeGrantKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::NodeCapability => "node_capability",
            Self::TachiProjected => "tachi_projected",
        }
    }
}

/// An already authenticated and authorized projection, not a request to mint authority.
/// The upstream stable ID is used directly; no local alias is generated.
#[derive(Debug, Clone)]
pub struct NodeGrantProjection {
    pub grant_id: String,
    pub kind: NodeGrantKind,
    pub device_id: String,
    pub identity_epoch: u64,
    pub capability: String,
    pub args_hash: String,
    pub nonce: String,
    pub granted_at: DateTime<Utc>,
    /// None uses the frozen 300-second lifetime; an explicit lifetime is at most 15 minutes.
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// The caller must resolve an active Node-capable identity from the canonical identity owner.
/// Connection/call/revision are proposed first-claim bindings, not authority on their own.
pub struct NodeGrantClaim<'a> {
    pub grant_id: &'a str,
    pub kind: NodeGrantKind,
    pub identity: &'a DeviceIdentityV1,
    pub capability: &'a str,
    pub args: &'a serde_json::Value,
    pub nonce: &'a str,
    pub connection_id: &'a str,
    pub cap_revision: u64,
    pub call_id: &'a str,
}

/// Local committed claim evidence. Not an execution-success receipt or signed grant proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedNodeGrant {
    pub grant_id: String,
    pub kind: NodeGrantKind,
    pub device_id: String,
    pub identity_epoch: u64,
    pub capability: String,
    pub args_hash: String,
    pub nonce: String,
    pub granted_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub claimed_at: DateTime<Utc>,
    pub connection_id: String,
    pub cap_revision: u64,
    pub call_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeClaimFailure {
    NotClaimable,
    AlreadyClaimed,
}

fn epoch(value: u64) -> Option<i64> {
    i64::try_from(value).ok()
}
fn timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|v| v.with_timezone(&Utc))
}
fn revision(value: &str) -> Option<u64> {
    let parsed: u64 = value.parse().ok()?;
    (parsed.to_string() == value).then_some(parsed)
}

struct StoredClaim {
    granted: String,
    expires: String,
    revoked: Option<String>,
    consumed: Option<String>,
    connection: Option<String>,
    revision: Option<String>,
    call: Option<String>,
}
impl StoredClaim {
    fn state(
        &self,
        now: DateTime<Utc>,
    ) -> Result<(DateTime<Utc>, DateTime<Utc>), NodeClaimFailure> {
        use NodeClaimFailure::{AlreadyClaimed, NotClaimable};
        let granted = timestamp(&self.granted).ok_or(NotClaimable)?;
        let expires = timestamp(&self.expires).ok_or(NotClaimable)?;
        if expires <= now
            || expires <= granted
            || expires - granted > Duration::minutes(15)
            || self.revoked.is_some()
        {
            return Err(NotClaimable);
        }
        match (&self.consumed, &self.connection, &self.revision, &self.call) {
            (None, None, None, None) => Ok((granted, expires)),
            (Some(consumed), Some(connection), Some(rev), Some(call))
                if timestamp(consumed).is_some()
                    && !connection.is_empty()
                    && revision(rev).is_some()
                    && !call.is_empty() =>
            {
                Err(AlreadyClaimed)
            }
            _ => Err(NotClaimable),
        }
    }
}

impl ApprovalStore {
    /// Store a trusted Node projection without minting or authenticating authority.
    /// A future adapter must authenticate/authorize its source before calling this primitive.
    /// In particular, Tachi unavailability is not permission to synthesize a local projection.
    /// This has no production activation in the storage-only leaf.
    pub fn insert_trusted_node_projection(&self, grant: &NodeGrantProjection) -> Result<()> {
        let identity_epoch = epoch(grant.identity_epoch).ok_or_else(|| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                "node_grant_identity_epoch_out_of_range"
            );
            anyhow::Error::msg("identity epoch exceeds canonical storage range")
        })?;
        ensure!(
            [
                grant.grant_id.as_str(),
                &grant.device_id,
                &grant.capability,
                &grant.nonce
            ]
            .iter()
            .all(|s| !s.is_empty()),
            "invalid Node grant projection"
        );
        ensure!(
            grant.args_hash.len() == 64
                && grant
                    .args_hash
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
            "invalid canonical argument hash"
        );
        let expires = match grant.expires_at {
            Some(value) => value,
            None => grant
                .granted_at
                .checked_add_signed(Duration::seconds(DEFAULT_GRANT_TTL_SECS))
                .ok_or_else(|| {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                        "node_grant_lifetime_overflow"
                    );
                    anyhow::Error::msg("invalid grant lifetime")
                })?,
        };
        ensure!(
            expires > grant.granted_at && expires - grant.granted_at <= Duration::minutes(15),
            "invalid Node grant lifetime"
        );
        self.lock().execute(
            "INSERT INTO approval_grants
            (approval_id, grant_kind, args_hash, granted_at, expires_at, device_id,
             identity_epoch, capability, nonce, revoked_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                grant.grant_id,
                grant.kind.as_str(),
                grant.args_hash,
                grant.granted_at.to_rfc3339(),
                expires.to_rfc3339(),
                grant.device_id,
                identity_epoch,
                grant.capability,
                grant.nonce,
                grant.revoked_at.map(|v| v.to_rfc3339())
            ],
        )?;
        Ok(())
    }

    /// Atomically commit the first claim tuple, then return evidence. Never accepts a caller clock.
    /// Store failure returns no claim evidence; there is no automatic unclaim or transfer.
    pub fn claim_node_grant_by_id(
        &self,
        claim: &NodeGrantClaim<'_>,
    ) -> Result<Result<ClaimedNodeGrant, NodeClaimFailure>> {
        let invalid = || Ok(Err(NodeClaimFailure::NotClaimable));
        let Some(identity_epoch) = epoch(claim.identity.identity_epoch) else {
            return invalid();
        };
        if [
            claim.grant_id,
            &claim.identity.device_id,
            claim.capability,
            claim.nonce,
            claim.connection_id,
            claim.call_id,
        ]
        .iter()
        .any(|s| s.is_empty())
            || claim.identity.is_revoked()
            || !matches!(claim.identity.role, DeviceRole::Node | DeviceRole::Both)
        {
            return invalid();
        }
        let hash = args_hash(claim.args);
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = Utc::now();
        let lookup = |conn: &rusqlite::Connection| -> rusqlite::Result<Option<StoredClaim>> {
            conn.query_row(
                "SELECT granted_at, expires_at, revoked_at, consumed_at,
                     claim_connection_id, claim_cap_revision, claim_call_id FROM approval_grants
                 WHERE approval_id = ?1 AND grant_kind = ?2 AND device_id = ?3
                   AND identity_epoch = ?4 AND capability = ?5 AND args_hash = ?6 AND nonce = ?7",
                params![
                    claim.grant_id,
                    claim.kind.as_str(),
                    claim.identity.device_id,
                    identity_epoch,
                    claim.capability,
                    hash,
                    claim.nonce
                ],
                |r| {
                    Ok(StoredClaim {
                        granted: r.get(0)?,
                        expires: r.get(1)?,
                        revoked: r.get(2)?,
                        consumed: r.get(3)?,
                        connection: r.get(4)?,
                        revision: r.get(5)?,
                        call: r.get(6)?,
                    })
                },
            )
            .optional()
        };
        let Some(stored) = lookup(&tx)? else {
            return invalid();
        };
        let (granted_at, expires_at) = match stored.state(now) {
            Ok(times) => times,
            Err(reason) => return Ok(Err(reason)),
        };
        let changed = tx.execute(
            "UPDATE approval_grants SET consumed_at = ?8,
              claim_connection_id = ?9, claim_cap_revision = ?10, claim_call_id = ?11
            WHERE approval_id = ?1 AND grant_kind = ?2 AND device_id = ?3 AND identity_epoch = ?4
              AND capability = ?5 AND args_hash = ?6 AND nonce = ?7
              AND granted_at = ?12 AND expires_at = ?13 AND revoked_at IS NULL
              AND consumed_at IS NULL AND claim_connection_id IS NULL
              AND claim_cap_revision IS NULL AND claim_call_id IS NULL",
            params![
                claim.grant_id,
                claim.kind.as_str(),
                claim.identity.device_id,
                identity_epoch,
                claim.capability,
                hash,
                claim.nonce,
                now.to_rfc3339(),
                claim.connection_id,
                claim.cap_revision.to_string(),
                claim.call_id,
                stored.granted,
                stored.expires
            ],
        )?;
        if changed != 1 {
            let reason = lookup(&tx)?
                .and_then(|r| r.state(now).err())
                .unwrap_or(NodeClaimFailure::NotClaimable);
            return Ok(Err(reason));
        }
        tx.commit()?;
        Ok(Ok(ClaimedNodeGrant {
            grant_id: claim.grant_id.into(),
            kind: claim.kind,
            device_id: claim.identity.device_id.clone(),
            identity_epoch: claim.identity.identity_epoch,
            capability: claim.capability.into(),
            args_hash: hash,
            nonce: claim.nonce.into(),
            granted_at,
            expires_at,
            claimed_at: now,
            connection_id: claim.connection_id.into(),
            cap_revision: claim.cap_revision,
            call_id: claim.call_id.into(),
        }))
    }

    /// Apply an already authorized revocation monotonically; never release a claim.
    pub fn record_node_revocation(
        &self,
        grant_id: &str,
        revoked_at: DateTime<Utc>,
    ) -> Result<bool> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE approval_grants SET revoked_at = ?2
            WHERE approval_id = ?1 AND grant_kind IN ('node_capability', 'tachi_projected')
              AND revoked_at IS NULL",
            params![grant_id, revoked_at.to_rfc3339()],
        )?;
        tx.commit()?;
        Ok(changed == 1)
    }
}

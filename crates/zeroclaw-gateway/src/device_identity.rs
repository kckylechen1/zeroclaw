//! Local device-identity projection: Ed25519 keys, pairing, ceiling, revoke.
//!
//! Pairing codes bootstrap enrollment only. Durable identity is the key
//! fingerprint recorded in `device_identities.db`.

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use zeroclaw_api::device_identity::{DeviceIdentityV1, DeviceKeyAlgorithm, DeviceRole};

const PAIRING_TTL: Duration = Duration::from_secs(300);
const CHALLENGE_TTL_SECS: i64 = 60;
const AUTH_CONTEXT: &str = "zeroclaw.node.auth.v1";

/// Node-side keypair. The private key never leaves this handle.
pub struct DeviceKeyPair {
    inner: Ed25519KeyPair,
    public_key_hex: String,
    fingerprint: String,
}

impl DeviceKeyPair {
    pub fn generate() -> Result<Self, IdentityError> {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
            .map_err(|_| IdentityError::Crypto("ed25519 keygen failed"))?;
        let inner = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
            .map_err(|_| IdentityError::Crypto("ed25519 pkcs8 parse failed"))?;
        Ok(Self::from_key_pair(inner))
    }

    fn from_key_pair(inner: Ed25519KeyPair) -> Self {
        let public_key_hex = hex::encode(inner.public_key().as_ref());
        let fingerprint = fingerprint_public_key(inner.public_key().as_ref());
        Self {
            inner,
            public_key_hex,
            fingerprint,
        }
    }

    #[must_use]
    pub fn public_key_hex(&self) -> &str {
        &self.public_key_hex
    }

    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    #[must_use]
    pub fn sign(&self, message: &[u8]) -> String {
        hex::encode(self.inner.sign(message).as_ref())
    }
}

/// Why enrollment or verification failed. Handshake maps every verify
/// failure to the same in-band `identity_rejected` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityError {
    Crypto(&'static str),
    InvalidPublicKey,
    InvalidPairingCode,
    FingerprintConflict,
    WidenRefused,
    IdentityRejected,
    PersistFailed,
    Capacity,
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Crypto(msg) => write!(f, "{msg}"),
            Self::InvalidPublicKey => write!(f, "invalid public key"),
            Self::InvalidPairingCode => write!(f, "invalid or expired pairing code"),
            Self::FingerprintConflict => write!(f, "public key is already bound"),
            Self::WidenRefused => write!(f, "live advertisement exceeds the approved ceiling"),
            Self::IdentityRejected => write!(f, "identity rejected"),
            Self::PersistFailed => write!(f, "identity persist failed"),
            Self::Capacity => write!(f, "identity table at capacity"),
        }
    }
}

#[must_use]
pub fn fingerprint_public_key(public_key: &[u8]) -> String {
    hex::encode(Sha256::digest(public_key))
}

#[must_use]
pub fn auth_message(nonce: &str, device_id: &str, key_fingerprint: &str, epoch: u64) -> Vec<u8> {
    format!("{AUTH_CONTEXT}\n{nonce}\n{device_id}\n{key_fingerprint}\n{epoch}").into_bytes()
}

pub fn verify_auth_signature(
    public_key_hex: &str,
    message: &[u8],
    signature_hex: &str,
) -> Result<(), IdentityError> {
    let public_key = hex::decode(public_key_hex).map_err(|_| IdentityError::IdentityRejected)?;
    let signature = hex::decode(signature_hex).map_err(|_| IdentityError::IdentityRejected)?;
    if public_key.len() != 32 {
        return Err(IdentityError::IdentityRejected);
    }
    UnparsedPublicKey::new(&ED25519, &public_key)
        .verify(message, &signature)
        .map_err(|_| IdentityError::IdentityRejected)
}

/// Live advertisement must be a subset of the approved ceiling.
pub fn admit_live_caps(
    advertised: &[String],
    ceiling: &[String],
) -> Result<Vec<String>, IdentityError> {
    let allowed: std::collections::HashSet<&str> = ceiling.iter().map(String::as_str).collect();
    let mut admitted = Vec::with_capacity(advertised.len());
    for cap in advertised {
        if !allowed.contains(cap.as_str()) {
            return Err(IdentityError::WidenRefused);
        }
        admitted.push(cap.clone());
    }
    Ok(admitted)
}

#[derive(Debug, Clone)]
struct PendingPairing {
    expires_at: Instant,
}

#[derive(Clone)]
pub struct DeviceIdentityStore {
    inner: Arc<StoreInner>,
}

struct StoreInner {
    rows: Mutex<HashMap<String, DeviceIdentityV1>>,
    pairing: Mutex<HashMap<String, PendingPairing>>,
    db_path: Option<PathBuf>,
}

impl DeviceIdentityStore {
    #[must_use]
    pub fn memory() -> Self {
        Self {
            inner: Arc::new(StoreInner {
                rows: Mutex::new(HashMap::new()),
                pairing: Mutex::new(HashMap::new()),
                db_path: None,
            }),
        }
    }

    pub fn open(data_dir: &Path) -> Result<Self, rusqlite::Error> {
        std::fs::create_dir_all(data_dir)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        let db_path = data_dir.join("device_identities.db");
        create_owner_only_file(&db_path)?;
        let conn = Connection::open(&db_path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             CREATE TABLE IF NOT EXISTS device_identities (
                device_id TEXT PRIMARY KEY,
                public_key TEXT NOT NULL,
                key_fingerprint TEXT NOT NULL UNIQUE,
                algorithm TEXT NOT NULL,
                role TEXT NOT NULL,
                identity_epoch INTEGER NOT NULL,
                admitted_at TEXT NOT NULL,
                revoked_at TEXT,
                capability_ceiling TEXT NOT NULL
             );",
        )?;
        harden_owner_only(&db_path);
        let store = Self {
            inner: Arc::new(StoreInner {
                rows: Mutex::new(HashMap::new()),
                pairing: Mutex::new(HashMap::new()),
                db_path: Some(db_path),
            }),
        };
        store.load_from_db(&conn)?;
        Ok(store)
    }

    fn load_from_db(&self, conn: &Connection) -> Result<(), rusqlite::Error> {
        let mut stmt = conn.prepare(
            "SELECT device_id, public_key, key_fingerprint, algorithm, role,
                    identity_epoch, admitted_at, revoked_at, capability_ceiling
             FROM device_identities",
        )?;
        let rows = stmt.query_map([], |row| {
            let ceiling_json: String = row.get(8)?;
            let capability_ceiling = serde_json::from_str(&ceiling_json).unwrap_or_default();
            Ok(DeviceIdentityV1 {
                device_id: row.get(0)?,
                public_key: row.get(1)?,
                key_fingerprint: row.get(2)?,
                algorithm: DeviceKeyAlgorithm::Ed25519,
                role: DeviceRole::Node,
                identity_epoch: row.get(5)?,
                admitted_at: row.get(6)?,
                revoked_at: row.get(7)?,
                capability_ceiling,
            })
        })?;
        let mut cache = self.inner.rows.lock();
        for row in rows.flatten() {
            cache.insert(row.device_id.clone(), row);
        }
        Ok(())
    }

    pub fn issue_pairing_code(&self) -> Result<String, IdentityError> {
        let mut bytes = [0u8; 8];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| IdentityError::Crypto("pairing code rng failed"))?;
        let code = hex::encode(bytes);
        self.inner.pairing.lock().insert(
            code.clone(),
            PendingPairing {
                expires_at: Instant::now() + PAIRING_TTL,
            },
        );
        Ok(code)
    }

    pub fn enroll(
        &self,
        code: &str,
        public_key_hex: &str,
        ceiling: Vec<String>,
    ) -> Result<DeviceIdentityV1, IdentityError> {
        self.consume_pairing_code(code)?;
        let public_key =
            hex::decode(public_key_hex).map_err(|_| IdentityError::InvalidPublicKey)?;
        if public_key.len() != 32 {
            return Err(IdentityError::InvalidPublicKey);
        }
        let fingerprint = fingerprint_public_key(&public_key);
        {
            let rows = self.inner.rows.lock();
            if rows
                .values()
                .any(|row| row.key_fingerprint == fingerprint && !row.is_revoked())
            {
                return Err(IdentityError::FingerprintConflict);
            }
        }
        let identity = DeviceIdentityV1 {
            device_id: uuid::Uuid::new_v4().to_string(),
            public_key: hex::encode(public_key),
            key_fingerprint: fingerprint,
            algorithm: DeviceKeyAlgorithm::Ed25519,
            role: DeviceRole::Node,
            identity_epoch: 1,
            admitted_at: Utc::now().to_rfc3339(),
            revoked_at: None,
            capability_ceiling: ceiling,
        };
        self.persist_insert(&identity)
            .map_err(|_| IdentityError::Crypto("identity persist failed"))?;
        self.inner
            .rows
            .lock()
            .insert(identity.device_id.clone(), identity.clone());
        Ok(identity)
    }

    fn consume_pairing_code(&self, code: &str) -> Result<(), IdentityError> {
        let mut pending = self.inner.pairing.lock();
        pending.retain(|_, item| item.expires_at > Instant::now());
        match pending.remove(code) {
            Some(item) if item.expires_at > Instant::now() => Ok(()),
            _ => Err(IdentityError::InvalidPairingCode),
        }
    }

    /// Whether a row exists for `device_id`, including revoked rows.
    #[must_use]
    pub fn contains(&self, device_id: &str) -> bool {
        self.inner.rows.lock().contains_key(device_id)
    }

    /// Active (non-revoked) identity matching both id and fingerprint.
    /// Unknown, revoked, and fingerprint mismatch are indistinguishable.
    pub fn active_identity(
        &self,
        device_id: &str,
        key_fingerprint: &str,
    ) -> Option<DeviceIdentityV1> {
        let rows = self.inner.rows.lock();
        let row = rows.get(device_id)?;
        if row.is_revoked() || row.key_fingerprint != key_fingerprint {
            return None;
        }
        Some(row.clone())
    }

    pub fn revoke(&self, device_id: &str) -> Result<bool, IdentityError> {
        let revoked_at = Utc::now().to_rfc3339();
        {
            let rows = self.inner.rows.lock();
            let Some(row) = rows.get(device_id) else {
                return Ok(false);
            };
            if row.revoked_at.is_some() {
                return Ok(true);
            }
        }
        if let Some(path) = &self.inner.db_path {
            persist_revoke(path, device_id, &revoked_at)
                .map_err(|_| IdentityError::PersistFailed)?;
        }
        let mut rows = self.inner.rows.lock();
        if let Some(row) = rows.get_mut(device_id) {
            row.revoked_at = Some(revoked_at);
        }
        Ok(true)
    }

    fn persist_insert(&self, identity: &DeviceIdentityV1) -> Result<(), rusqlite::Error> {
        let Some(path) = &self.inner.db_path else {
            return Ok(());
        };
        let conn = Connection::open(path)?;
        let ceiling =
            serde_json::to_string(&identity.capability_ceiling).unwrap_or_else(|_| "[]".into());
        conn.execute(
            "INSERT INTO device_identities (
                device_id, public_key, key_fingerprint, algorithm, role,
                identity_epoch, admitted_at, revoked_at, capability_ceiling
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                identity.device_id,
                identity.public_key,
                identity.key_fingerprint,
                identity.algorithm.as_str(),
                identity.role.as_str(),
                identity.identity_epoch,
                identity.admitted_at,
                identity.revoked_at,
                ceiling,
            ],
        )?;
        harden_owner_only(path);
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct PendingChallenge {
    pub nonce: String,
    pub expires_at: DateTime<Utc>,
    pub device_id: String,
    pub key_fingerprint: String,
}

impl PendingChallenge {
    pub fn issue(device_id: String, key_fingerprint: String) -> Result<Self, IdentityError> {
        let mut bytes = [0u8; 32];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| IdentityError::Crypto("challenge rng failed"))?;
        Ok(Self {
            nonce: hex::encode(bytes),
            expires_at: Utc::now() + chrono::Duration::seconds(CHALLENGE_TTL_SECS),
            device_id,
            key_fingerprint,
        })
    }

    #[must_use]
    pub fn expired(&self) -> bool {
        Utc::now() > self.expires_at
    }
}

fn create_owner_only_file(path: &Path) -> Result<(), rusqlite::Error> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    harden_owner_only(path);
    Ok(())
}

fn harden_owner_only(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

fn persist_revoke(path: &Path, device_id: &str, revoked_at: &str) -> Result<(), rusqlite::Error> {
    let conn = Connection::open(path)?;
    conn.execute(
        "UPDATE device_identities SET revoked_at = ?1 WHERE device_id = ?2",
        rusqlite::params![revoked_at, device_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_binds_fingerprint_and_ceiling() {
        let store = DeviceIdentityStore::memory();
        let keys = DeviceKeyPair::generate().unwrap();
        let code = store.issue_pairing_code().unwrap();
        let identity = store
            .enroll(&code, keys.public_key_hex(), vec!["system.notify".into()])
            .unwrap();
        assert_eq!(identity.key_fingerprint, keys.fingerprint());
        assert_eq!(identity.capability_ceiling, ["system.notify"]);
        assert!(
            store
                .active_identity(&identity.device_id, &identity.key_fingerprint)
                .is_some()
        );
        assert!(store.enroll(&code, keys.public_key_hex(), vec![]).is_err());
    }

    #[test]
    fn expired_or_unknown_pairing_code_fails() {
        let store = DeviceIdentityStore::memory();
        let keys = DeviceKeyPair::generate().unwrap();
        assert_eq!(
            store.enroll("missing", keys.public_key_hex(), vec![]),
            Err(IdentityError::InvalidPairingCode)
        );
    }

    #[test]
    fn signed_challenge_roundtrip_and_wrong_key_fail() {
        let store = DeviceIdentityStore::memory();
        let keys = DeviceKeyPair::generate().unwrap();
        let other = DeviceKeyPair::generate().unwrap();
        let code = store.issue_pairing_code().unwrap();
        let identity = store
            .enroll(&code, keys.public_key_hex(), vec!["system.notify".into()])
            .unwrap();
        let challenge =
            PendingChallenge::issue(identity.device_id.clone(), identity.key_fingerprint.clone())
                .unwrap();
        let message = auth_message(
            &challenge.nonce,
            &identity.device_id,
            &identity.key_fingerprint,
            identity.identity_epoch,
        );
        let good = keys.sign(&message);
        assert!(verify_auth_signature(&identity.public_key, &message, &good).is_ok());
        let bad = other.sign(&message);
        assert_eq!(
            verify_auth_signature(&identity.public_key, &message, &bad),
            Err(IdentityError::IdentityRejected)
        );
    }

    #[test]
    fn unknown_and_revoked_lookup_are_none() {
        let store = DeviceIdentityStore::memory();
        let keys = DeviceKeyPair::generate().unwrap();
        let code = store.issue_pairing_code().unwrap();
        let identity = store
            .enroll(&code, keys.public_key_hex(), vec!["system.notify".into()])
            .unwrap();
        assert!(
            store
                .active_identity("missing", &identity.key_fingerprint)
                .is_none()
        );
        assert_eq!(store.revoke(&identity.device_id), Ok(true));
        assert!(
            store
                .active_identity(&identity.device_id, &identity.key_fingerprint)
                .is_none()
        );
    }

    #[test]
    #[cfg(unix)]
    fn revoke_persist_failure_rolls_back_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let store = DeviceIdentityStore::open(dir.path()).unwrap();
        let keys = DeviceKeyPair::generate().unwrap();
        let code = store.issue_pairing_code().unwrap();
        let identity = store
            .enroll(&code, keys.public_key_hex(), vec!["system.notify".into()])
            .unwrap();
        let db = dir.path().join("device_identities.db");
        use std::os::unix::fs::PermissionsExt;
        for suffix in ["", "-wal", "-shm"] {
            let path = dir.path().join(format!("device_identities.db{suffix}"));
            if path.exists() {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
            }
        }
        assert_eq!(
            store.revoke(&identity.device_id),
            Err(IdentityError::PersistFailed)
        );
        assert!(
            store
                .active_identity(&identity.device_id, &identity.key_fingerprint)
                .is_some(),
            "memory must roll back when persist fails"
        );
        drop(store);
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600)).unwrap();
        for suffix in ["-wal", "-shm"] {
            let path = dir.path().join(format!("device_identities.db{suffix}"));
            if path.exists() {
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
        }
        let reopened = DeviceIdentityStore::open(dir.path()).unwrap();
        assert!(
            reopened
                .active_identity(&identity.device_id, &identity.key_fingerprint)
                .is_some(),
            "restart must keep the pre-revoke admitted row"
        );
    }

    #[test]
    fn ceiling_admits_subset_and_refuses_widen() {
        let ceiling = vec!["system.notify".into()];
        assert_eq!(
            admit_live_caps(&["system.notify".into()], &ceiling).unwrap(),
            ["system.notify"]
        );
        assert!(admit_live_caps(&[], &ceiling).unwrap().is_empty());
        assert_eq!(
            admit_live_caps(&["system.notify".into(), "camera.snap".into()], &ceiling),
            Err(IdentityError::WidenRefused)
        );
        assert_eq!(ceiling, ["system.notify"]);
    }

    #[test]
    #[cfg(unix)]
    fn identity_db_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = DeviceIdentityStore::open(dir.path()).unwrap();
        let keys = DeviceKeyPair::generate().unwrap();
        let code = store.issue_pairing_code().unwrap();
        store
            .enroll(&code, keys.public_key_hex(), vec!["system.notify".into()])
            .unwrap();
        let path = dir.path().join("device_identities.db");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "device identity db must be 0o600, got {mode:#o}"
        );
    }
}

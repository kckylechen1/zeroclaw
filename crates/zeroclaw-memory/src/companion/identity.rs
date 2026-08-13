//! Mint-once companion agent identity.
//!
//! The map lives at `{companion_store_dir}/agent-identity.json`, next to
//! `companion-memory.db`, not inside memcore. A JSON file is the host-owned
//! identity table: operators can edit it by hand, a mapping failure can refuse
//! a capture without writing a wrong UUID into `memories`, and a schema
//! migrate of the PortableKernel file cannot rewrite or drop the map.
//!
//! # Rename
//!
//! Changing `[agents.<alias>]` mints a **new** UUID the first time the new
//! alias is seen. To keep the old identity, copy the existing UUID onto the
//! new alias key before the new alias is captured. The runtime never rewrites
//! a key that is already present.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use zeroclaw_api::companion::AgentIdentityId;
use zeroclaw_config::companion::COMPANION_AGENT_IDENTITY_FILE;

static IDENTITY_FILE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AgentIdentityFile {
    #[serde(default = "identity_file_version")]
    version: u32,
    #[serde(default)]
    agents: BTreeMap<String, String>,
}

const fn identity_file_version() -> u32 {
    1
}

/// Resolve the mint-once UUID for `alias`, creating the mapping on first sight.
///
/// # Errors
/// Returns when `alias` is blank, the file is unreadable or not valid JSON,
/// a stored value is not a UUID, or a newly minted mapping cannot be written.
pub fn resolve_or_mint(store_dir: &Path, alias: &str) -> anyhow::Result<AgentIdentityId> {
    let alias = alias.trim();
    if alias.is_empty() {
        anyhow::bail!("companion agent alias is empty; refusing to mint an identity");
    }

    let _guard = IDENTITY_FILE_LOCK.lock();
    let path = identity_path(store_dir);
    let mut file = load_identity_file(&path)?;
    if let Some(existing) = file.agents.get(alias) {
        return parse_stored_uuid(alias, existing);
    }

    let minted = uuid::Uuid::new_v4();
    file.version = identity_file_version();
    file.agents
        .insert(alias.to_string(), minted.hyphenated().to_string());
    persist_identity_file(&path, &file)?;
    Ok(AgentIdentityId::from_opaque(
        minted.hyphenated().to_string(),
    ))
}

/// Read a stored mapping without minting.
#[must_use]
pub fn peek(store_dir: &Path, alias: &str) -> Option<AgentIdentityId> {
    let alias = alias.trim();
    if alias.is_empty() {
        return None;
    }
    let _guard = IDENTITY_FILE_LOCK.lock();
    let file = load_identity_file(&identity_path(store_dir)).ok()?;
    file.agents
        .get(alias)
        .and_then(|raw| parse_stored_uuid(alias, raw).ok())
}

fn identity_path(store_dir: &Path) -> PathBuf {
    store_dir.join(COMPANION_AGENT_IDENTITY_FILE)
}

fn load_identity_file(path: &Path) -> anyhow::Result<AgentIdentityFile> {
    if !path.exists() {
        return Ok(AgentIdentityFile::default());
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("read companion agent identity {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parse companion agent identity {}", path.display()))
}

fn persist_identity_file(path: &Path, file: &AgentIdentityFile) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create companion identity dir {}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(file).context("serialize companion agent identity")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes)
        .with_context(|| format!("write companion agent identity {}", tmp.display()))?;
    set_owner_only_file(&tmp)?;
    std::fs::rename(&tmp, path).with_context(|| {
        format!(
            "install companion agent identity {} -> {}",
            tmp.display(),
            path.display()
        )
    })?;
    set_owner_only_file(path)?;
    Ok(())
}

fn parse_stored_uuid(alias: &str, raw: &str) -> anyhow::Result<AgentIdentityId> {
    let parsed = uuid::Uuid::parse_str(raw.trim()).map_err(|_| {
        anyhow::Error::msg(format!(
            "companion agent identity for `{alias}` is not a UUID; refusing to use it"
        ))
    })?;
    Ok(AgentIdentityId::from_opaque(
        parsed.hyphenated().to_string(),
    ))
}

fn set_owner_only_file(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if path.exists() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn first_sight_mints_and_second_sight_reuses() {
        let tmp = TempDir::new().unwrap();
        let first = resolve_or_mint(tmp.path(), "alpha").expect("mint");
        let second = resolve_or_mint(tmp.path(), "alpha").expect("reuse");
        assert_eq!(first, second);
        assert!(uuid::Uuid::parse_str(first.as_str()).is_ok());
        assert_eq!(peek(tmp.path(), "alpha").as_ref(), Some(&first));
    }

    #[test]
    fn restart_rereads_the_same_uuid() {
        let tmp = TempDir::new().unwrap();
        let minted = resolve_or_mint(tmp.path(), "beta").expect("mint");
        drop(minted.clone());
        let after = resolve_or_mint(tmp.path(), "beta").expect("reread");
        assert_eq!(minted, after);
    }

    #[test]
    fn existing_entry_is_never_rewritten() {
        let tmp = TempDir::new().unwrap();
        let known = "550e8400-e29b-41d4-a716-446655440000";
        let path = identity_path(tmp.path());
        std::fs::write(
            &path,
            format!(r#"{{"version":1,"agents":{{"kept":"{known}"}}}}"#),
        )
        .unwrap();
        let resolved = resolve_or_mint(tmp.path(), "kept").expect("existing");
        assert_eq!(resolved.as_str(), known);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains(known));
        assert_eq!(raw.matches(known).count(), 1);
        let _ = resolve_or_mint(tmp.path(), "other").expect("new alias");
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains(known),
            "minting another alias must not rewrite kept"
        );
        assert_eq!(
            resolve_or_mint(tmp.path(), "kept").expect("still").as_str(),
            known
        );
    }

    #[test]
    fn rename_without_manual_copy_mints_a_new_identity() {
        let tmp = TempDir::new().unwrap();
        let old = resolve_or_mint(tmp.path(), "old-alias").expect("old");
        let new = resolve_or_mint(tmp.path(), "new-alias").expect("new");
        assert_ne!(old, new);
        assert_eq!(peek(tmp.path(), "old-alias").as_ref(), Some(&old));
    }

    #[test]
    fn blank_alias_is_refused() {
        let tmp = TempDir::new().unwrap();
        assert!(resolve_or_mint(tmp.path(), "   ").is_err());
        assert!(peek(tmp.path(), "   ").is_none());
    }

    #[test]
    fn corrupt_stored_value_is_refused() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            identity_path(tmp.path()),
            r#"{"version":1,"agents":{"bad":"not-a-uuid"}}"#,
        )
        .unwrap();
        let err = resolve_or_mint(tmp.path(), "bad").expect_err("refuse");
        assert!(err.to_string().contains("not a UUID"), "{err}");
    }

    #[test]
    fn mapping_write_failure_returns_err() {
        let tmp = TempDir::new().unwrap();
        let blocked = tmp.path().join(COMPANION_AGENT_IDENTITY_FILE);
        std::fs::create_dir(&blocked).unwrap();
        let err = resolve_or_mint(tmp.path(), "blocked").expect_err("dir in the way");
        assert!(peek(tmp.path(), "blocked").is_none());
        let _ = err;
    }
}

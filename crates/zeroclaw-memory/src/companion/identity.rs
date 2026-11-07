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
//! Supported agent renames bind both aliases to the same UUID before the
//! configuration is committed. Creating a new agent under a retired alias
//! clears that alias's old mapping so the new agent receives a fresh UUID.

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

/// Re-point `to` at the identity currently held by `from` (alias rename).
/// The `from` mapping is PRESERVED (both names resolve to one identity, so a
/// failed config save leaves the source identity intact). If `from` has
/// never been resolved, mint its identity here: a stale `to` mapping must
/// never supply the identity of an unrelated, renamed-away agent.
///
/// Callers must validate the config rename and reject an occupied target
/// before calling this function, then persist config only after it succeeds.
/// Returns whether the identity map changed.
pub fn rename_alias(store_dir: &Path, from: &str, to: &str) -> anyhow::Result<bool> {
    let (from, to) = (from.trim(), to.trim());
    anyhow::ensure!(
        !from.is_empty() && !to.is_empty(),
        "companion agent aliases are empty; refusing to rename an identity"
    );
    let _guard = IDENTITY_FILE_LOCK.lock();
    let path = identity_path(store_dir);
    let mut file = load_identity_file(&path)?;
    let identity = match file.agents.get(from) {
        Some(raw) => parse_stored_uuid(from, raw)?.as_str().to_string(),
        None => uuid::Uuid::new_v4().hyphenated().to_string(),
    };
    if file.agents.get(from) == Some(&identity) && file.agents.get(to) == Some(&identity) {
        return Ok(false);
    }
    file.version = identity_file_version();
    file.agents.insert(from.to_string(), identity.clone());
    file.agents.insert(to.to_string(), identity);
    persist_identity_file(&path, &file)?;
    Ok(true)
}

/// Drop any mapping for `alias` so the next resolution mints a FRESH
/// identity. Called at the config agent-CREATE boundary: a preserved rename
/// mapping (a→b kept `a` pointing at the old identity) must not silently
/// hand a genuinely new agent `a` the old identity — and with it the old
/// agent's identity-keyed state (e.g. the durable task list). Returns
/// whether a mapping was dropped.
pub fn invalidate_alias(store_dir: &Path, alias: &str) -> anyhow::Result<bool> {
    let _guard = IDENTITY_FILE_LOCK.lock();
    let path = identity_path(store_dir);
    let mut file = load_identity_file(&path)?;
    if file.agents.remove(alias).is_some() {
        persist_identity_file(&path, &file)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

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

/// Every alias whose recorded identity matches `identity` (both current and
/// rename-preserved source entries). Used by the reminder delivery gate so an
/// agent renamed via the config editor while a reminder is pending still
/// passes the live-agent check when the target matches its new alias.
pub fn aliases_for_identity(store_dir: &Path, identity: &str) -> Vec<String> {
    let identity = identity.trim();
    if identity.is_empty() {
        return Vec::new();
    }
    let _guard = IDENTITY_FILE_LOCK.lock();
    let Ok(file) = load_identity_file(&identity_path(store_dir)) else {
        return Vec::new();
    };
    file.agents
        .into_iter()
        .filter_map(|(alias, raw)| {
            parse_stored_uuid(&alias, &raw)
                .ok()
                .filter(|id| id.as_str() == identity)
                .map(|_| alias)
        })
        .collect()
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
    fn unrelated_alias_mints_a_new_identity() {
        let tmp = TempDir::new().unwrap();
        let old = resolve_or_mint(tmp.path(), "old-alias").expect("old");
        let new = resolve_or_mint(tmp.path(), "new-alias").expect("new");
        assert_ne!(old, new);
        assert_eq!(peek(tmp.path(), "old-alias").as_ref(), Some(&old));
    }

    #[test]
    fn rename_before_first_use_replaces_a_retired_targets_identity() {
        let tmp = TempDir::new().unwrap();
        let old = resolve_or_mint(tmp.path(), "a").unwrap();
        rename_alias(tmp.path(), "a", "b").unwrap();
        assert!(peek(tmp.path(), "c").is_none());

        assert!(rename_alias(tmp.path(), "c", "a").unwrap());
        let renamed = resolve_or_mint(tmp.path(), "a").unwrap();
        assert_ne!(renamed, old, "c must not inherit b's durable state");
        assert_eq!(peek(tmp.path(), "c"), Some(renamed.clone()));
        assert_eq!(peek(tmp.path(), "b"), Some(old));
        assert!(!rename_alias(tmp.path(), "c", "a").unwrap());
        assert_eq!(peek(tmp.path(), "a"), Some(renamed));
    }

    #[test]
    fn rename_keeps_source_identity_and_create_reuse_mints_fresh() {
        let tmp = TempDir::new().unwrap();
        let old = resolve_or_mint(tmp.path(), "a").unwrap();
        rename_alias(tmp.path(), "a", "b").unwrap();
        assert_eq!(peek(tmp.path(), "a"), Some(old.clone()));
        assert_eq!(peek(tmp.path(), "b"), Some(old.clone()));

        invalidate_alias(tmp.path(), "a").unwrap();
        assert_ne!(resolve_or_mint(tmp.path(), "a").unwrap(), old);
        assert_eq!(peek(tmp.path(), "b"), Some(old));
    }

    #[test]
    fn failed_rename_preserves_existing_identity_file() {
        let tmp = TempDir::new().unwrap();
        let old = resolve_or_mint(tmp.path(), "a").unwrap();
        let stale = resolve_or_mint(tmp.path(), "b").unwrap();
        let path = identity_path(tmp.path());
        let before = std::fs::read(&path).unwrap();
        std::fs::create_dir(path.with_extension("json.tmp")).unwrap();

        assert!(rename_alias(tmp.path(), "a", "b").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(peek(tmp.path(), "a"), Some(old));
        assert_eq!(peek(tmp.path(), "b"), Some(stale));
    }

    #[test]
    fn rename_refuses_a_corrupt_source_identity_without_overwriting_target() {
        let tmp = TempDir::new().unwrap();
        let target = resolve_or_mint(tmp.path(), "b").unwrap();
        let path = identity_path(tmp.path());
        let mut file = load_identity_file(&path).unwrap();
        file.agents
            .insert("a".to_string(), "not-a-uuid".to_string());
        persist_identity_file(&path, &file).unwrap();
        let before = std::fs::read(&path).unwrap();

        assert!(rename_alias(tmp.path(), "a", "b").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(peek(tmp.path(), "b"), Some(target));
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

    #[test]
    fn reverse_lookup_returns_every_holder_including_preserved_renames() {
        let tmp = TempDir::new().unwrap();
        let id = resolve_or_mint(tmp.path(), "a").unwrap();
        let _ = resolve_or_mint(tmp.path(), "other").unwrap();
        rename_alias(tmp.path(), "a", "b").unwrap();

        let holders = aliases_for_identity(tmp.path(), id.as_str());
        assert_eq!(holders, vec!["a".to_string(), "b".to_string()]);
        assert!(
            aliases_for_identity(tmp.path(), "00000000-0000-0000-0000-000000000000").is_empty()
        );
    }
}

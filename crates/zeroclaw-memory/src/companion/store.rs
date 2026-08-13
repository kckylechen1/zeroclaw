//! PortableKernel companion store: open / close / reload lifecycle.

use std::path::{Path, PathBuf};

use anyhow::Context;
use memcore::{DbOpenContext, MemoryError, MemoryStore, StoreProfile};
use parking_lot::Mutex;

const MIGRATE_CLI_HINT: &str = "zeroclaw companion migrate";

/// In-process PortableKernel store for companion memory.
///
/// Drop closes the rusqlite connection. Production holds this behind
/// `Arc` on gateway `AppState` and the channel orchestrator context.
pub struct CompanionStore {
    path: PathBuf,
    store: Mutex<MemoryStore>,
}

impl CompanionStore {
    /// Runtime open: missing path → `create_fresh`; existing path →
    /// `open_existing_deny`. Never migrates. Never calls `create_fresh` on a
    /// path that already exists.
    ///
    /// Crate-private: the only production caller is [`crate::create_companion_store`].
    ///
    /// # Errors
    /// Returns when the directory cannot be created, memcore refuses the open
    /// (including schema mismatch), or owner-only permissions cannot be set.
    pub(crate) fn open_runtime(path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            Self::open_existing_deny(path)
        } else {
            Self::create_fresh(path)
        }
    }

    /// CLI seam for schema upgrades. Runtime and the daemon must not call this.
    ///
    /// The only legitimate caller is the future `zeroclaw companion migrate`
    /// command in the binary crate. That CLI passes a typed
    /// `Allow { approved_by }` context. The CLI body itself is a later slice.
    ///
    /// # Errors
    /// Returns when the path is missing or memcore refuses the authorized open.
    pub fn open_for_schema_migrate(path: &Path, approved_by: &str) -> anyhow::Result<Self> {
        if !path.exists() {
            anyhow::bail!(
                "companion memory at {} does not exist; nothing to migrate. \
                 Enable companion memory once so a store is created, or pass the \
                 existing companion-memory.db path.",
                path.display()
            );
        }
        let ctx = DbOpenContext::open_existing_allow(approved_by)
            .with_profile(StoreProfile::PortableKernel);
        Self::open_with_context(path, &ctx)
    }

    /// Filesystem path of this store.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stamped memcore profile. Runtime always requests PortableKernel.
    #[must_use]
    pub fn store_profile(&self) -> StoreProfile {
        self.store.lock().store_profile()
    }

    #[cfg(test)]
    pub(crate) fn store_handle(&self) -> &Mutex<MemoryStore> {
        &self.store
    }

    fn create_fresh(path: &Path) -> anyhow::Result<Self> {
        let ctx = DbOpenContext::create_fresh().with_profile(StoreProfile::PortableKernel);
        Self::open_with_context(path, &ctx)
    }

    fn open_existing_deny(path: &Path) -> anyhow::Result<Self> {
        let ctx = DbOpenContext::open_existing_deny().with_profile(StoreProfile::PortableKernel);
        Self::open_with_context(path, &ctx)
    }

    fn open_with_context(path: &Path, ctx: &DbOpenContext) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            ensure_owner_only_dir(parent)
                .with_context(|| format!("create companion store dir {}", parent.display()))?;
        }
        let path_str = path.to_str().with_context(|| {
            format!(
                "companion memory db path is not valid UTF-8: {}",
                path.display()
            )
        })?;
        let store = MemoryStore::open_with_context(path_str, ctx)
            .map_err(|err| map_open_error(err, path))?;
        ensure_owner_only_file(path)
            .with_context(|| format!("set companion db permissions {}", path.display()))?;
        harden_existing_sqlite_sidecars(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            store: Mutex::new(store),
        })
    }
}

fn map_open_error(err: MemoryError, path: &Path) -> anyhow::Error {
    let msg = match err {
        MemoryError::SchemaMigrationOptInRequired {
            stored, expected, ..
        } => format!(
            "companion memory at {} is schema {stored} (this build expects {expected}); \
             runtime opens never migrate. Run `{MIGRATE_CLI_HINT}` to upgrade, then restart.",
            path.display()
        ),
        MemoryError::DbCreateTargetExists { stored, .. } => format!(
            "companion memory at {} is already stamped at schema {stored}; \
             runtime must not call create_fresh on an existing store. Reopen with \
             open_existing, or run `{MIGRATE_CLI_HINT}` if the schema is older.",
            path.display()
        ),
        other => format!(
            "failed to open companion memory at {}: {other}",
            path.display()
        ),
    };
    anyhow::Error::msg(msg)
}

#[cfg(unix)]
fn ensure_owner_only_dir(path: &Path) -> anyhow::Result<()> {
    use std::io::ErrorKind;
    use std::os::unix::fs::PermissionsExt;

    match std::fs::create_dir(path) {
        Ok(()) => {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
            Ok(())
        }
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {
            warn_if_existing_dir_not_owner_only(path);
            Ok(())
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            std::fs::create_dir_all(path)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

#[cfg(unix)]
fn warn_if_existing_dir_not_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let actual = meta.permissions().mode() & 0o777;
    if actual == 0o700 {
        return;
    }
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
            .with_attrs(::serde_json::json!({
                "path": path.display().to_string(),
                "expected_mode": "0700",
                "actual_mode": format!("{actual:04o}"),
            })),
        &format!(
            "companion store dir {} has mode {actual:04o}; expected 0700. \
             Leaving permissions unchanged so a shared directory is not silently tightened.",
            path.display()
        )
    );
}

#[cfg(not(unix))]
fn ensure_owner_only_dir(path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        std::fs::create_dir_all(path)?;
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_owner_only_file(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if path.exists() {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_owner_only_file(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

fn sqlite_sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut path = db_path.as_os_str().to_os_string();
    path.push(suffix);
    path.into()
}

fn harden_existing_sqlite_sidecars(db_path: &Path) -> anyhow::Result<()> {
    for suffix in ["-wal", "-shm"] {
        let sidecar = sqlite_sidecar_path(db_path, suffix);
        if sidecar.exists() {
            ensure_owner_only_file(&sidecar)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::companion::{create_companion_store, reload_companion_store};
    use memcore::MemoryEntry;
    use tempfile::TempDir;
    use zeroclaw_config::schema::Config;

    fn enabled_config(data_dir: &Path) -> Config {
        let mut config = Config {
            data_dir: data_dir.to_path_buf(),
            ..Config::default()
        };
        config.companion_memory.enable = true;
        config
    }

    fn probe_entry(id: &str, text: &str) -> MemoryEntry {
        let now = chrono::Local::now().to_rfc3339();
        MemoryEntry {
            id: id.into(),
            path: "/companion/probe".into(),
            summary: text.chars().take(80).collect(),
            text: text.into(),
            importance: 0.5,
            timestamp: now.clone(),
            valid_from: now,
            valid_until: None,
            category: "fact".into(),
            topic: String::new(),
            keywords: Vec::new(),
            persons: Vec::new(),
            entities: Vec::new(),
            location: String::new(),
            source: "zeroclaw-test".into(),
            scope: "general".into(),
            archived: false,
            access_count: 0,
            scored_count: 0,
            last_use_at: None,
            last_access: None,
            revision: 1,
            vector: None,
            retention_policy: None,
            domain: None,
            metadata: serde_json::json!({}),
            recall_count: 0,
            query_diversity: 0,
            tier: "raw".into(),
        }
    }

    fn unix_mode(path: &Path) -> u32 {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            0
        }
    }

    #[test]
    fn first_open_create_fresh_succeeds_with_owner_only_permissions() {
        let tmp = TempDir::new().unwrap();
        let config = enabled_config(tmp.path());
        let store = create_companion_store(&config)
            .expect("open")
            .expect("enabled store");
        let path = store.path().to_path_buf();
        assert!(path.exists());
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("companion-memory.db")
        );
        assert_eq!(store.store_profile(), StoreProfile::PortableKernel);
        #[cfg(unix)]
        {
            assert_eq!(unix_mode(path.parent().unwrap()), 0o700);
            assert_eq!(unix_mode(&path), 0o600);
        }
        let _ = unix_mode(&path);
    }

    #[test]
    fn restart_open_existing_preserves_data() {
        let tmp = TempDir::new().unwrap();
        let config = enabled_config(tmp.path());
        let path;
        {
            let store = create_companion_store(&config)
                .expect("first open")
                .expect("enabled");
            path = store.path().to_path_buf();
            store
                .store_handle()
                .lock()
                .upsert(&probe_entry("probe-1", "companion survives restart"))
                .expect("upsert");
        }
        let reopened = CompanionStore::open_runtime(&path).expect("second open");
        let got = reopened
            .store_handle()
            .lock()
            .get("probe-1")
            .expect("get")
            .expect("row");
        assert_eq!(got.text, "companion survives restart");
    }

    #[test]
    fn stamped_db_refuses_create_fresh_and_runtime_reopen_uses_existing() {
        let tmp = TempDir::new().unwrap();
        let config = enabled_config(tmp.path());
        let path;
        {
            let store = create_companion_store(&config)
                .expect("first open")
                .expect("enabled");
            path = store.path().to_path_buf();
            store
                .store_handle()
                .lock()
                .upsert(&probe_entry("probe-stamp", "must not be wiped"))
                .expect("upsert");
        }
        let path_str = path.to_str().unwrap();
        let err = match MemoryStore::open_with_context(
            path_str,
            &DbOpenContext::create_fresh().with_profile(StoreProfile::PortableKernel),
        ) {
            Err(err) => err,
            Ok(_) => panic!("create_fresh on a stamped db must fail"),
        };
        assert!(
            matches!(err, MemoryError::DbCreateTargetExists { .. }),
            "{err}"
        );

        let reopened = CompanionStore::open_runtime(&path).expect("runtime reopen");
        let got = reopened
            .store_handle()
            .lock()
            .get("probe-stamp")
            .expect("get")
            .expect("row");
        assert_eq!(got.text, "must not be wiped");
    }

    #[test]
    fn schema_mismatch_is_denied_with_migrate_guidance() {
        let tmp = TempDir::new().unwrap();
        let config = enabled_config(tmp.path());
        let path;
        {
            let store = create_companion_store(&config)
                .expect("first open")
                .expect("enabled");
            path = store.path().to_path_buf();
        }
        {
            let conn = rusqlite::Connection::open(&path).expect("sqlite");
            conn.pragma_update(
                None,
                "user_version",
                memcore::db::migrations::EXPECTED_SCHEMA_VERSION - 1,
            )
            .expect("backdate schema stamp");
        }
        let err = CompanionStore::open_runtime(&path)
            .err()
            .expect("deny older schema");
        let msg = err.to_string();
        assert!(msg.contains(MIGRATE_CLI_HINT), "{msg}");
        assert!(msg.contains("never migrate"), "{msg}");
    }

    #[test]
    fn schema_migrate_seam_allows_explicit_upgrade() {
        let tmp = TempDir::new().unwrap();
        let config = enabled_config(tmp.path());
        let path;
        {
            let store = create_companion_store(&config)
                .expect("first open")
                .expect("enabled");
            path = store.path().to_path_buf();
        }
        {
            let conn = rusqlite::Connection::open(&path).expect("sqlite");
            conn.pragma_update(
                None,
                "user_version",
                memcore::db::migrations::EXPECTED_SCHEMA_VERSION - 1,
            )
            .expect("backdate schema stamp");
        }
        let migrated =
            CompanionStore::open_for_schema_migrate(&path, "cli:zeroclaw companion migrate")
                .expect("CLI Allow seam");
        drop(migrated);
        CompanionStore::open_runtime(&path).expect("runtime opens after explicit migrate");
    }

    #[test]
    fn reload_drops_old_arc_and_open_existing() {
        let tmp = TempDir::new().unwrap();
        let config = enabled_config(tmp.path());
        let first = create_companion_store(&config)
            .expect("first")
            .expect("enabled");
        first
            .store_handle()
            .lock()
            .upsert(&probe_entry("probe-reload", "reload keeps rows"))
            .expect("upsert");
        let reloaded = reload_companion_store(Some(first), &config)
            .expect("reload")
            .expect("still enabled");
        let got = reloaded
            .store_handle()
            .lock()
            .get("probe-reload")
            .expect("get")
            .expect("row");
        assert_eq!(got.text, "reload keeps rows");
    }

    #[test]
    fn tachi_backend_and_companion_use_distinct_files() {
        use crate::traits::{Memory, MemoryCategory};

        let tmp = TempDir::new().unwrap();
        let mut config = enabled_config(tmp.path());
        config.memory.backend = "tachi".into();

        let companion = create_companion_store(&config)
            .expect("companion")
            .expect("enabled");
        companion
            .store_handle()
            .lock()
            .upsert(&probe_entry("companion-only", "only in companion"))
            .expect("companion upsert");

        let tachi = crate::tachi::TachiMemory::new("tachi", tmp.path()).expect("tachi open");
        let companion_path = companion.path().to_path_buf();
        let tachi_path = tmp.path().join("memory").join("tachi.db");
        assert_ne!(companion_path, tachi_path);
        assert!(companion_path.exists());
        assert!(tachi_path.exists());
        assert!(
            !companion_path.starts_with(tmp.path().join("memory")),
            "companion must not live under memory/"
        );

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            tachi
                .store("tachi-only", "only in tachi", MemoryCategory::Core, None)
                .await
                .expect("tachi store");
        });
        drop(tachi);
        drop(companion);

        fn text_hits(db: &Path, needle: &str) -> i64 {
            let conn = rusqlite::Connection::open(db).expect("sqlite");
            conn.query_row(
                "SELECT COUNT(*) FROM memories WHERE text LIKE ?1",
                [format!("%{needle}%")],
                |row| row.get(0),
            )
            .expect("count")
        }

        assert_eq!(text_hits(&companion_path, "only in companion"), 1);
        assert_eq!(text_hits(&companion_path, "only in tachi"), 0);
        assert_eq!(text_hits(&tachi_path, "only in tachi"), 1);
        assert_eq!(text_hits(&tachi_path, "only in companion"), 0);
    }

    #[cfg(unix)]
    #[test]
    fn existing_wide_permission_dir_is_left_unchanged_and_warned() {
        use std::os::unix::fs::PermissionsExt;

        let _writer_guard = zeroclaw_log::__private_test_writer_lock();
        let _hook_guard = zeroclaw_log::__private_test_hook_lock();
        zeroclaw_log::try_install_capture_subscriber();
        let mut rx = zeroclaw_log::subscribe_or_install();
        while rx.try_recv().is_ok() {}

        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("shared-tmp-like");
        std::fs::create_dir(&store_dir).unwrap();
        std::fs::set_permissions(&store_dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(unix_mode(&store_dir), 0o777);

        let mut config = enabled_config(tmp.path());
        config.companion_memory.store_dir = Some(store_dir.clone());
        let store = create_companion_store(&config)
            .expect("open into pre-existing dir")
            .expect("enabled");
        drop(store);

        assert_eq!(
            unix_mode(&store_dir),
            0o777,
            "pre-existing store_dir must not be chmod'd"
        );

        let mut found = false;
        loop {
            match rx.try_recv() {
                Ok(value) => {
                    if value.get("severity_text").and_then(|v| v.as_str()) != Some("WARN") {
                        continue;
                    }
                    let attrs = value.get("attributes").cloned().unwrap_or_default();
                    let message = value.get("message").and_then(|v| v.as_str()).unwrap_or("");
                    let expected =
                        attrs.get("expected_mode").and_then(|v| v.as_str()) == Some("0700");
                    let actual = attrs.get("actual_mode").and_then(|v| v.as_str()) == Some("0777");
                    let message_has_modes =
                        message.contains("expected 0700") && message.contains("0777");
                    if (expected && actual) || message_has_modes {
                        found = true;
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(
                    tokio::sync::broadcast::error::TryRecvError::Empty
                    | tokio::sync::broadcast::error::TryRecvError::Closed,
                ) => break,
            }
        }
        zeroclaw_log::clear_broadcast_hook();
        assert!(
            found,
            "pre-existing wide store_dir must WARN with expected 0700 vs actual 0777"
        );
    }
}

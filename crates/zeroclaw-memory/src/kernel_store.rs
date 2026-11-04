//! Host-owned identities for ZeroClaw's optional shared memory kernel.
//!
//! Profile and role are admission requirements, never a request to reshape or
//! adopt an existing store. Legacy stores need the explicit offline migration.

use portable_kernel::{DbOpenContext, StoreProfile};

// Catch accidental feature unification with the full product at compile time.
const _: () = assert!(portable_kernel::IS_PORTABLE_BUILD);
const _: () = assert!(!portable_kernel::ADMIN_SURFACE_ENABLED);

pub(crate) const MIGRATE_CLI_HINT: &str = "zeroclaw memory migrate-kernel";

/// Which ZeroClaw owner may admit a shared-kernel database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelStoreKind {
    Memory,
    Companion,
}

impl KernelStoreKind {
    /// Stable role persisted in the database's write-once identity.
    #[must_use]
    pub const fn role(self) -> &'static str {
        match self {
            Self::Memory => "zeroclaw-memory",
            Self::Companion => "zeroclaw-companion",
        }
    }

    /// Preserve the caller's create/migration intent and require this owner.
    #[must_use]
    pub fn context(self, context: DbOpenContext) -> DbOpenContext {
        context
            .with_exact_profile(StoreProfile::PortableKernel)
            .with_exact_role(self.role())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::companion::CompanionStore;
    use crate::tachi::TachiMemory;
    use portable_kernel::MemoryStore;
    use rusqlite::{Connection, OpenFlags};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    fn store_path(workspace: &Path, kind: KernelStoreKind) -> PathBuf {
        match kind {
            KernelStoreKind::Memory => workspace.join("memory/tachi.db"),
            KernelStoreKind::Companion => workspace.join("companion/companion-memory.db"),
        }
    }

    fn runtime_open(workspace: &Path, kind: KernelStoreKind) -> anyhow::Result<()> {
        match kind {
            KernelStoreKind::Memory => TachiMemory::new("tachi", workspace).map(drop),
            KernelStoreKind::Companion => {
                CompanionStore::open_runtime(&store_path(workspace, kind)).map(drop)
            }
        }
    }

    fn files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                assert!(entry.file_type().unwrap().is_file());
                (
                    PathBuf::from(entry.file_name()),
                    std::fs::read(entry.path()).unwrap(),
                )
            })
            .collect()
    }

    fn seed_store(path: &Path, context: DbOpenContext) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let store = MemoryStore::open_with_context(path.to_str().unwrap(), &context).unwrap();
        store
            .set_state("zeroclaw_contract", "probe", r#"{"value":"preserved"}"#)
            .unwrap();
    }

    fn assert_runtime_refuses_unchanged(workspace: &Path, kind: KernelStoreKind) {
        let path = store_path(workspace, kind);
        let before = files(path.parent().unwrap());
        let error = runtime_open(workspace, kind).expect_err("runtime must refuse");
        assert!(error.to_string().contains(MIGRATE_CLI_HINT), "{error:#}");
        assert_eq!(
            files(path.parent().unwrap()),
            before,
            "runtime changed the source"
        );
        if kind == KernelStoreKind::Companion {
            assert!(CompanionStore::open_for_status_probe(&path).is_err());
            assert_eq!(
                files(path.parent().unwrap()),
                before,
                "status changed the source"
            );
        }
    }

    #[test]
    fn runtime_creates_distinct_owned_portable_stores() {
        let tmp = TempDir::new().unwrap();
        let generic = TachiMemory::new("tachi", tmp.path()).unwrap();
        assert_eq!(
            generic.store_handle().lock().store_profile(),
            StoreProfile::PortableKernel
        );
        assert_eq!(
            generic.store_handle().lock().db_label(),
            KernelStoreKind::Memory.role()
        );
        let companion_path = store_path(tmp.path(), KernelStoreKind::Companion);
        let companion = CompanionStore::open_runtime(&companion_path).unwrap();
        assert_eq!(companion.store_profile(), StoreProfile::PortableKernel);
        assert_eq!(
            companion.with_store(|store| store.db_label().to_owned()),
            KernelStoreKind::Companion.role()
        );
        drop(generic);
        drop(companion);

        for kind in [KernelStoreKind::Memory, KernelStoreKind::Companion] {
            let path = store_path(tmp.path(), kind);
            assert_eq!(
                portable_kernel::db::store_profile_stamp(&path).unwrap(),
                Some(StoreProfile::PortableKernel)
            );
            let conn =
                Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
            assert_eq!(
                portable_kernel::db::store_identity::store_role_stamp(&conn)
                    .unwrap()
                    .as_deref(),
                Some(kind.role())
            );
            for table in [
                "audit_log",
                "agent_known_state",
                "llm_usage",
                "sandbox_rules",
                "sandbox_policies",
                "sandbox_exec_audit",
                "hub_capabilities",
                "vault_entries",
                "vault_key_health",
                "provider_accounts",
                "model_deployments",
                "foundry_jobs",
                "exec_envs",
                "dispatch_outcomes",
                "mirror_eval_runs",
                "session_claims",
                "agent_identities",
                "identity_admissions",
                "a2a_envelopes",
            ] {
                let exists: bool = conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
                        [table],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert!(!exists, "{kind:?} created product table {table}");
            }
            drop(conn);
            runtime_open(tmp.path(), kind).expect("matching role survives restart");
        }
    }

    #[test]
    fn runtime_and_explicit_upgrade_refuse_wrong_profile_role_and_unowned_stores() {
        for kind in [KernelStoreKind::Memory, KernelStoreKind::Companion] {
            let other = match kind {
                KernelStoreKind::Memory => KernelStoreKind::Companion,
                KernelStoreKind::Companion => KernelStoreKind::Memory,
            };
            for context in [
                DbOpenContext::create_fresh()
                    .with_exact_profile(StoreProfile::TachiFull)
                    .with_exact_role(kind.role()),
                other.context(DbOpenContext::create_fresh()),
                DbOpenContext::create_fresh().with_exact_profile(StoreProfile::PortableKernel),
            ] {
                let tmp = TempDir::new().unwrap();
                let path = store_path(tmp.path(), kind);
                seed_store(&path, context);
                assert_runtime_refuses_unchanged(tmp.path(), kind);
                if kind == KernelStoreKind::Companion {
                    let before = files(path.parent().unwrap());
                    assert!(
                        CompanionStore::open_for_schema_migrate(&path, "test:explicit-upgrade")
                            .is_err()
                    );
                    assert_eq!(
                        files(path.parent().unwrap()),
                        before,
                        "Allow must not confer an owner"
                    );
                }
            }
        }
    }

    #[test]
    fn runtime_refuses_owned_older_schema_without_upgrade() {
        for kind in [KernelStoreKind::Memory, KernelStoreKind::Companion] {
            let tmp = TempDir::new().unwrap();
            let path = store_path(tmp.path(), kind);
            seed_store(&path, kind.context(DbOpenContext::create_fresh()));
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(
                None,
                "user_version",
                portable_kernel::db::PORTABLE_OUTPUT_STAMP - 1,
            )
            .unwrap();
            drop(conn);
            assert_runtime_refuses_unchanged(tmp.path(), kind);
        }
    }

    #[test]
    fn runtime_refuses_unversioned_and_legacy_files_without_identity_adoption() {
        for kind in [KernelStoreKind::Memory, KernelStoreKind::Companion] {
            for version in [0, 38] {
                let tmp = TempDir::new().unwrap();
                let path = store_path(tmp.path(), kind);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                let conn = Connection::open(&path).unwrap();
                conn.execute_batch("CREATE TABLE legacy_probe(value TEXT); INSERT INTO legacy_probe VALUES ('preserved');").unwrap();
                conn.pragma_update(None, "user_version", version).unwrap();
                drop(conn);
                assert_runtime_refuses_unchanged(tmp.path(), kind);
            }
        }
    }

    #[test]
    fn companion_upgrade_requires_nonempty_approval_before_open() {
        let tmp = TempDir::new().unwrap();
        let path = store_path(tmp.path(), KernelStoreKind::Companion);
        let error = CompanionStore::open_for_schema_migrate(&path, " \n\t")
            .err()
            .unwrap();
        assert!(error.to_string().contains("approved_by"));
        assert!(!path.parent().unwrap().exists());
    }
}

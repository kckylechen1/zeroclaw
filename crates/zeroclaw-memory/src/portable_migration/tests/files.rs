use super::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

fn assert_stage_removed(parent: &Path) {
    assert!(std::fs::read_dir(parent).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".zeroclaw-kernel-migration-")
    }));
}

#[test]
fn committed_wal_is_read_without_touching_original_db_wal_or_shm() {
    let root = tempfile::TempDir::new().unwrap();
    let source_path = root.path().join("legacy.db");
    let destination = root.path().join("new.db");
    restore_fixture(&source_path, KernelStoreKind::Memory);
    let writer = snapshot::open_auxiliary_target(&source_path).unwrap();
    writer
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .unwrap();
    writer.execute("UPDATE hard_state SET value_json='{\"committed_wal\":true}',version=version+1 WHERE namespace='zeroclaw-fixture' AND key='ordinary'",[]).unwrap();
    assert!(
        source::suffixed(&source_path, "-wal")
            .metadata()
            .unwrap()
            .len()
            > 0
    );
    assert!(source::suffixed(&source_path, "-shm").exists());
    let before = source::census(&source_path).unwrap();
    let receipt = migrate_kernel_store(
        &source_path,
        &destination,
        KernelStoreKind::Memory,
        "fixture-operator",
    )
    .unwrap();
    assert_eq!(source::census(&source_path).unwrap(), before);
    assert_eq!(receipt.verification.source_files, before);
    let store = open_store(
        &destination,
        KernelStoreKind::Memory,
        DbOpenContext::open_existing_deny(),
    )
    .unwrap();
    assert_eq!(
        store.get_state_kv("zeroclaw-fixture", "ordinary").unwrap(),
        Some(("{\"committed_wal\":true}".into(), 8))
    );
    drop(store);
    assert_stage_removed(root.path());
    close_connection(writer).unwrap();
}

#[test]
fn uncommitted_wal_frames_are_excluded_and_original_files_remain_unchanged() {
    let root = tempfile::TempDir::new().unwrap();
    let source_path = root.path().join("legacy.db");
    let destination = root.path().join("new.db");
    restore_fixture(&source_path, KernelStoreKind::Memory);
    let writer = snapshot::open_auxiliary_target(&source_path).unwrap();
    writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; PRAGMA cache_size=1; PRAGMA cache_spill=ON;").unwrap();
    writer.execute("UPDATE hard_state SET value_json='{\"committed\":true}' WHERE namespace='zeroclaw-fixture' AND key='ordinary'",[]).unwrap();
    let committed_wal_size = source::suffixed(&source_path, "-wal")
        .metadata()
        .unwrap()
        .len();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let uncommitted = json!({"uncommitted":"x".repeat(256*1024)}).to_string();
    writer.execute("UPDATE hard_state SET value_json=?1 WHERE namespace='zeroclaw-fixture' AND key='ordinary'",[uncommitted]).unwrap();
    assert!(
        source::suffixed(&source_path, "-wal")
            .metadata()
            .unwrap()
            .len()
            > committed_wal_size,
        "fixture must spill real uncommitted WAL frames"
    );
    let before = source::census(&source_path).unwrap();
    migrate_kernel_store(
        &source_path,
        &destination,
        KernelStoreKind::Memory,
        "fixture-operator",
    )
    .unwrap();
    assert_eq!(source::census(&source_path).unwrap(), before);
    let store = open_store(
        &destination,
        KernelStoreKind::Memory,
        DbOpenContext::open_existing_deny(),
    )
    .unwrap();
    assert_eq!(
        store
            .get_state_kv("zeroclaw-fixture", "ordinary")
            .unwrap()
            .unwrap()
            .0,
        "{\"committed\":true}"
    );
    drop(store);
    writer.execute_batch("ROLLBACK").unwrap();
    close_connection(writer).unwrap();
}

#[test]
fn missing_original_shm_is_not_created_by_the_private_snapshot_reader() {
    let root = tempfile::TempDir::new().unwrap();
    let source_path = root.path().join("legacy.db");
    let destination = root.path().join("new.db");
    restore_fixture(&source_path, KernelStoreKind::Memory);
    let writer = snapshot::open_auxiliary_target(&source_path).unwrap();
    // SQLite's exclusive WAL mode keeps its index in memory and provides a
    // genuine DB+WAL input whose SHM has never been created.
    writer
        .execute_batch(
            "PRAGMA locking_mode=EXCLUSIVE; PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;",
        )
        .unwrap();
    writer.execute("UPDATE hard_state SET value_json='{\"without_shm\":true}' WHERE namespace='zeroclaw-fixture' AND key='ordinary'",[]).unwrap();
    assert!(source::suffixed(&source_path, "-wal").exists());
    assert!(!source::suffixed(&source_path, "-shm").exists());
    let before = source::census(&source_path).unwrap();
    migrate_kernel_store(
        &source_path,
        &destination,
        KernelStoreKind::Memory,
        "fixture-operator",
    )
    .unwrap();
    assert_eq!(source::census(&source_path).unwrap(), before);
    assert!(!source::suffixed(&source_path, "-shm").exists());
    close_connection(writer).unwrap();
}

#[test]
fn observed_source_change_during_copy_refuses_before_migration_or_publication() {
    let root = tempfile::TempDir::new().unwrap();
    let source_path = root.path().join("legacy.db");
    let destination = root.path().join("new.db");
    restore_fixture(&source_path, KernelStoreKind::Memory);
    let original = std::fs::read(&source_path).unwrap();
    let error = migrate_inner(
        &source_path,
        &destination,
        KernelStoreKind::Memory,
        "fixture-operator",
        || {
            // Deliberately mutate this test-owned file at a deterministic boundary.
            // No sleeps, concurrent scheduling assumptions, or production bypass.
            let mut file = OpenOptions::new().append(true).open(&source_path)?;
            file.write_all(b"fixture-source-changed")?;
            file.sync_all()?;
            Ok(())
        },
        |_| Ok(()),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("source file set"));
    let mut changed = original;
    changed.extend_from_slice(b"fixture-source-changed");
    assert_eq!(std::fs::read(&source_path).unwrap(), changed);
    assert_no_target(&destination);
}

#[test]
fn occupied_database_sidecars_and_source_aliases_are_never_overwritten() {
    let root = tempfile::TempDir::new().unwrap();
    let source_path = root.path().join("legacy.db");
    restore_fixture(&source_path, KernelStoreKind::Memory);
    let source_before = source::census(&source_path).unwrap();
    for (index, suffix) in TARGET_SUFFIXES.iter().enumerate() {
        let destination = root.path().join(format!("occupied-{index}.db"));
        let occupied = source::suffixed(&destination, suffix);
        std::fs::write(&occupied, b"foreign-file-must-survive").unwrap();
        let inode = occupied.metadata().unwrap().ino();
        let error = migrate_kernel_store(
            &source_path,
            &destination,
            KernelStoreKind::Memory,
            "fixture-operator",
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("occupied"));
        assert_eq!(
            std::fs::read(&occupied).unwrap(),
            b"foreign-file-must-survive"
        );
        assert_eq!(occupied.metadata().unwrap().ino(), inode);
    }
    let hardlink = root.path().join("source-alias.db");
    std::fs::hard_link(&source_path, &hardlink).unwrap();
    assert!(
        migrate_kernel_store(
            &source_path,
            &hardlink,
            KernelStoreKind::Memory,
            "fixture-operator"
        )
        .is_err()
    );
    assert!(
        migrate_kernel_store(
            &source_path,
            &source_path,
            KernelStoreKind::Memory,
            "fixture-operator"
        )
        .is_err()
    );
    let link = root.path().join("broken-target-link.db");
    symlink(root.path().join("does-not-exist"), &link).unwrap();
    assert!(
        migrate_kernel_store(
            &source_path,
            &link,
            KernelStoreKind::Memory,
            "fixture-operator"
        )
        .is_err()
    );
    assert!(
        std::fs::symlink_metadata(link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(source::census(&source_path).unwrap(), source_before);
    assert_stage_removed(root.path());
}

#[test]
fn publication_refuses_injected_sidecars_without_adopting_or_removing_them() {
    for suffix in ["-wal", "-shm", "-journal", ".identity.json"] {
        let root = tempfile::TempDir::new().unwrap();
        let source_path = root.path().join("legacy.db");
        let destination = root.path().join("new.db");
        restore_fixture(&source_path, KernelStoreKind::Memory);
        let before = source::census(&source_path).unwrap();
        let foreign = source::suffixed(&destination, suffix);
        let mut identity = None;
        let error = migrate_inner(
            &source_path,
            &destination,
            KernelStoreKind::Memory,
            "fixture-operator",
            || Ok(()),
            |published| {
                assert_eq!(published, destination);
                let mut file = create_private_file(&foreign)?;
                file.write_all(b"foreign-publication-sidecar")?;
                file.sync_all()?;
                identity = Some(source::physical_identity(&file.metadata()?)?);
                Ok(())
            },
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("occupied sidecar"),
            "{error:#}"
        );
        assert_eq!(
            std::fs::read(&foreign).unwrap(),
            b"foreign-publication-sidecar"
        );
        assert_eq!(
            source::physical_identity(&foreign.metadata().unwrap()).unwrap(),
            identity.unwrap()
        );
        assert!(!destination.exists());
        assert!(!source::suffixed(&destination, ".migration-marker").exists());
        assert_eq!(source::census(&source_path).unwrap(), before);
        assert_stage_removed(root.path());
    }
}

#[test]
fn publication_binds_reopen_to_held_database_and_preserves_a_replacement() {
    let root = tempfile::TempDir::new().unwrap();
    let source_path = root.path().join("legacy.db");
    let destination = root.path().join("new.db");
    let moved = root.path().join("moved-published.db");
    let foreign_sidecar = source::suffixed(&destination, "-wal");
    restore_fixture(&source_path, KernelStoreKind::Memory);
    let before = source::census(&source_path).unwrap();
    let mut replacement_identity = None;
    let error = migrate_inner(
        &source_path,
        &destination,
        KernelStoreKind::Memory,
        "fixture-operator",
        || Ok(()),
        |published| {
            std::fs::rename(published, &moved)?;
            let mut replacement = create_private_file(published)?;
            replacement.write_all(b"foreign-replacement-database")?;
            replacement.sync_all()?;
            replacement_identity = Some(source::physical_identity(&replacement.metadata()?)?);
            std::fs::write(&foreign_sidecar, b"foreign-replacement-wal")?;
            Ok(())
        },
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("identity changed"),
        "{error:#}"
    );
    assert_eq!(
        std::fs::read(&destination).unwrap(),
        b"foreign-replacement-database"
    );
    assert_eq!(
        source::physical_identity(&destination.metadata().unwrap()).unwrap(),
        replacement_identity.unwrap()
    );
    assert_eq!(
        std::fs::read(&foreign_sidecar).unwrap(),
        b"foreign-replacement-wal"
    );
    assert!(moved.exists());
    assert!(!source::suffixed(&destination, ".migration-marker").exists());
    assert_eq!(source::census(&source_path).unwrap(), before);
    assert_stage_removed(root.path());
}

#[test]
fn unsafe_parent_write_permissions_are_refused_and_published_permissions_are_private() {
    let root = tempfile::TempDir::new().unwrap();
    let source_path = root.path().join("legacy.db");
    restore_fixture(&source_path, KernelStoreKind::Memory);
    let parent = root.path().join("unsafe-parent");
    std::fs::create_dir(&parent).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o770)).unwrap();
    let destination = parent.join("new.db");
    let error = migrate_kernel_store(
        &source_path,
        &destination,
        KernelStoreKind::Memory,
        "fixture-operator",
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("permissions are unsafe"));
    assert_no_target(&destination);
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    let receipt = migrate_kernel_store(
        &source_path,
        &destination,
        KernelStoreKind::Memory,
        "fixture-operator",
    )
    .unwrap();
    assert_receipt(&destination, KernelStoreKind::Memory, &receipt);
    assert_eq!(destination.metadata().unwrap().mode() & 0o777, 0o600);
    assert_eq!(
        source::suffixed(&destination, ".migration-marker")
            .metadata()
            .unwrap()
            .mode()
            & 0o777,
        0o600
    );
    assert_stage_removed(&parent);
}

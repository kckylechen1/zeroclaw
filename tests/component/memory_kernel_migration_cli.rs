//! Offline recovery must dispatch before normal configuration and proxy boot.

use std::path::Path;
use std::process::{Command, Output};

fn migrate(config_dir: &Path, source: &Path, destination: &Path, store: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zeroclaw"))
        .arg("--config-dir")
        .arg(config_dir)
        .args(["memory", "migrate-kernel", "--source"])
        .arg(source)
        .arg("--destination")
        .arg(destination)
        .args(["--store", store, "--approved-by", "component-fixture"])
        .env("ZEROCLAW_CONFIG_DIR", config_dir)
        .env("LC_ALL", "C")
        .output()
        .expect("run the offline migration command")
}

fn assert_migration_error(output: &Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "missing source must be refused");
    assert!(
        output.stdout.is_empty(),
        "failed migration must not emit a receipt"
    );
    #[cfg(not(feature = "memory-tachi"))]
    assert!(
        stderr.contains("memory-tachi"),
        "unexpected error: {stderr}"
    );
    #[cfg(all(feature = "memory-tachi", unix))]
    assert!(
        stderr.to_lowercase().contains("source"),
        "unexpected error: {stderr}"
    );
    #[cfg(all(feature = "memory-tachi", not(unix)))]
    assert!(stderr.contains("Unix"), "unexpected error: {stderr}");
}

#[test]
fn kernel_migration_does_not_initialize_a_missing_config_directory() {
    let root = tempfile::tempdir().unwrap();
    let config_dir = root.path().join("not-configured");
    let source = root.path().join("missing-source.db");
    let destination = root.path().join("new.db");
    for store in ["memory", "companion"] {
        let output = migrate(&config_dir, &source, &destination, store);
        assert_migration_error(&output);
        assert!(!config_dir.exists());
        assert!(!source.exists());
        assert!(!destination.exists());
    }
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn kernel_migration_bypasses_malformed_agent_config() {
    let root = tempfile::tempdir().unwrap();
    let config_dir = root.path().join("config");
    std::fs::create_dir(&config_dir).unwrap();
    let config_path = config_dir.join("config.toml");
    let original = b"[proxy\nnot valid TOML, intentionally\n";
    std::fs::write(&config_path, original).unwrap();
    let source = root.path().join("missing-source.db");
    let destination = root.path().join("new.db");
    for store in ["memory", "companion"] {
        let output = migrate(&config_dir, &source, &destination, store);
        assert_migration_error(&output);
        assert_eq!(std::fs::read(&config_path).unwrap(), original);
        assert_eq!(std::fs::read_dir(&config_dir).unwrap().count(), 1);
        assert!(!destination.exists());
    }
}

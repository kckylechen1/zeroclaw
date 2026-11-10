//! Exercise the legacy CLI against the filesystem: its removed listener cannot
//! consume skip markers, so the command must fail with a localized migration.

use std::time::Duration;
use tokio::process::Command;

#[tokio::test]
async fn retired_skip_update_rejects_without_writing_markers_in_both_locales() {
    for (locale, diagnostic) in [("en", "are retired"), ("zh-CN", "已退役")] {
        let config = tempfile::tempdir().expect("config directory");
        std::fs::write(
            config.path().join("config.toml"),
            format!("schema_version = 3\nlocale = \"{locale}\"\n"),
        )
        .expect("write config");
        let skip_dir = config.path().join("data/telegram_skip");
        let marker = skip_dir.join("work.json");

        let mut help = Command::new(env!("CARGO_BIN_EXE_zeroclaw"));
        help.kill_on_drop(true)
            .env("ZEROCLAW_CONFIG_DIR", config.path())
            .env("RUST_LOG", "off")
            .args(["telegram", "skip-update", "--help"]);
        let output = tokio::time::timeout(Duration::from_secs(5), help.output())
            .await
            .expect("help must return promptly")
            .expect("execute legacy command help");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "help failed: {stdout}");
        assert!(
            stdout.contains(diagnostic) && stdout.contains("zeroclaw-bridge-telegram"),
            "{locale} help must describe retirement: {stdout}"
        );

        // A pre-existing marker must also stay untouched: reporting it as
        // pending would promise work that no production listener will perform.
        let previous =
            b"[{\"update_id\":12345,\"reason\":\"operator skip\",\"created_at_unix\":1}]";
        for existing_marker in [false, true] {
            if existing_marker {
                std::fs::create_dir_all(&skip_dir).expect("legacy marker directory");
                std::fs::write(&marker, previous).expect("legacy marker");
            }
            let mut command = Command::new(env!("CARGO_BIN_EXE_zeroclaw"));
            command
                .kill_on_drop(true)
                .env("ZEROCLAW_CONFIG_DIR", config.path())
                .env("RUST_LOG", "off")
                .args([
                    "telegram",
                    "skip-update",
                    "--alias",
                    "work",
                    "--update-id",
                    "12345",
                    "--reason",
                    "legacy operator request",
                ]);
            let output = tokio::time::timeout(Duration::from_secs(5), command.output())
                .await
                .expect("retired command must return promptly")
                .expect("execute legacy command");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success(),
                "retired command succeeded: {stderr}"
            );
            assert!(
                stderr.contains(diagnostic) && stderr.contains("zeroclaw-bridge-telegram"),
                "{locale} must explain the migration: {stderr}"
            );
            if existing_marker {
                assert_eq!(std::fs::read(&marker).unwrap(), previous);
            } else {
                assert!(!skip_dir.exists(), "retired command created a skip store");
            }
        }
    }
}

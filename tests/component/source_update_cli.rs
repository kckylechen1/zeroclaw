//! Exercise the real CLI boundary: legacy flags must never reach an upstream
//! release endpoint or replace the running fork.

use std::io::ErrorKind;
use std::net::TcpListener;
use std::time::Duration;
use tokio::process::Command;

#[tokio::test]
async fn source_only_update_rejects_legacy_modes_before_network_or_binary_mutation() {
    let config = tempfile::tempdir().expect("config directory");
    std::fs::write(
        config.path().join("config.toml"),
        "schema_version = 3\nlocale = \"en\"\n",
    )
    .expect("write config");
    let proxy = TcpListener::bind(("127.0.0.1", 0)).expect("loopback proxy sentinel");
    proxy.set_nonblocking(true).expect("nonblocking proxy");
    let proxy_url = format!("http://{}", proxy.local_addr().expect("proxy address"));
    let binary = env!("CARGO_BIN_EXE_zeroclaw");
    let before = std::fs::metadata(binary).expect("binary metadata");

    for args in [
        &["update"][..],
        &["update", "--force"],
        &["update", "--version", "9999.0.0", "--force"],
        &["update", "--check"],
        &["update", "--check", "--json"],
    ] {
        let mut command = Command::new(binary);
        command
            .kill_on_drop(true)
            .env("ZEROCLAW_CONFIG_DIR", config.path())
            .env("RUST_LOG", "off")
            .env("HTTP_PROXY", &proxy_url)
            .env("HTTPS_PROXY", &proxy_url)
            .env("ALL_PROXY", &proxy_url)
            .env("http_proxy", &proxy_url)
            .env("https_proxy", &proxy_url)
            .env("all_proxy", &proxy_url)
            .env_remove("NO_PROXY")
            .env_remove("no_proxy")
            .args(args);
        let output = tokio::time::timeout(Duration::from_secs(5), command.output())
            .await
            .expect("disabled update must return promptly")
            .expect("execute real update command");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?} unexpectedly succeeded");
        assert!(
            stderr.contains("https://github.com/kckylechen1/zeroclaw")
                && stderr.contains("./install.sh --source"),
            "{args:?} must provide fork source instructions: {stderr}"
        );
        match proxy.accept() {
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            other => panic!("update attempted a network connection: {other:?}"),
        }
    }

    let after = std::fs::metadata(binary).expect("binary metadata after commands");
    assert_eq!(before.len(), after.len());
    assert_eq!(
        before.modified().expect("original binary timestamp"),
        after.modified().expect("unchanged binary timestamp")
    );
}

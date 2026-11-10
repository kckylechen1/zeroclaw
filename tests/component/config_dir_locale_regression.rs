//! Regression: `--config-dir` must drive CLI locale detection, not only the
//! `ZEROCLAW_CONFIG_DIR` env var.
//!
//! Before the fix, locale was detected (and the clap help tree translated)
//! *before* the parsed `--config-dir` flag was applied to the environment, so
//! `--help` always rendered in the default/env locale and the flag was ignored
//! for every CLI string. These tests spawn the real binary (each invocation is a
//! fresh process, which is required because the resolved locale lives in a
//! process-global `OnceLock`) and assert the flag now selects the locale.
//!
//! Locale support converged to `en` + `zh-CN` (issue #584 Slice 1): the first
//! test drives the embedded `zh-CN` catalog; the second pins the compatibility
//! guarantee for configurations that still select a retired locale (`ja`,
//! `es`, `fr`) with catalogs fetched before retirement — the on-disk overlay
//! keeps rendering while the embedded English fallback covers the rest.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;

const ZH_CN_CLI_FTL: &str = include_str!("../../crates/zeroclaw-runtime/locales/zh-CN/cli.ftl");

/// Minimal catalog a user could still have on disk from a pre-retirement
/// `zeroclaw locales fetch ja`. Only the key under test is needed;
/// `--help` renders the long about, so that is the key overridden here.
const RETIRED_JA_DISK_CATALOG: &str =
    "cli-status-long-about =\n    ディスク上の引退済みロケール カタログによる状態説明。\n";

fn catalog_message<'a>(catalog: &'a str, key: &str) -> &'a str {
    catalog
        .lines()
        .find_map(|line| {
            let (candidate, value) = line.split_once(" = ")?;
            (candidate == key).then_some(value)
        })
        .unwrap_or_else(|| panic!("missing single-line Fluent message `{key}`"))
}

fn write_locale_config(dir: &Path, locale: &str) {
    std::fs::write(dir.join("config.toml"), format!("locale = \"{locale}\"\n")).unwrap();
}

fn help_stdout<I, S>(args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = Command::new(env!("CARGO_BIN_EXE_zeroclaw"))
        .args(args)
        .output()
        .expect("failed to run zeroclaw --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "zeroclaw help failed with status {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status.code()
    );
    stdout.into_owned()
}

#[test]
fn config_dir_flag_drives_cli_locale_space_form() {
    let tmp = tempfile::tempdir().unwrap();
    write_locale_config(tmp.path(), "zh-CN");
    let help = help_stdout([
        OsString::from("--config-dir"),
        tmp.path().as_os_str().to_owned(),
        OsString::from("--help"),
    ]);
    let expected = catalog_message(ZH_CN_CLI_FTL, "cli-about");
    assert!(
        help.contains(expected),
        "`--config-dir <zh-CN-dir> --help` must render Chinese; got:\n{help}"
    );
}

#[test]
fn config_dir_equals_flag_after_subcommand_drives_locale() {
    let tmp = tempfile::tempdir().unwrap();
    write_locale_config(tmp.path(), "ja");
    let disk_catalog = tmp.path().join("data/ftl/ja/cli.ftl");
    std::fs::create_dir_all(disk_catalog.parent().unwrap()).unwrap();
    std::fs::write(&disk_catalog, RETIRED_JA_DISK_CATALOG).unwrap();
    let mut config_arg = OsString::from("--config-dir=");
    config_arg.push(tmp.path());
    let help = help_stdout([
        OsString::from("status"),
        config_arg,
        OsString::from("--help"),
    ]);
    let expected = "ディスク上の引退済みロケール";
    assert!(
        help.contains(expected),
        "`status --config-dir=<ja-dir> --help` must render the on-disk retired-locale overlay; got:\n{help}"
    );
}

#[test]
fn stale_downloaded_catalog_cannot_restore_upstream_update_help() {
    for locale in ["zh-CN", "ja"] {
        let tmp = tempfile::tempdir().unwrap();
        write_locale_config(tmp.path(), locale);
        let disk_catalog = tmp.path().join(format!("data/ftl/{locale}/cli.ftl"));
        std::fs::create_dir_all(disk_catalog.parent().unwrap()).unwrap();
        std::fs::write(
            disk_catalog,
            "cli-update-about = STALE_DOWNLOAD_AND_INSTALL\n\
             cli-update-long-about = STALE_DOWNLOAD_AND_INSTALL --check --force\n",
        )
        .unwrap();
        let help = help_stdout([
            OsString::from("--config-dir"),
            tmp.path().as_os_str().to_owned(),
            OsString::from("update"),
            OsString::from("--help"),
        ]);
        assert!(help.contains("./install.sh --source"), "{locale}: {help}");
        assert!(!help.contains("STALE_DOWNLOAD_AND_INSTALL"), "{help}");
    }
}

#[tokio::test]
async fn locales_fetch_restores_same_build_catalogs_without_network() {
    use std::io::ErrorKind;
    use std::net::TcpListener;
    use std::time::Duration;

    let tmp = tempfile::tempdir().unwrap();
    write_locale_config(tmp.path(), "zh-CN");
    let destination = tmp.path().join("data/ftl/zh-CN");
    std::fs::create_dir_all(&destination).unwrap();
    std::fs::write(destination.join("cli.ftl"), "cli-update-about = stale\n").unwrap();
    let proxy = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    proxy.set_nonblocking(true).unwrap();
    let proxy_url = format!("http://{}", proxy.local_addr().unwrap());

    for catalogs in [Some("tools"), None] {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_zeroclaw"));
        command
            .kill_on_drop(true)
            .arg("--config-dir")
            .arg(tmp.path())
            .args(["locales", "fetch", "zh-CN"])
            .env("RUST_LOG", "off")
            .env_remove("NO_PROXY")
            .env_remove("no_proxy");
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            command.env(key, &proxy_url);
        }
        if let Some(catalogs) = catalogs {
            command.args(["--catalog", catalogs]);
        }
        let output = tokio::time::timeout(Duration::from_secs(5), command.output())
            .await
            .expect("bundled catalogue installation must not wait for network")
            .expect("run real locales command");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(
            std::fs::read_to_string(destination.join("tools.ftl")).unwrap(),
            include_str!("../../crates/zeroclaw-runtime/locales/zh-CN/tools.ftl"),
        );
        let cli = std::fs::read_to_string(destination.join("cli.ftl")).unwrap();
        if catalogs.is_some() {
            assert_eq!(cli, "cli-update-about = stale\n");
        } else {
            assert_eq!(cli, ZH_CN_CLI_FTL);
        }
        match proxy.accept() {
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            other => panic!("locales fetch attempted a network connection: {other:?}"),
        }
    }
}

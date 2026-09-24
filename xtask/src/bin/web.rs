//! `cargo xtask web` — drive the web dashboard build from cargo.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::Path;
use std::process::Command;
use xtask::util::{repo_root, require_tool, run_cmd};

#[derive(Parser, Debug)]
#[command(name = "web", about = "Build the ZeroClaw web dashboard")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run `npm install` in web/.
    Install,
    /// Run `npm run build` (installs dependencies first when needed).
    Build,
    /// Start `npm run dev` (installs dependencies first when needed).
    Dev,
    /// Typecheck (`tsc -b`) without producing a bundle.
    Check,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = repo_root();
    let web_dir = root.join("web");
    if matches!(cli.cmd, Cmd::Install) || node_modules_needs_install(&web_dir) {
        npm_install(&web_dir)?;
    }
    match cli.cmd {
        Cmd::Install => Ok(()),
        Cmd::Build => npm_run(&web_dir, "build"),
        Cmd::Dev => npm_run(&web_dir, "dev"),
        Cmd::Check => npx(&web_dir, &["tsc", "-b"]),
    }
}

fn npm_install(web_dir: &Path) -> Result<()> {
    require_tool("npm", "https://nodejs.org/ or `nvm install --lts`")?;
    println!("==> npm install ({})", web_dir.display());
    run_cmd(Command::new(bin("npm")).current_dir(web_dir).arg("install"))
}

fn node_modules_needs_install(web_dir: &Path) -> bool {
    let node_modules = web_dir.join("node_modules");
    if !node_modules.exists() {
        return true;
    }
    let sentinel = node_modules.join(".package-lock.json");
    let lock = web_dir.join("package-lock.json");
    let (Ok(sentinel_meta), Ok(lock_meta)) = (sentinel.metadata(), lock.metadata()) else {
        // Sentinel missing → install never completed cleanly. Lock missing → treat
        // node_modules as authoritative (no signal to re-install).
        return !sentinel.exists() && lock.exists();
    };
    match (sentinel_meta.modified(), lock_meta.modified()) {
        (Ok(sentinel_t), Ok(lock_t)) => lock_t > sentinel_t,
        _ => false,
    }
}

fn npm_run(web_dir: &Path, script: &str) -> Result<()> {
    println!("==> npm run {script}");
    run_cmd(
        Command::new(bin("npm"))
            .current_dir(web_dir)
            .args(["run", script]),
    )
}

fn npx(web_dir: &Path, args: &[&str]) -> Result<()> {
    println!("==> npx {}", args.join(" "));
    let mut cmd = Command::new(bin("npx"));
    cmd.current_dir(web_dir).arg("--no-install").args(args);
    run_cmd(&mut cmd)
}

fn bin(tool: &str) -> String {
    if cfg!(windows) {
        format!("{tool}.cmd")
    } else {
        tool.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::node_modules_needs_install;
    use std::fs;
    use std::thread::sleep;
    use std::time::Duration;
    use tempfile::TempDir;

    fn fresh_web_dir() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        fs::write(dir.path().join("package-lock.json"), "{}").unwrap();
        dir
    }

    #[test]
    fn needs_install_when_node_modules_missing() {
        let dir = fresh_web_dir();
        // No node_modules at all → must trigger install.
        assert!(node_modules_needs_install(dir.path()));
    }

    #[test]
    fn needs_install_when_sentinel_missing() {
        // node_modules exists but `.package-lock.json` sentinel was never written
        // (e.g. partial / failed previous install). Must trigger install.
        let dir = fresh_web_dir();
        fs::create_dir(dir.path().join("node_modules")).unwrap();
        assert!(node_modules_needs_install(dir.path()));
    }

    #[test]
    fn needs_install_when_lock_is_newer_than_sentinel() {
        // The actual staleness fix: a pull or merge updated package-lock.json
        // after the last install. Sentinel is OLDER than lock → reinstall.
        let dir = fresh_web_dir();
        let nm = dir.path().join("node_modules");
        fs::create_dir(&nm).unwrap();
        fs::write(nm.join(".package-lock.json"), "{}").unwrap();
        // Wait a measurable tick, then bump lock mtime.
        sleep(Duration::from_millis(20));
        fs::write(
            dir.path().join("package-lock.json"),
            "{ \"updated\": true }",
        )
        .unwrap();
        assert!(
            node_modules_needs_install(dir.path()),
            "stale node_modules (sentinel older than lock) must reinstall"
        );
    }

    #[test]
    fn skips_when_sentinel_is_at_least_as_new_as_lock() {
        // Clean install just finished. Sentinel mtime ≥ lock mtime → no
        // spurious reinstall on every build invocation.
        let dir = fresh_web_dir();
        let nm = dir.path().join("node_modules");
        fs::create_dir(&nm).unwrap();
        // Write sentinel AFTER lock so its mtime is newer.
        sleep(Duration::from_millis(20));
        fs::write(nm.join(".package-lock.json"), "{}").unwrap();
        assert!(
            !node_modules_needs_install(dir.path()),
            "fresh node_modules must not trigger spurious reinstall"
        );
    }

    #[test]
    fn skips_when_lock_missing() {
        // Defensive case: no package-lock.json (unusual — repo always has one).
        // Without a signal we cannot tell if node_modules is stale; treat as
        // valid to avoid a reinstall loop.
        let dir = TempDir::new().expect("tempdir");
        let nm = dir.path().join("node_modules");
        fs::create_dir(&nm).unwrap();
        fs::write(nm.join(".package-lock.json"), "{}").unwrap();
        assert!(!node_modules_needs_install(dir.path()));
    }
}

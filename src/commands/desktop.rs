//! `zeroclaw desktop` — launch or install the companion desktop app.

use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::gateway_helpers::{t, ta};

/// Launch the companion app, or print install / download instructions.
pub fn handle(do_install: bool) -> Result<()> {
    let download_url = "https://www.zeroclawlabs.ai/download";

    if do_install {
        println!(
            "{}",
            t(
                "cli-desktop-download",
                "Download the ZeroClaw companion app:"
            )
        );
        println!();
        #[cfg(target_os = "macos")]
        {
            println!("  macOS:  {download_url}"); // i18n-exempt: literal command/identifier example
            println!();
            println!(
                "{}",
                t(
                    "cli-desktop-homebrew",
                    "Or install via Homebrew (coming soon):"
                )
            );
            println!("  brew install --cask zeroclaw"); // i18n-exempt: literal command/identifier example
        }
        #[cfg(target_os = "linux")]
        {
            println!("  Linux:  {download_url}"); // i18n-exempt: literal command/identifier example
            println!();
            println!(
                "{}",
                t(
                    "cli-desktop-linux-pkg",
                    "  Download the .deb or .AppImage for your architecture."
                )
            );
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            println!("  {download_url}");
        }
        println!();

        // On macOS, open the download page in the browser
        #[cfg(target_os = "macos")]
        {
            let _ = std::process::Command::new("open").arg(download_url).spawn();
        }
        #[cfg(target_os = "linux")]
        {
            let _ = std::process::Command::new("xdg-open")
                .arg(download_url)
                .spawn();
        }
        return Ok(());
    }

    // Locate the companion app
    let desktop_bin = {
        let mut found = None;

        // 1. macOS: check /Applications/ZeroClaw.app
        #[cfg(target_os = "macos")]
        {
            let app_paths = [
                PathBuf::from("/Applications/ZeroClaw.app/Contents/MacOS/ZeroClaw"),
                PathBuf::from(std::env::var("HOME").unwrap_or_default())
                    .join("Applications/ZeroClaw.app/Contents/MacOS/ZeroClaw"),
            ];
            for app in &app_paths {
                if app.is_file() {
                    found = Some(app.clone());
                    break;
                }
            }
        }

        // 2. Same directory as the current executable
        if found.is_none()
            && let Ok(exe) = std::env::current_exe()
        {
            let sibling = exe.with_file_name("zeroclaw-desktop");
            if sibling.is_file() {
                found = Some(sibling);
            }
        }

        // 3. Common cargo/local install locations under the user's home directory.
        //    Uses directories::UserDirs so HOME (Unix) and USERPROFILE (Windows)
        //    are both resolved correctly. On Windows the binary is .exe — try
        //    both names since which::which (step 4) only catches PATH entries.
        if found.is_none()
            && let Some(home) = directories::UserDirs::new().map(|u| u.home_dir().to_path_buf())
        {
            let bin_names: &[&str] = if cfg!(windows) {
                &["zeroclaw-desktop.exe", "zeroclaw-desktop"]
            } else {
                &["zeroclaw-desktop"]
            };
            // .cargo/bin works the same on Windows; .local/bin is XDG (Unix only).
            let dirs: &[&str] = if cfg!(windows) {
                &[".cargo/bin"]
            } else {
                &[".cargo/bin", ".local/bin"]
            };
            'outer: for dir in dirs {
                for name in bin_names {
                    let candidate = home.join(dir).join(name);
                    if candidate.is_file() {
                        found = Some(candidate);
                        break 'outer;
                    }
                }
            }
        }

        // 4. Fallback to PATH lookup
        if found.is_none()
            && let Ok(path) = which::which("zeroclaw-desktop")
        {
            found = Some(path);
        }

        found
    };

    match desktop_bin {
        Some(bin) => {
            println!(
                "{}",
                t(
                    "cli-desktop-launching",
                    "Launching ZeroClaw companion app..."
                )
            );
            let _child = std::process::Command::new(&bin)
                .spawn()
                .with_context(|| format!("Failed to launch {}", bin.display()))?;
        }
        None => {
            println!(
                "{}",
                t(
                    "cli-desktop-not-installed",
                    "ZeroClaw companion app is not installed."
                )
            );
            println!();
            println!(
                "{}",
                ta(
                    "cli-desktop-download-at",
                    &[("url", download_url)],
                    "Download it at"
                )
            );
            println!("  Or run: zeroclaw desktop --install"); // i18n-exempt: literal command
            println!();
            println!(
                "{}",
                t(
                    "cli-desktop-blurb1",
                    "The companion app is a lightweight menu bar app that"
                )
            );
            println!(
                "{}",
                t(
                    "cli-desktop-blurb2",
                    "connects to the same gateway as the CLI."
                )
            );
            std::process::exit(1);
        }
    }
    Ok(())
}

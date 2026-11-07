//! `zeroclaw telegram` — Telegram channel operator tooling.

use anyhow::Result;
use std::path::{Path, PathBuf};
use zeroclaw_runtime::i18n::get_required_cli_string_with_args;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TelegramSkipMarker {
    pub update_id: i64,
    /// Operator-supplied reason, archived alongside the dead letter.
    pub reason: String,
    /// Unix epoch seconds when the CLI wrote the marker.
    pub created_at_unix: u64,
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Where the operator skip list for `alias` lives under the daemon's
/// `data_dir`. Shared contract between the CLI writer and the listener.
#[must_use]
pub fn telegram_skip_list_path(data_dir: &Path, alias: &str) -> PathBuf {
    data_dir.join("telegram_skip").join(format!("{alias}.json"))
}

/// Best-effort read of the skip list; a missing or malformed file means
/// "no skips" — the retry loop must never fail closed on operator file
/// trouble.
pub fn load_telegram_skip_markers(data_dir: &Path, alias: &str) -> Vec<TelegramSkipMarker> {
    let path = telegram_skip_list_path(data_dir, alias);
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str::<Vec<TelegramSkipMarker>>(&raw).unwrap_or_else(|err| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({
                        "path": path.display().to_string(),
                        "err": err.to_string(),
                    })),
                "telegram skip list unreadable; treating as empty"
            );
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}

/// Append a skip marker (the `zeroclaw telegram skip-update` entry
/// point). Atomic tmp+rename so the listener never observes a torn file.
/// Idempotent per update_id.
pub fn append_telegram_skip_marker(
    data_dir: &Path,
    alias: &str,
    update_id: i64,
    reason: &str,
) -> Result<(), String> {
    if alias.is_empty() || alias.contains('/') || alias.contains('\\') {
        return Err(format!(
            "invalid bot alias '{alias}': path separators are not allowed"
        ));
    }
    let path = telegram_skip_list_path(data_dir, alias);
    let mut markers = load_telegram_skip_markers(data_dir, alias);
    if markers.iter().any(|marker| marker.update_id == update_id) {
        return Ok(());
    }
    markers.push(TelegramSkipMarker {
        update_id,
        reason: reason.to_string(),
        created_at_unix: unix_now_secs(),
    });
    let parent = path
        .parent()
        .ok_or_else(|| "skip list has no parent".to_string())?;
    std::fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    let body = serde_json::to_string_pretty(&markers).map_err(|err| err.to_string())?;
    let tmp = path.with_extension("json.tmp");
    if let Ok(mut file) = std::fs::File::create(&tmp) {
        use std::io::Write as _;
        if let Ok(()) = file.write_all(body.as_bytes()) {
            let _ = file.sync_all();
        }
    } else {
        return Err(format!("write {}: open failed", tmp.display()));
    }
    std::fs::rename(&tmp, &path).map_err(|err| format!("rename into {}: {err}", path.display()))?;
    Ok(())
}

/// `zeroclaw telegram skip-update`: record an explicit operator skip
/// marker for a poisoned Telegram update. The running daemon consumes
/// the marker on its next retry, archives the raw payload as a dead
/// letter under its data directory, and advances its offset past the
/// update. Nothing is ever dropped automatically.
pub fn run_skip_update(
    config: &crate::config::Config,
    alias: &str,
    update_id: i64,
    reason: Option<String>,
) -> Result<()> {
    let reason = reason.unwrap_or_else(|| "operator skip".to_string());
    let args = [
        ("update_id", update_id.to_string()),
        ("alias", alias.to_string()),
    ];
    let arg_refs: &[(&str, &str)] = &[
        ("update_id", args[0].1.as_str()),
        ("alias", args[1].1.as_str()),
    ];

    if load_telegram_skip_markers(&config.data_dir, alias)
        .iter()
        .any(|marker| marker.update_id == update_id)
    {
        println!(
            "{}",
            get_required_cli_string_with_args("telegram-skip-update-pending", arg_refs)
        );
        return Ok(());
    }

    match append_telegram_skip_marker(&config.data_dir, alias, update_id, &reason) {
        Ok(()) => {
            println!(
                "{}",
                get_required_cli_string_with_args("telegram-skip-update-written", arg_refs)
            );
            Ok(())
        }
        Err(err) => {
            println!(
                "{}",
                get_required_cli_string_with_args(
                    "telegram-skip-update-failed",
                    &[arg_refs, &[("error", err.as_str())]].concat(),
                )
            );
            Err(anyhow::Error::msg(err))
        }
    }
}

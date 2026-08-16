//! `zeroclaw telegram` — Telegram channel operator tooling.

use anyhow::Result;
use zeroclaw_channels::telegram::{append_telegram_skip_marker, load_telegram_skip_markers};
use zeroclaw_runtime::i18n::get_required_cli_string_with_args;

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

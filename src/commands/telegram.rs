//! Compatibility diagnostics for retired in-process Telegram operator tooling.

use anyhow::Result;
use zeroclaw_runtime::i18n::get_required_cli_string;

/// Keep the legacy command parseable without writing markers that the
/// standalone Telegram bridge cannot consume.
pub fn run_skip_update(_alias: &str, _update_id: i64, _reason: Option<String>) -> Result<()> {
    anyhow::bail!(get_required_cli_string("telegram-skip-update-retired"))
}

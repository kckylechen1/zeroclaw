//! Source-only update policy for this fork.
//!
//! Upstream release assets do not identify or preserve this fork. Until a
//! verified fork release channel exists, reject every legacy updater entry
//! point before network access or executable mutation. Source installation
//! remains owned by `install.sh`.

use anyhow::{Result, bail};

pub fn unavailable() -> Result<()> {
    #[cfg(feature = "agent-runtime")]
    let message = zeroclaw_runtime::i18n::get_required_cli_string("cli-update-source-only");
    #[cfg(not(feature = "agent-runtime"))]
    let message = "This fork has no verified binary update channel. Update a checkout of \
        https://github.com/kckylechen1/zeroclaw and run ./install.sh --source with your \
        existing preset/features. Upstream releases cannot update this fork."
        .to_string();
    bail!(message)
}

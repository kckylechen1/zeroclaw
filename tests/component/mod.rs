#[cfg(feature = "agent-runtime")]
mod config_dir_locale_regression;
mod config_patch_cli;
mod config_persistence;
mod config_schema;
mod dockerignore_test;
mod gateway;
mod gemini_capabilities;
mod memory_kernel_migration_cli;
mod provider_resolution;
mod provider_schema;
#[cfg(all(feature = "agent-runtime", feature = "channel-telegram"))]
mod retired_telegram_cli;
mod security;
mod skills_bundle_cli;
mod source_update_cli;

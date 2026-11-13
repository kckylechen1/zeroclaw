//! Compatibility exports for the shared config-owned locale implementation.

pub use zeroclaw_config::i18n::{
    LocaleOption, available_locales, builtin_catalog, detect_locale, get_cli_string,
    get_cli_string_with_args, get_required_cli_string, get_required_cli_string_with_args,
    get_tool_description, init, normalize_locale,
};

#[cfg(test)]
pub(crate) use zeroclaw_config::i18n::get_english_cli_string_with_args;

//! `zeroclaw config` — inspect, mutate, migrate, and document configuration.

use anyhow::{Context, Result};
use dialoguer::Select;
use zeroclaw_config::api_error::{ConfigApiCode, ConfigApiError};

use crate::config::Config;
use crate::gateway_helpers::{t, ta};

/// Decorate the value at `path` in `config.toml` with a leading `# {comment}`
/// line, preserving any non-comment whitespace. Mirrors the gateway's
/// `apply_comments`. Best-effort — silently bails on parse errors so a
/// successful set isn't downgraded to a failure for a metadata problem.
async fn apply_comment_inline(
    config_path: &std::path::Path,
    path: &str,
    comment: &str,
) -> Result<()> {
    zeroclaw_config::comment_writer::apply_comments(
        config_path,
        &[(path.to_string(), comment.to_string())],
    )
    .await
    .context("failed to write comment annotation")
}

pub(crate) fn config_patch_prop_kind(
    config: &Config,
    path: &str,
) -> Option<crate::config::PropKind> {
    config
        .prop_fields()
        .into_iter()
        .find(|f| f.name == path)
        .map(|f| f.kind)
}

pub(crate) fn json_value_to_setprop_string(
    value: &serde_json::Value,
    config: &Config,
    path: &str,
    op_index: usize,
    json: bool,
) -> Result<String> {
    let kind = config_patch_prop_kind(config, path);
    match zeroclaw_config::typed_value::coerce_for_set_prop(value, kind) {
        Ok(value_str) => Ok(value_str),
        Err(err) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"path": path, "error": err.message.clone()})),
                "config patch coercion rejected JSON value"
            );
            let err = err.with_path(path).with_op_index(op_index);
            let human = err.message.clone();
            config_patch_fail_json_or_human(json, err, human)
        }
    }
}

pub(crate) fn config_patch_map_prop_error(
    err: anyhow::Error,
    path: &str,
    op_index: usize,
) -> ConfigApiError {
    let msg = err.to_string();
    if msg.starts_with("Unknown property") {
        ConfigApiError::path_not_found(path).with_op_index(op_index)
    } else {
        ConfigApiError::from_validation(err)
            .with_path(path)
            .with_op_index(op_index)
    }
}

pub(crate) fn config_patch_json_error(err: &ConfigApiError) -> Result<()> {
    eprintln!("{}", serde_json::to_string_pretty(err)?);
    std::process::exit(1);
}

pub(crate) fn config_patch_json_value_type_error(
    message: impl Into<String>,
    path: Option<String>,
    op_index: Option<usize>,
) -> ConfigApiError {
    let mut err = ConfigApiError::new(ConfigApiCode::ValueTypeMismatch, message.into());
    if let Some(path) = path {
        err = err.with_path(path);
    }
    if let Some(op_index) = op_index {
        err = err.with_op_index(op_index);
    }
    err
}

pub(crate) fn config_patch_fail_json_or_human<T>(
    json: bool,
    err: ConfigApiError,
    human: impl Into<String>,
) -> Result<T>
where
    T: Sized,
{
    if json {
        config_patch_json_error(&err)?;
    }
    anyhow::bail!("{}", human.into())
}

#[cfg(feature = "agent-runtime")]
fn model_path_provider_type(path: &str) -> Option<&'static str> {
    let parts: Vec<&str> = path.split('.').collect();
    if parts.len() != 5 || parts[0] != "providers" || parts[1] != "models" || parts[4] != "model" {
        return None;
    }
    let family = parts[2];
    zeroclaw_providers::list_model_providers()
        .iter()
        .find(|p| p.name == family)
        .map(|p| p.name)
}

fn map_key_for_prop_path<'a>(section_path: &str, prop_path: &'a str) -> Option<&'a str> {
    let tail = prop_path.strip_prefix(section_path)?.strip_prefix('.')?;
    let mut parts = tail.split('.');
    let key = parts.next().filter(|key| !key.is_empty())?;
    parts.next()?;
    Some(key)
}

/// Split `section_arg` into the map key under `section_path` with NOTHING after
/// it, the `config init <section>.<alias>` shape.
fn map_key_for_section_arg<'a>(section_path: &str, section_arg: &'a str) -> Option<&'a str> {
    let tail = section_arg.strip_prefix(section_path)?.strip_prefix('.')?;
    (!tail.is_empty() && !tail.contains('.')).then_some(tail)
}

/// Longest alias-materializable section whose path prefixes `path`, plus the
/// alias `split` extracts. `#[resource_key]` sections are excluded: their keys
/// are values from another domain (model id, voice, tool name) and may
/// themselves contain dots, so a dot split would yield a bogus alias.
fn alias_target_for_path<'a>(
    path: &'a str,
    split: impl Fn(&str, &'a str) -> Option<&'a str>,
) -> Option<(&'static str, &'a str)> {
    Config::map_key_sections()
        .into_iter()
        .filter(|section| section.kind == zeroclaw_config::traits::MapKeyKind::Map)
        .filter(|section| !section.resource_key)
        .filter_map(|section| split(section.path, path).map(|key| (section.path, key)))
        .max_by_key(|(section_path, _)| section_path.len())
}

/// `config init <section>.<alias>`: materialize a dynamic-map alias with schema
/// defaults. Returns the created `"<section>.<alias>"` path, or `None` when
/// `section_arg` is not a `<map-section>.<new-alias>` shape (the alias already
/// exists, the section is resource-keyed or a natural-key list, or the argument
/// is a plain nested prefix that `init_defaults` already handles). A reserved
/// alias is an error, not a silent no-op.
pub(crate) fn init_map_alias(config: &mut Config, section_arg: &str) -> Result<Option<String>> {
    let Some((section_path, alias)) = alias_target_for_path(section_arg, map_key_for_section_arg)
    else {
        return Ok(None);
    };
    match zeroclaw_config::alias_refs::create_map_key_checked(config, section_path, alias) {
        Ok(true) => Ok(Some(format!("{section_path}.{alias}"))),
        Ok(false) => Ok(None),
        Err(e) => Err(anyhow::Error::msg(e.to_string())),
    }
}

/// Dirty every generated leaf under a newly created map alias so required
/// default-valued fields survive the incremental writer's empty-leaf pruning.
fn mark_new_map_alias_dirty(config: &mut Config, alias_path: &str) {
    let prefix = format!("{alias_path}.");
    let leaf_paths: Vec<String> = config
        .prop_fields()
        .into_iter()
        .filter_map(|field| field.name.starts_with(&prefix).then_some(field.name))
        .collect();

    if leaf_paths.is_empty() {
        config.mark_dirty(alias_path);
    } else {
        for path in leaf_paths {
            config.mark_dirty(&path);
        }
    }
}

pub(crate) fn ensure_map_key_for_prop_path(config: &mut Config, prop_path: &str) -> Result<bool> {
    let Some((section_path, key)) = alias_target_for_path(prop_path, map_key_for_prop_path) else {
        return Ok(false);
    };

    // The alias already exists in the loaded config (e.g. a hyphenated cron
    // alias the TOML loader accepts and `config get`/`config list` resolve):
    // leave it alone. `create_map_key` applies the strict new-alias grammar,
    // which would reject a valid loaded key. Mirror `Config::ensure_map_key_for_path`,
    // which also skips creation for existing keys so alias validation runs only
    // when auto-materializing a brand-new alias.
    if config
        .get_map_keys(section_path)
        .is_some_and(|keys| keys.iter().any(|k| k == key))
    {
        return Ok(false);
    }

    // Route through the shared `create_map_key_checked` (not raw
    // `create_map_key`) so this CLI path inherits the reserved `default`
    // agent guard from the one place it's defined, rather than re-deriving
    // `section == "agents" && is_reserved_agent_alias(key)` here too. Without
    // this, widening past `providers.*` would let `config set
    // agents.default.enabled ...` auto-create the reserved runtime-fallback
    // agent alias, which the rename guard then refuses to ever rename.
    let created =
        match zeroclaw_config::alias_refs::create_map_key_checked(config, section_path, key) {
            Ok(created) => created,
            Err(zeroclaw_config::alias_refs::CreateError::Reserved(_)) => return Ok(false),
            Err(e) => return Err(anyhow::Error::msg(e.to_string())),
        };
    if created {
        // The section matched and the alias was newly materialized, but the
        // requested prop path might still not resolve (typo'd trailing field
        // name, or belt-and-suspenders against a resource-key path that
        // slipped past the `!resource_key` filter above). Roll back rather
        // than leave a phantom alias, falling through to the normal
        // "Unknown property" error exactly as before this alias existed.
        //
        // IMPORTANT: this probe/rollback must stay strictly inside the
        // `if created` branch. `create_map_key` returns `Ok(false)` when the
        // alias already existed (idempotent case) — never run this rollback
        // when `created == false`, or a bogus tail-field on an
        // ALREADY-EXISTING alias would delete a legitimate, pre-existing
        // config entry that has nothing to do with this call.
        if config.get_prop(prop_path).is_err() {
            let _ = config.delete_map_key(section_path, key);
            return Ok(false);
        }
        config.mark_dirty(&format!("{section_path}.{key}"));
    }
    Ok(created)
}

pub async fn handle(config_command: crate::ConfigCommands, config: &mut Config) -> Result<()> {
    match config_command {
        crate::ConfigCommands::Schema { path } => {
            #[cfg(feature = "schema-export")]
            {
                let schema = schemars::schema_for!(crate::config::Config);
                let value = match path.as_deref() {
                    None => {
                        serde_json::to_value(&schema).context("failed to serialize JSON Schema")?
                    }
                    Some(prop_path) => {
                        let full = serde_json::to_value(&schema)
                            .context("failed to serialize JSON Schema")?;
                        let mut out = full;
                        if let serde_json::Value::Object(ref mut map) = out {
                            map.insert(
                                "x-zeroclaw-requested-path".into(),
                                serde_json::Value::String(prop_path.into()),
                            );
                        }
                        out
                    }
                };
                println!("{}", serde_json::to_string_pretty(&value)?);
                Ok(())
            }
            #[cfg(not(feature = "schema-export"))]
            {
                let _ = path;
                anyhow::bail!("zeroclaw was built without the 'schema-export' feature")
            }
        }
        crate::ConfigCommands::List { filter, secrets } => {
            let entries = config.prop_fields();
            println!(
                "{}",
                t(
                    "cli-config-legend",
                    "Legend: \u{1f489} env-overridden  \u{1f512} secret"
                )
            );
            println!();
            let mut current_category = "";
            for entry in &entries {
                if secrets && !entry.is_secret {
                    continue;
                }
                if let Some(ref f) = filter
                    && !entry.name.starts_with(f.as_str())
                {
                    continue;
                }
                if entry.category != current_category {
                    if !current_category.is_empty() {
                        println!();
                    }
                    println!("{}:", entry.category);
                    current_category = entry.category;
                }
                let env = if config.prop_is_env_overridden(&entry.name) {
                    "\u{1f489} "
                } else {
                    "  "
                };
                let lock = if entry.is_secret { " \u{1f512}" } else { "" };
                println!(
                    "{env}{:<45} = {:<20} ({}){lock}",
                    entry.name, entry.display_value, entry.type_hint
                );
            }
            Ok(())
        }
        crate::ConfigCommands::Get { path, json } => {
            let known_paths: Vec<String> =
                config.prop_fields().into_iter().map(|f| f.name).collect();
            let path = zeroclaw_config::helpers::resolve_field_path(&known_paths, &path);
            if Config::prop_is_secret(&path) {
                let entries = config.prop_fields();
                let populated = entries
                    .iter()
                    .find(|e| e.name == path)
                    .map(|e| e.display_value != "<unset>")
                    .unwrap_or(false);
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "path": path,
                            "populated": populated,
                        }))?
                    );
                } else if populated {
                    println!(
                        "{}",
                        ta(
                            "cli-config-secret-set",
                            &[("path", &path)],
                            "is set (encrypted secret, value not displayed)"
                        )
                    );
                } else {
                    println!(
                        "{}",
                        ta(
                            "cli-config-secret-unset",
                            &[("path", &path)],
                            "is not set (encrypted secret)"
                        )
                    );
                }
            } else {
                match config.get_prop(&path) {
                    Ok(value) => {
                        if json {
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&serde_json::json!({
                                    "path": path,
                                    "value": value,
                                }))?
                            );
                        } else {
                            println!("{value}");
                        }
                    }
                    Err(e) => {
                        // Classify the anyhow string into a stable code so
                        // the CLI's --json envelope matches the HTTP shape.
                        // Same single-source-of-truth helper the gateway
                        // uses; never hardcode a code at the call site.
                        let api_err = zeroclaw_config::api_error::ConfigApiError::from_validation(
                            anyhow::Error::msg(e.to_string()),
                        )
                        .with_path(&path);
                        if json {
                            eprintln!("{}", serde_json::to_string_pretty(&api_err)?);
                            std::process::exit(1);
                        }
                        anyhow::bail!("{e}");
                    }
                }
            }
            Ok(())
        }
        crate::ConfigCommands::Set {
            path,
            value,
            no_interactive,
            comment,
            json,
        } => {
            crate::config::migration::ensure_disk_at_current_version(&config.config_path)?;
            let known_paths: Vec<String> =
                config.prop_fields().into_iter().map(|f| f.name).collect();
            let mut path = zeroclaw_config::helpers::resolve_field_path(&known_paths, &path);
            if ensure_map_key_for_prop_path(config, &path)? {
                let known_paths: Vec<String> =
                    config.prop_fields().into_iter().map(|f| f.name).collect();
                path = zeroclaw_config::helpers::resolve_field_path(&known_paths, &path);
            }
            if no_interactive {
                let val = value.ok_or_else(|| {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({"path": path})),
                        "config set --no-interactive refused: positional value missing"
                    );
                    anyhow::Error::msg(format!(
                        "Value required in --no-interactive mode. Usage: zeroclaw config set --no-interactive {path} <value>"
                    ))
                })?;
                config.set_prop_persistent(&path, &val)?;
            } else if Config::prop_is_secret(&path) {
                if value.is_some() {
                    eprintln!(
                        "  \u{26a0} {path} is an encrypted secret \u{2014} using masked input."
                    );
                }
                let secret_value = dialoguer::Password::new()
                    .with_prompt(format!("Enter value for {path}"))
                    .interact()?;
                let secret_value = secret_value.trim().to_string();
                if secret_value.is_empty() {
                    anyhow::bail!("Value cannot be empty.");
                }
                config.set_prop_persistent(&path, &secret_value)?;
            } else if let Some(val) = value {
                config.set_prop_persistent(&path, &val)?;
            } else if let Some(provider_type) = model_path_provider_type(&path) {
                use dialoguer::{FuzzySelect, Input};
                let provider_ref = path
                    .split('.')
                    .nth(3)
                    .map(|alias| format!("{provider_type}.{alias}"));
                let catalog_selector = provider_ref.as_deref().unwrap_or(provider_type);
                let (models, _pricing, live) =
                    zeroclaw_runtime::quickstart::model_catalog_with_config(
                        Some(config),
                        catalog_selector,
                    )
                    .await;
                if live && !models.is_empty() {
                    let current = config.get_prop(&path).unwrap_or_default();
                    let default = models.iter().position(|m| m == &current).unwrap_or(0);
                    let Some(idx) = FuzzySelect::new()
                        .with_prompt(format!("Model id for {provider_type}"))
                        .items(&models)
                        .default(default)
                        .max_length(models.len().max(1))
                        .interact_opt()?
                    else {
                        anyhow::bail!("cancelled");
                    };
                    config.set_prop_persistent(&path, &models[idx])?;
                } else {
                    eprintln!(
                        "  no live catalog for `{provider_type}` — \
                         enter the model id manually."
                    );
                    let m = Input::<String>::new()
                        .with_prompt(format!("Model id for {provider_type}"))
                        .allow_empty(false)
                        .interact_text()?;
                    config.set_prop_persistent(&path, &m)?;
                }
            } else {
                let field_info = config.prop_fields().into_iter().find(|f| f.name == path);
                let variants = field_info.as_ref().and_then(|info| {
                    let get_variants = info.enum_variants?;
                    let variants = get_variants();
                    let current_index = variants
                        .iter()
                        .position(|v| v == &info.display_value)
                        .unwrap_or(0);
                    Some((variants, current_index))
                });
                if let Some((variants, current_index)) = variants {
                    let selected = Select::new()
                        .with_prompt(format!("Select value for {path}"))
                        .items(&variants)
                        .default(current_index)
                        .interact()?;
                    config.set_prop_persistent(&path, &variants[selected])?;
                } else if field_info
                    .as_ref()
                    .is_some_and(|f| f.kind == crate::config::PropKind::StringArray)
                {
                    let current_items: Vec<String> = field_info
                        .as_ref()
                        .and_then(|f| {
                            let raw = toml::from_str::<toml::Value>(&format!(
                                "v = {}",
                                if f.display_value == "<unset>" {
                                    "[]".to_string()
                                } else {
                                    f.display_value.clone()
                                }
                            ))
                            .ok();
                            raw.and_then(|v| v.get("v").cloned())
                                .and_then(|v| v.as_array().cloned())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                                        .collect()
                                })
                        })
                        .unwrap_or_default();
                    let editor_content = current_items.join("\n");
                    let edited = dialoguer::Editor::new()
                        .edit(&editor_content)?
                        .unwrap_or(editor_content);
                    let val = edited
                        .lines()
                        .map(|l| l.trim())
                        .filter(|l| !l.is_empty())
                        .collect::<Vec<_>>()
                        .join(", ");
                    config.set_prop_persistent(&path, &val)?;
                } else {
                    anyhow::bail!("Value required. Usage: zeroclaw config set {path} <value>");
                }
            }
            Box::pin(config.save_dirty()).await?;
            if let Some(c) = comment.as_ref()
                && !c.is_empty()
            {
                apply_comment_inline(&config.config_path, &path, c).await?;
            }
            if json {
                let envelope = if Config::prop_is_secret(&path) {
                    serde_json::json!({"path": path, "populated": true})
                } else {
                    let value_str = config.get_prop(&path).unwrap_or_default();
                    serde_json::json!({"path": path, "value": value_str})
                };
                println!("{}", serde_json::to_string_pretty(&envelope)?);
            } else {
                println!(
                    "{}",
                    ta("cli-config-updated", &[("path", &path)], "updated")
                );
            }
            Ok(())
        }
        crate::ConfigCommands::Init { section, json } => {
            crate::config::migration::ensure_disk_at_current_version(&config.config_path)?;
            let mut initialized: Vec<String> = config
                .init_defaults(section.as_deref())
                .into_iter()
                .map(str::to_string)
                .collect();
            for section in &initialized {
                config.mark_dirty(section);
            }
            // `init_defaults` only instantiates nested struct sections. A
            // `<section>.<alias>` argument names a dynamic-map entry, which
            // has to be materialized through `create_map_key` instead.
            if let Some(arg) = section.as_deref()
                && let Some(created) = init_map_alias(config, arg)?
            {
                mark_new_map_alias_dirty(config, &created);
                initialized.push(created);
            }
            if !initialized.is_empty() {
                Box::pin(config.save_dirty()).await?;
            }
            if json {
                let envelope = serde_json::json!({"initialized": initialized});
                println!("{}", serde_json::to_string_pretty(&envelope)?);
            } else if initialized.is_empty() {
                println!(
                    "{}",
                    t(
                        "cli-config-all-configured",
                        "All sections already configured."
                    )
                );
            } else {
                println!(
                    "Initialized {} section(s) with defaults:",
                    initialized.len()
                );
                for name in &initialized {
                    println!("  {name}");
                }
                println!(
                    "\n{}",
                    t(
                        "cli-config-review-hint",
                        "Run `zeroclaw config list` to review, then set required fields."
                    )
                );
            }
            Ok(())
        }
        crate::ConfigCommands::Migrate { json } => {
            match crate::config::migration::migrate_file_in_place(&config.config_path)? {
                Some(report) => {
                    let to = report.to_version;
                    if json {
                        let envelope = serde_json::json!({
                            "migrated": true,
                            "backup_path": report.backup_path.display().to_string(),
                            "schema_version": to,
                        });
                        println!("{}", serde_json::to_string_pretty(&envelope)?);
                    } else {
                        println!(
                            "{}",
                            ta(
                                "cli-config-backed-up",
                                &[("path", &report.backup_path.display().to_string())],
                                "Backed up to"
                            )
                        );
                        println!(
                            "Migrated {} to schema version {to}.",
                            config.config_path.display()
                        );
                    }
                }
                None => {
                    let strict_error =
                        std::fs::read_to_string(&config.config_path)
                            .ok()
                            .and_then(|raw| {
                                crate::config::migration::migrate_to_current(&raw)
                                    .err()
                                    .map(|e| format!("{e:#}"))
                            });
                    if json {
                        let envelope = serde_json::json!({
                            "migrated": false,
                            "schema_version": crate::config::migration::CURRENT_SCHEMA_VERSION,
                            "valid": strict_error.is_none(),
                            "error": strict_error,
                        });
                        println!("{}", serde_json::to_string_pretty(&envelope)?);
                        if strict_error.is_some() {
                            std::process::exit(1);
                        }
                    } else {
                        println!(
                            "{}",
                            t(
                                "cli-config-schema-current",
                                "Config already at current schema version."
                            )
                        );
                        if let Some(error) = strict_error {
                            anyhow::bail!(
                                "config at {} does not deserialize strictly; the resilient \
                                 loader is substituting defaults for the failing section. \
                                 Parse error: {error}",
                                config.config_path.display()
                            );
                        }
                    }
                }
            }
            Ok(())
        }
        crate::ConfigCommands::Patch { input, json } => {
            crate::config::migration::ensure_disk_at_current_version(&config.config_path)?;
            let body = match input.as_deref() {
                None | Some("-") => {
                    use std::io::Read;
                    let mut buf = String::new();
                    if let Err(err) = std::io::stdin().read_to_string(&mut buf) {
                        let api_err = ConfigApiError::new(
                            ConfigApiCode::InternalError,
                            format!("failed to read JSON Patch from stdin: {err}"),
                        );
                        config_patch_fail_json_or_human(
                            json,
                            api_err,
                            format!("Failed to read JSON Patch from stdin: {err}"),
                        )?;
                    }
                    buf
                }
                Some(path) => match tokio::fs::read_to_string(path).await {
                    Ok(body) => body,
                    Err(err) => {
                        let api_err = ConfigApiError::new(
                            ConfigApiCode::InternalError,
                            format!("failed to read JSON Patch from {path}: {err}"),
                        );
                        config_patch_fail_json_or_human(
                            json,
                            api_err,
                            format!("Failed to read JSON Patch from {path}: {err}"),
                        )?
                    }
                },
            };

            let parsed: serde_json::Value = match serde_json::from_str(body.trim()) {
                Ok(parsed) => parsed,
                Err(err) => {
                    let api_err = config_patch_json_value_type_error(
                        format!("JSON Patch body must be valid JSON: {err}"),
                        None,
                        None,
                    );
                    config_patch_fail_json_or_human(
                        json,
                        api_err,
                        format!("JSON Patch body must be valid JSON: {err}"),
                    )?
                }
            };
            let ops = match parsed.as_array() {
                Some(ops) => ops,
                None => {
                    let api_err = config_patch_json_value_type_error(
                        "JSON Patch body must be a JSON array of operations",
                        None,
                        None,
                    );
                    config_patch_fail_json_or_human(
                        json,
                        api_err,
                        "JSON Patch body must be a JSON array of operations",
                    )?
                }
            };

            let mut results: Vec<serde_json::Value> = Vec::with_capacity(ops.len());

            for (idx, op) in ops.iter().enumerate() {
                let object = match op.as_object() {
                    Some(object) => object,
                    None => {
                        let message = format!("JSON Patch op[{idx}] must be an object");
                        let api_err =
                            config_patch_json_value_type_error(message.clone(), None, Some(idx));
                        config_patch_fail_json_or_human(json, api_err, message)?
                    }
                };
                let op_name = match object.get("op").and_then(|v| v.as_str()) {
                    Some(op_name) => op_name,
                    None => {
                        let message = format!("JSON Patch op[{idx}] requires string `op` field");
                        let api_err =
                            config_patch_json_value_type_error(message.clone(), None, Some(idx));
                        config_patch_fail_json_or_human(json, api_err, message)?
                    }
                };
                let raw_path = match object.get("path").and_then(|v| v.as_str()) {
                    Some(raw_path) => raw_path,
                    None => {
                        let message = format!("JSON Patch op[{idx}] requires string `path` field");
                        let api_err =
                            config_patch_json_value_type_error(message.clone(), None, Some(idx));
                        config_patch_fail_json_or_human(json, api_err, message)?
                    }
                };
                let path = if let Some(stripped) = raw_path.strip_prefix('/') {
                    stripped.replace('/', ".")
                } else {
                    raw_path.to_string()
                };
                if matches!(op_name, "add" | "replace") && config.ensure_map_key_for_path(&path) {
                    let err = ConfigApiError::new(
                        ConfigApiCode::ValidationFailed,
                        "alias `default` is reserved and cannot be created",
                    )
                    .with_path(&path)
                    .with_op_index(idx);
                    let human = format!(
                        "op[{idx}] `{op_name}` on `{path}`: alias `default` is reserved and cannot be created"
                    );
                    config_patch_fail_json_or_human(json, err, human)?;
                }
                let comment = match object.get("comment") {
                    Some(value) => match value.as_str() {
                        Some(comment) => Some(comment),
                        None => {
                            let message =
                                format!("JSON Patch op[{idx}] `comment` field must be a string");
                            let api_err = config_patch_json_value_type_error(
                                message.clone(),
                                Some(path.clone()),
                                Some(idx),
                            );
                            config_patch_fail_json_or_human(json, api_err, message)?
                        }
                    },
                    None => None,
                };
                let is_secret = Config::prop_is_secret(&path);

                let result_entry: serde_json::Value = match op_name {
                    "add" | "replace" => {
                        let value = match op.get("value") {
                            Some(value) => value,
                            None => {
                                ::zeroclaw_log::record!(
                                    WARN,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Reject
                                    )
                                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                                    .with_attrs(
                                        ::serde_json::json!({
                                            "op": op_name,
                                            "op_index": idx,
                                            "path": path,
                                        })
                                    ),
                                    "config patch op rejected: missing `value` field"
                                );
                                let message = format!(
                                    "op[{idx}] `{op_name}` on `{path}`: missing `value` field"
                                );
                                let api_err = config_patch_json_value_type_error(
                                    message.clone(),
                                    Some(path.clone()),
                                    Some(idx),
                                );
                                config_patch_fail_json_or_human(json, api_err, message)?
                            }
                        };
                        let value_str =
                            json_value_to_setprop_string(value, config, &path, idx, json)?;
                        match config.set_prop_persistent(&path, &value_str) {
                            Ok(()) => {}
                            Err(err) => {
                                let api_err = config_patch_map_prop_error(err, &path, idx);
                                let human = format!(
                                    "op[{idx}] `{op_name}` on `{path}` failed: {}",
                                    api_err.message
                                );
                                config_patch_fail_json_or_human(json, api_err, human)?;
                            }
                        }
                        if is_secret {
                            serde_json::json!({
                                "op": op_name,
                                "path": path,
                                "populated": !value_str.is_empty(),
                            })
                        } else {
                            serde_json::json!({
                                "op": op_name,
                                "path": path,
                                "value": value_str,
                            })
                        }
                    }
                    "remove" => {
                        match config.set_prop_persistent(&path, "") {
                            Ok(()) => {}
                            Err(err) => {
                                let api_err = config_patch_map_prop_error(err, &path, idx);
                                let human = format!(
                                    "op[{idx}] `remove` on `{path}` failed: {}",
                                    api_err.message
                                );
                                config_patch_fail_json_or_human(json, api_err, human)?;
                            }
                        }
                        if is_secret {
                            serde_json::json!({
                                "op": "remove",
                                "path": path,
                                "populated": false,
                            })
                        } else {
                            serde_json::json!({
                                "op": "remove",
                                "path": path,
                                "value": serde_json::Value::Null,
                            })
                        }
                    }
                    "test" => {
                        if is_secret {
                            let err =
                                ConfigApiError::secret_test_forbidden(&path).with_op_index(idx);
                            let human = format!(
                                "op[{idx}] `test` on `{path}`: secret_test_forbidden \
                                 \u{2014} test ops are not allowed against secret paths"
                            );
                            config_patch_fail_json_or_human(json, err, human)?;
                        }
                        let want = match op.get("value") {
                            Some(value) => value,
                            None => {
                                let err = ConfigApiError::new(
                                    ConfigApiCode::ValueTypeMismatch,
                                    "JSON Patch `test` op requires `value` field",
                                )
                                .with_path(&path)
                                .with_op_index(idx);
                                let human =
                                    format!("op[{idx}] `test` on `{path}`: missing `value` field");
                                config_patch_fail_json_or_human(json, err, human)?
                            }
                        };
                        let actual = match config.get_prop(&path) {
                            Ok(actual) => actual,
                            Err(err) => {
                                let human = format!(
                                    "op[{idx}] `test` on `{path}` failed to read current value: {err}"
                                );
                                let api_err = config_patch_map_prop_error(err, &path, idx);
                                config_patch_fail_json_or_human(json, api_err, human)?
                            }
                        };
                        let want_str = match zeroclaw_config::typed_value::coerce_for_set_prop(
                            want,
                            config_patch_prop_kind(config, &path),
                        ) {
                            Ok(want_str) => want_str,
                            Err(err) => {
                                let err = err.with_path(&path).with_op_index(idx);
                                config_patch_fail_json_or_human(
                                    json,
                                    err.clone(),
                                    err.message.clone(),
                                )?
                            }
                        };
                        if actual != want_str {
                            let err = ConfigApiError::new(
                                ConfigApiCode::ValidationFailed,
                                format!("`test` op failed: expected {want_str:?}, got {actual:?}"),
                            )
                            .with_path(&path)
                            .with_op_index(idx);
                            let human = format!(
                                "op[{idx}] `test` on `{path}` failed: expected {want_str}, got {actual}"
                            );
                            config_patch_fail_json_or_human(json, err, human)?;
                        }
                        serde_json::json!({
                            "op": "test",
                            "path": path,
                            "value": actual,
                        })
                    }
                    "move" | "copy" => {
                        let err = ConfigApiError::op_not_supported(op_name)
                            .with_path(&path)
                            .with_op_index(idx);
                        let human = format!(
                            "op[{idx}] `{op_name}` on `{path}`: op_not_supported \
                             \u{2014} move/copy require a reference graph that is not built yet"
                        );
                        config_patch_fail_json_or_human(json, err, human)?
                    }
                    other => {
                        let err = ConfigApiError::new(
                            ConfigApiCode::OpNotSupported,
                            format!("unknown JSON Patch operation `{other}`"),
                        )
                        .with_path(&path)
                        .with_op_index(idx);
                        let human = format!("op[{idx}] unknown JSON Patch operation `{other}`");
                        config_patch_fail_json_or_human(json, err, human)?
                    }
                };
                results.push(result_entry);
            }

            if let Err(err) = config.validate() {
                let api_err = ConfigApiError::from_validation(err);
                let human = format!(
                    "validation failed after applying patch \u{2014} no changes saved: {}",
                    api_err.message
                );
                config_patch_fail_json_or_human(json, api_err, human)?;
            }
            Box::pin(config.save_dirty()).await?;

            if json {
                let body = serde_json::json!({"saved": true, "results": results});
                println!("{}", serde_json::to_string_pretty(&body)?);
            } else {
                println!(
                    "{}",
                    ta(
                        "cli-config-applied-ops",
                        &[("count", &results.len().to_string())],
                        "Applied operations"
                    )
                );
                for entry in &results {
                    let op = entry.get("op").and_then(|v| v.as_str()).unwrap_or("?");
                    let path = entry.get("path").and_then(|v| v.as_str()).unwrap_or("?");
                    if let Some(populated) = entry.get("populated").and_then(|v| v.as_bool()) {
                        let lock = "\u{1f512}";
                        let label = if populated { "set" } else { "unset" };
                        println!("  {op:<8} {path}  {lock} ({label})");
                    } else {
                        let value = entry
                            .get("value")
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "null".to_string());
                        println!("  {op:<8} {path} = {value}");
                    }
                }
            }
            Ok(())
        }
        crate::ConfigCommands::Docs => {
            let port = config.gateway.port;
            let host = if config.gateway.host == "[::]" || config.gateway.host == "0.0.0.0" {
                "127.0.0.1".to_string()
            } else {
                config.gateway.host.clone()
            };
            let url = format!("http://{host}:{port}/api/docs");

            let health = format!("http://{host}:{port}/health");
            let daemon_running = reqwest::Client::new()
                .get(&health)
                .timeout(std::time::Duration::from_secs(2))
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false);

            println!("{url}");
            if !daemon_running {
                eprintln!(
                    "Note: gateway does not appear to be running at {host}:{port}. \
                     Start it with `zeroclaw service start` (background) or `zeroclaw daemon` (foreground) to load the explorer."
                );
            }
            Ok(())
        }
        crate::ConfigCommands::Complete { partial } => {
            let prefix = partial.as_deref().unwrap_or("");
            for entry in config.prop_fields() {
                if entry.name.starts_with(prefix) {
                    println!("{}", entry.name);
                }
            }
            Ok(())
        }
        crate::ConfigCommands::Generate { version, encrypt } => {
            let target = version.unwrap_or(crate::config::migration::CURRENT_SCHEMA_VERSION);
            let zeroclaw_dir = config
                .config_path
                .parent()
                .map(std::path::Path::to_path_buf);
            let opts = crate::config::migration::GenerateOptions {
                encrypt_secrets: encrypt,
                secret_store_dir: zeroclaw_dir.as_deref(),
            };
            let toml_out = crate::config::migration::generate(target, &opts)?;
            print!("{toml_out}");
            Ok(())
        }
    }
}

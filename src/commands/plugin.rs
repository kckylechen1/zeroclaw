//! `zeroclaw plugin` — list, search, install, remove, inspect, and migrate WASM plugins.

#[cfg(feature = "plugins-wasm")]
use crate::PluginCommands;
#[cfg(feature = "plugins-wasm")]
use crate::config::Config;
#[cfg(feature = "plugins-wasm")]
use crate::gateway_helpers::{t, ta};
#[cfg(feature = "plugins-wasm")]
use crate::plugin_registry;
#[cfg(feature = "plugins-wasm")]
use anyhow::{Result, bail};

#[cfg(feature = "plugins-wasm")]
fn plugin_host_with_configured_security(
    config: &Config,
) -> Result<zeroclaw::plugins::host::PluginHost> {
    let mode = zeroclaw::plugins::host::PluginHost::resolve_signature_mode(
        &config.plugins.security.signature_mode,
    );
    let trusted = config.plugins.security.trusted_publisher_keys.clone();
    Ok(
        zeroclaw::plugins::host::PluginHost::from_plugins_dir_with_security(
            &config.plugins.resolved_plugins_dir(),
            mode,
            trusted,
        )?,
    )
}

#[cfg(feature = "plugins-wasm")]
async fn seed_plugin_config_entry(config: &mut Config, plugin_name: &str) -> Result<()> {
    let whole_config_degraded = config
        .degraded_security
        .iter()
        .any(|s| s == crate::config::migration::WHOLE_CONFIG_SENTINEL);
    if whole_config_degraded || config.degraded_sections.iter().any(|s| s == "plugins") {
        eprintln!(
            "{}",
            ta(
                "cli-plugin-config-entry-seed-skipped",
                &[("name", plugin_name)],
                "warning: skipped seeding the plugin config entry: the \
                 [plugins] section on disk is malformed. Repair it, add \
                 `[[plugins.entries]]` with the plugin name, then set values \
                 with `zeroclaw config set plugins.entries.<name>.config.<key>`."
            )
        );
        return Ok(());
    }
    if plugin_name.is_empty() || plugin_name.contains('.') {
        eprintln!(
            "{}",
            ta(
                "cli-plugin-config-entry-seed-unaddressable",
                &[("name", plugin_name)],
                "warning: skipped seeding the plugin config entry: the plugin \
                 name cannot be addressed by a dotted config path. Add a \
                 `[[plugins.entries]]` block to the config file by hand."
            )
        );
        return Ok(());
    }
    let created = config
        .create_map_key("plugins.entries", plugin_name)
        .map_err(anyhow::Error::msg)?;
    if !created {
        return Ok(());
    }
    config.mark_dirty(&format!("plugins.entries.{plugin_name}"));
    Box::pin(config.save_dirty()).await?;
    println!(
        "{}",
        ta(
            "cli-plugin-config-entry-seeded",
            &[("name", plugin_name)],
            "Seeded config entry. Set plugin config values with \
             `zeroclaw config set plugins.entries.<name>.config.<key>`."
        )
    );
    Ok(())
}

#[cfg(feature = "plugins-wasm")]
pub async fn handle(plugin_command: crate::PluginCommands, config: &mut Config) -> Result<()> {
    match plugin_command {
        PluginCommands::List => {
            let host = plugin_host_with_configured_security(config)?;
            let plugins = host.list_plugins();
            if plugins.is_empty() {
                println!("{}", t("cli-plugins-none", "No plugins installed."));
            } else {
                println!("{}", t("cli-plugins-installed", "Installed plugins:"));
                for p in &plugins {
                    println!(
                        "  {} v{} — {}",
                        p.name,
                        p.version,
                        p.description.as_deref().unwrap_or("(no description)")
                    );
                }
            }
            let target = config.plugins.resolved_plugins_dir().display().to_string();
            for legacy in crate::config::schema::legacy_plugin_dirs_with_entries(config) {
                eprintln!(
                    "{}",
                    ta(
                        "cli-plugin-legacy-detected",
                        &[("path", &legacy.display().to_string()), ("target", &target)],
                        "Note: plugins in a legacy location are not loaded by the agent — \
                         run `zeroclaw plugin migrate` to move them.",
                    )
                );
            }
            Ok(())
        }
        PluginCommands::Search { query, registry } => {
            let registry_url = plugin_registry::registry_url(registry.as_deref());
            let index = plugin_registry::fetch_registry_index(&registry_url).await?;
            zeroclaw::plugins::registry::write_cached_registry_index(
                &config.data_dir,
                &registry_url,
                &index,
            )?;
            let matches = plugin_registry::search_entries(&index, &query);
            if matches.is_empty() {
                println!(
                    "{}",
                    ta(
                        "cli-plugin-search-none",
                        &[("query", &query)],
                        "No matching plugins."
                    )
                );
            } else {
                println!(
                    "{}",
                    ta(
                        "cli-plugin-search-results",
                        &[("query", &query), ("count", &matches.len().to_string())],
                        "Plugins matching query:"
                    )
                );
                for plugin in &matches {
                    let missing_description;
                    let description = if let Some(description) = plugin.description.as_deref() {
                        description
                    } else {
                        missing_description = t("cli-plugin-no-description", "(no description)");
                        &missing_description
                    };
                    println!(
                        "{}",
                        ta(
                            "cli-plugin-search-result",
                            &[
                                ("name", &plugin.name),
                                ("version", &plugin.version),
                                ("description", description),
                            ],
                            "Plugin search result"
                        )
                    );
                }
            }
            Ok(())
        }
        PluginCommands::Install { source, registry } => {
            if plugin_registry::looks_like_url(&source) {
                bail!(
                    "`zeroclaw plugin install <url>` is not supported; use `--registry <url>` with a plugin name, or install a local plugin path"
                );
            }
            let mut host = plugin_host_with_configured_security(config)?;
            if plugin_registry::is_local_plugin_source(&source) {
                let name = host.install(&source)?;
                println!(
                    "{}",
                    ta(
                        "cli-plugin-installed-from",
                        &[("source", &source)],
                        "Plugin installed"
                    )
                );
                Box::pin(seed_plugin_config_entry(config, &name)).await?;
            } else {
                let registry_url = plugin_registry::registry_url(registry.as_deref());
                println!(
                    "{}",
                    ta(
                        "cli-plugin-install-resolving",
                        &[("source", &source)],
                        "Resolving plugin from registry..."
                    )
                );
                let downloaded = plugin_registry::download_registry_plugin(
                    &registry_url,
                    &source,
                    Some(&config.data_dir),
                )
                .await?;
                let plugin_dir = downloaded.plugin_dir().display().to_string();
                let name = host.install(&plugin_dir)?;
                println!(
                    "{}",
                    ta(
                        "cli-plugin-installed-name-version",
                        &[
                            ("name", &downloaded.manifest().name),
                            ("version", &downloaded.manifest().version),
                        ],
                        "Plugin installed"
                    )
                );
                Box::pin(seed_plugin_config_entry(config, &name)).await?;
            }
            Ok(())
        }
        PluginCommands::Remove { name } => {
            let mut host = plugin_host_with_configured_security(config)?;
            host.remove(&name)?;
            println!(
                "{}",
                ta("cli-plugin-removed", &[("name", &name)], "Plugin removed")
            );
            Ok(())
        }
        PluginCommands::Info { name } => {
            let host = plugin_host_with_configured_security(config)?;
            match host.get_plugin(&name) {
                Some(info) => {
                    println!(
                        "{}",
                        ta(
                            "cli-plugin-name-version",
                            &[("name", &info.name), ("version", &info.version)],
                            "Plugin"
                        )
                    );
                    if let Some(desc) = &info.description {
                        println!(
                            "{}",
                            ta("cli-plugin-description", &[("desc", desc)], "Description")
                        );
                    }
                    println!(
                        "{}",
                        ta(
                            "cli-plugin-capabilities",
                            &[("v", &format!("{:?}", info.capabilities))],
                            "Capabilities"
                        )
                    );
                    println!(
                        "{}",
                        ta(
                            "cli-plugin-permissions",
                            &[("v", &format!("{:?}", info.permissions))],
                            "Permissions"
                        )
                    );
                    match &info.wasm_path {
                        Some(path) => println!(
                            "{}",
                            ta(
                                "cli-plugin-wasm",
                                &[("path", &path.display().to_string())],
                                "WASM"
                            )
                        ),
                        None => {
                            println!("{}", t("cli-plugin-wasm-none", "WASM: (skill-only plugin)"));
                        }
                    }
                }
                None => println!(
                    "{}",
                    ta(
                        "cli-plugin-not-found",
                        &[("name", &name)],
                        "Plugin not found"
                    )
                ),
            }
            Ok(())
        }
        PluginCommands::Migrate => {
            let target = config.plugins.resolved_plugins_dir();
            let target_str = target.display().to_string();
            let legacy_dirs = crate::config::schema::legacy_plugin_dirs_with_entries(config);
            let mut total = 0usize;
            for legacy in &legacy_dirs {
                let moved = zeroclaw::plugins::host::migrate_plugins_dir(legacy, &target)?;
                if moved > 0 {
                    println!(
                        "{}",
                        ta(
                            "cli-plugin-migrated",
                            &[
                                ("count", &moved.to_string()),
                                ("path", &legacy.display().to_string()),
                                ("target", &target_str),
                            ],
                            "Migrated plugins from a legacy location.",
                        )
                    );
                }
                total += moved;
            }
            if total == 0 {
                println!("{}", t("cli-plugin-migrate-none", "Nothing to migrate."));
            }
            Ok(())
        }
    }
}

//! `zeroclaw gateway bridge add|remove|list`: operator management of
//! `[gateway.bridges.<name>]`. A bridge token is minted here and printed
//! once; only its SHA-256 hash is written to the config.

use anyhow::{Result, bail};

use super::{t, ta};
use crate::BridgeCommands;
use crate::config::Config;
use zeroclaw_config::pairing::{PairingGuard, generate_bridge_token};
use zeroclaw_config::schema::GatewayBridgeConfig;

pub async fn handle_bridge_command(config: &mut Config, command: BridgeCommands) -> Result<()> {
    match command {
        BridgeCommands::Add {
            name,
            sessions,
            session_prefix,
            rotate,
        } => add(config, &name, sessions, session_prefix, rotate).await,
        BridgeCommands::Remove { name } => remove(config, &name).await,
        BridgeCommands::List => {
            list(config);
            Ok(())
        }
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn scope_text(bridge: &GatewayBridgeConfig) -> String {
    let mut parts: Vec<String> = bridge.sessions.clone();
    if let Some(prefix) = bridge.session_prefix.as_deref().filter(|p| !p.is_empty()) {
        parts.push(format!("{prefix}*"));
    }
    parts.join(", ")
}

async fn add(
    config: &mut Config,
    name: &str,
    sessions: Vec<String>,
    session_prefix: Option<String>,
    rotate: bool,
) -> Result<()> {
    if !valid_name(name) {
        bail!(ta(
            "cli-bridge-invalid-name",
            &[("name", name)],
            "invalid name"
        ));
    }
    let existing = config.gateway.bridges.get(name).cloned();
    if existing.is_some() && !rotate {
        bail!(ta("cli-bridge-exists", &[("name", name)], "bridge exists"));
    }
    if existing.is_none() && rotate {
        bail!(ta(
            "cli-bridge-unknown",
            &[("name", name)],
            "unknown bridge"
        ));
    }
    crate::config::migration::ensure_disk_at_current_version(&config.config_path)?;

    let token = generate_bridge_token();
    let mut bridge = existing.unwrap_or_default();
    bridge.token_hash = PairingGuard::token_hash(&token);
    if !sessions.is_empty() {
        bridge.sessions = sessions;
    }
    if session_prefix.is_some() {
        bridge.session_prefix = session_prefix.filter(|p| !p.is_empty());
    }
    let scope = scope_text(&bridge);
    config.gateway.bridges.insert(name.to_string(), bridge);
    for field in ["token_hash", "sessions", "session_prefix"] {
        config.mark_dirty(&format!("gateway.bridges.{name}.{field}"));
    }
    Box::pin(config.save_dirty()).await?;

    println!(
        "{}",
        ta("cli-bridge-added", &[("name", name)], "bridge added")
    );
    println!();
    println!("{}", t("cli-bridge-token-once", "bridge token"));
    println!("  {token}");
    println!();
    if scope.is_empty() {
        println!("{}", t("cli-bridge-scope-none", "no chat sessions"));
    } else {
        println!(
            "{}",
            ta("cli-bridge-scope", &[("scope", &scope)], "sessions")
        );
    }
    println!("{}", t("cli-bridge-apply", "restart the gateway"));
    Ok(())
}

async fn remove(config: &mut Config, name: &str) -> Result<()> {
    if config.gateway.bridges.remove(name).is_none() {
        bail!(ta(
            "cli-bridge-unknown",
            &[("name", name)],
            "unknown bridge"
        ));
    }
    crate::config::migration::ensure_disk_at_current_version(&config.config_path)?;
    config.mark_dirty(&format!("gateway.bridges.{name}"));
    Box::pin(config.save_dirty()).await?;
    println!(
        "{}",
        ta("cli-bridge-removed", &[("name", name)], "bridge removed")
    );
    println!("{}", t("cli-bridge-apply", "restart the gateway"));
    Ok(())
}

fn list(config: &Config) {
    if config.gateway.bridges.is_empty() {
        println!("{}", t("cli-bridge-list-empty", "no bridges"));
        return;
    }
    let mut names: Vec<&String> = config.gateway.bridges.keys().collect();
    names.sort();
    for name in names {
        let scope = scope_text(&config.gateway.bridges[name]);
        let scope = if scope.is_empty() {
            t("cli-bridge-list-no-sessions", "none")
        } else {
            scope
        };
        println!(
            "{}",
            ta(
                "cli-bridge-list-row",
                &[("name", name), ("scope", &scope)],
                "bridge"
            )
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_names_are_config_keys() {
        assert!(valid_name("telegram"));
        assert!(valid_name("tg-2_b"));
        assert!(!valid_name(""));
        assert!(!valid_name("a.b"));
        assert!(!valid_name("a b"));
    }

    #[tokio::test]
    async fn add_writes_only_the_hash_and_remove_revokes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let mut config = Config {
            config_path: path.clone(),
            data_dir: tmp.path().join("data"),
            ..Config::default()
        };
        Box::pin(config.save()).await.unwrap();

        add(&mut config, "telegram", vec!["main".into()], None, false)
            .await
            .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        let bridge = config.gateway.bridges["telegram"].clone();
        assert!(written.contains("[gateway.bridges.telegram]"), "{written}");
        assert!(written.contains(&bridge.token_hash) || written.contains("enc"));
        assert!(
            !written.contains("zcb_"),
            "the token itself is never stored"
        );
        assert_eq!(bridge.sessions, ["main"]);

        // A second add needs --rotate, which keeps the scope.
        assert!(
            add(&mut config, "telegram", vec![], None, false)
                .await
                .is_err()
        );
        add(&mut config, "telegram", vec![], Some("tg:".into()), true)
            .await
            .unwrap();
        let rotated = &config.gateway.bridges["telegram"];
        assert_ne!(rotated.token_hash, bridge.token_hash);
        assert_eq!(rotated.sessions, ["main"]);
        assert_eq!(rotated.session_prefix.as_deref(), Some("tg:"));

        remove(&mut config, "telegram").await.unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("gateway.bridges.telegram"), "{written}");
        assert!(!written.contains("token_hash"), "{written}");
        assert!(remove(&mut config, "telegram").await.is_err());
    }
}

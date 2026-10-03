//! Configuration preservation across full and slim builds.
//!
//! Known channel settings remain available to full and dirty saves regardless of
//! which channel implementations are compiled. Dirty saves also retain untouched
//! extension sections unknown to the typed schema.

use std::path::PathBuf;
use tempfile::tempdir;
use zeroclaw_config::schema::Config;

const FULL_CONFIG_FIXTURE: &str = r#"schema_version = 3

# Top-level comment
[general]
name = "Production Control"

# Agent configuration
[agents.default]
acp_enable_mcp = false

# Lean channels (included in slim-control)
[channels.wechat.default]
enabled = true
api_base_url = "https://ilinkai.weixin.qq.com"

[channels.wecom.default]
enabled = true
webhook_key = "test-webhook-key"

# Broad channels (not included in slim-control)
# Telegram channel settings
[channels.telegram.default]
enabled = true
bot_token = "123456789:ABCdefGhIJKlmNoPQRsTUVwxyZ"
allowed_users = ["user1", "user2"]

# Discord channel settings
[channels.discord.default]
enabled = true
bot_token = "discord_bot_token_secret"

# Email channel settings
[channels.email.default]
enabled = true
imap_host = "imap.example.com"
smtp_host = "smtp.example.com"
username = "agent@example.com"
password = "email_secret_password"
from_address = "agent@example.com"

# An extension unknown to this binary
[future_extension]
mode = "custom-mode"
"#;

async fn setup_fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempdir().expect("create temp dir");
    let path = dir.path().join("config.toml");
    tokio::fs::write(&path, FULL_CONFIG_FIXTURE)
        .await
        .expect("write fixture");
    (dir, path)
}

#[tokio::test]
async fn cross_build_full_save_preserves_uncompiled_channel_sections() {
    let (_dir, config_path) = setup_fixture().await;

    // Load config from disk
    let raw = tokio::fs::read_to_string(&config_path)
        .await
        .expect("read config");
    let mut config: Config = toml::from_str(&raw).expect("parse config");
    config.config_path = config_path.clone();
    config.loaded_from = Some(config_path.clone());

    // Verify fields from both lean and broad channels are parsed
    assert_eq!(
        config
            .channels
            .wechat
            .get("default")
            .and_then(|c| c.api_base_url.as_deref()),
        Some("https://ilinkai.weixin.qq.com")
    );
    assert_eq!(
        config
            .channels
            .telegram
            .get("default")
            .map(|c| c.bot_token.as_str()),
        Some("123456789:ABCdefGhIJKlmNoPQRsTUVwxyZ")
    );
    assert_eq!(
        config
            .channels
            .discord
            .get("default")
            .map(|c| c.bot_token.as_str()),
        Some("discord_bot_token_secret")
    );

    // Mutate an agent setting
    if let Some(agent) = config.agents.get_mut("default") {
        agent.acp_enable_mcp = true;
    }

    // Full save
    config.save().await.expect("full save succeeds");

    // Re-read file from disk and assert all sections survive
    let saved_raw = tokio::fs::read_to_string(&config_path)
        .await
        .expect("read saved config");

    assert!(
        saved_raw.contains("[channels.wechat.default]"),
        "WeChat section must exist in saved config"
    );
    assert!(
        saved_raw.contains("[channels.telegram.default]"),
        "Telegram section must survive full save"
    );
    assert!(
        saved_raw.contains("bot_token = \"enc2:"),
        "Telegram bot_token must be securely encrypted on disk"
    );
    assert!(
        saved_raw.contains("[channels.discord.default]"),
        "Discord section must survive full save"
    );
    assert!(
        saved_raw.contains("[channels.email.default]"),
        "Email section must survive full save"
    );
    assert!(
        saved_raw.contains("agent@example.com"),
        "Email username must survive full save"
    );

    // Reload and decrypt with the secret store at the same dir
    let mut reloaded: Config = toml::from_str(&saved_raw).expect("reloaded config parses");
    let zeroclaw_dir = config_path.parent().expect("parent dir");
    let store = zeroclaw_config::secrets::SecretStore::new(zeroclaw_dir, true);
    reloaded.decrypt_secrets(&store).expect("decrypt secrets");

    assert_eq!(
        reloaded
            .channels
            .telegram
            .get("default")
            .map(|c| c.bot_token.as_str()),
        Some("123456789:ABCdefGhIJKlmNoPQRsTUVwxyZ"),
        "Telegram token decrypts back to original plaintext"
    );
    assert_eq!(
        reloaded
            .channels
            .discord
            .get("default")
            .map(|c| c.bot_token.as_str()),
        Some("discord_bot_token_secret"),
        "Discord token decrypts back to original plaintext"
    );
    assert_eq!(
        reloaded.agents.get("default").map(|a| a.acp_enable_mcp),
        Some(true)
    );
}

#[tokio::test]
async fn cross_build_dirty_save_preserves_untouched_sections() {
    let (_dir, config_path) = setup_fixture().await;

    let raw = tokio::fs::read_to_string(&config_path)
        .await
        .expect("read config");
    let mut config: Config = toml::from_str(&raw).expect("parse config");
    config.config_path = config_path.clone();
    config.loaded_from = Some(config_path.clone());

    // Modify only a single field and mark it dirty
    if let Some(wechat) = config.channels.wechat.get_mut("default") {
        wechat.api_base_url = Some("https://modified.ilink.api".to_string());
    }
    config.mark_dirty("channels.wechat.default.api_base_url");

    // Save only dirty paths
    config.save_dirty().await.expect("save_dirty succeeds");

    let saved_raw = tokio::fs::read_to_string(&config_path)
        .await
        .expect("read saved config");

    // Verify the modified field changed
    assert!(
        saved_raw.contains("api_base_url = \"https://modified.ilink.api\""),
        "modified api_base_url must be written"
    );

    // Verify untouched broad channels and comments are preserved verbatim
    assert!(
        saved_raw.contains("# Telegram channel settings"),
        "Telegram comment must be preserved"
    );
    assert!(
        saved_raw.contains("[channels.telegram.default]"),
        "Telegram section must remain"
    );
    assert!(
        saved_raw.contains("[channels.discord.default]"),
        "Discord section must remain"
    );
    assert!(
        saved_raw.contains("[channels.email.default]"),
        "Email section must remain"
    );
    assert!(
        saved_raw.contains("[future_extension]"),
        "unknown extension must survive dirty save"
    );
    assert!(saved_raw.contains("mode = \"custom-mode\""));
}

#[tokio::test]
async fn cross_build_dirty_save_secret_preservation() {
    let (_dir, config_path) = setup_fixture().await;

    let raw = tokio::fs::read_to_string(&config_path)
        .await
        .expect("read config");
    let mut config: Config = toml::from_str(&raw).expect("parse config");
    config.config_path = config_path.clone();
    config.loaded_from = Some(config_path.clone());

    // Mark dirty an unrelated path
    if let Some(agent) = config.agents.get_mut("default") {
        agent.acp_enable_mcp = true;
    }
    config.mark_dirty("agents.default.acp_enable_mcp");

    config.save_dirty().await.expect("save_dirty succeeds");

    let saved_raw = tokio::fs::read_to_string(&config_path)
        .await
        .expect("read saved config");

    // Secret fields must not be wiped
    assert!(
        saved_raw.contains("123456789:ABCdefGhIJKlmNoPQRsTUVwxyZ"),
        "telegram token must not be erased"
    );
    assert!(
        saved_raw.contains("discord_bot_token_secret"),
        "discord token must not be erased"
    );
}

#[tokio::test]
async fn cross_build_parent_table_edit_preserves_sibling_entries() {
    let (_dir, config_path) = setup_fixture().await;

    let raw = tokio::fs::read_to_string(&config_path)
        .await
        .expect("read config");
    let mut config: Config = toml::from_str(&raw).expect("parse config");
    config.config_path = config_path.clone();
    config.loaded_from = Some(config_path.clone());

    // Add a second wechat alias via create_map_key
    let created = config
        .create_map_key("channels.wechat", "secondary")
        .expect("create wechat secondary alias");
    assert!(created);

    if let Some(sec) = config.channels.wechat.get_mut("secondary") {
        sec.api_base_url = Some("https://secondary.ilink.api".to_string());
    }
    config.mark_dirty("channels.wechat.secondary.api_base_url");

    config.save_dirty().await.expect("save_dirty succeeds");

    let saved_raw = tokio::fs::read_to_string(&config_path)
        .await
        .expect("read saved config");

    // Both wechat default and secondary must exist, plus all uncompiled channels
    assert!(saved_raw.contains("https://ilinkai.weixin.qq.com"));
    assert!(saved_raw.contains("https://secondary.ilink.api"));
    assert!(saved_raw.contains("[channels.telegram.default]"));
    assert!(saved_raw.contains("[channels.discord.default]"));
}

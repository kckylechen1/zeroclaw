//! `zeroclaw auth` — manage model-provider subscription authentication profiles.

use anyhow::{Context, Result, bail};
use dialoguer::Password;

use crate::AuthCommands;
use crate::auth;
use crate::cli_input;
use crate::config::Config;
use crate::gateway_helpers::{t, ta};

// Interactive CLI input helpers used by `auth paste-token` /
// `auth setup-token` / `auth paste-redirect`. The dialoguer dep belongs
// to the binary; auth/mod.rs in zeroclaw-providers shouldn't pull it in,
// so reads live here and trait flows accept the resulting string.

#[cfg(feature = "agent-runtime")]
fn read_auth_input(prompt: &str) -> Result<String> {
    let input = Password::new()
        .with_prompt(prompt)
        .allow_empty_password(false)
        .interact()?;
    Ok(input.trim().to_string())
}

#[cfg(feature = "agent-runtime")]
fn read_plain_input(prompt: &str) -> Result<String> {
    let input: String = cli_input::Input::new()
        .with_prompt(prompt)
        .interact_text()?;
    Ok(input.trim().to_string())
}

#[cfg(feature = "agent-runtime")]
fn format_expiry(profile: &auth::profiles::AuthProfile) -> String {
    match profile
        .token_set
        .as_ref()
        .and_then(|token_set| token_set.expires_at)
    {
        Some(ts) => {
            let now = chrono::Utc::now();
            if ts <= now {
                format!("expired at {}", ts.to_rfc3339())
            } else {
                let mins = (ts - now).num_minutes();
                format!("expires in {mins}m ({})", ts.to_rfc3339())
            }
        }
        None => "n/a".to_string(),
    }
}

#[cfg(feature = "agent-runtime")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InlineProviderAuth {
    Codex,
    AnthropicSetupToken { alias: String },
}

#[cfg(feature = "agent-runtime")]
pub(crate) fn quickstart_field_value_eq(
    fields: &std::collections::HashMap<String, String>,
    key: &str,
    expected: &str,
) -> bool {
    fields
        .get(key)
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(expected))
}

#[cfg(feature = "agent-runtime")]
pub(crate) fn quickstart_inline_auth(
    kind: &str,
    alias: &str,
    fields: &std::collections::HashMap<String, String>,
) -> Option<InlineProviderAuth> {
    if kind == "openai" && quickstart_field_value_eq(fields, "auth_mode", "codex") {
        return Some(InlineProviderAuth::Codex);
    }
    if kind == "anthropic" && quickstart_field_value_eq(fields, "auth_mode", "setup_token") {
        return Some(InlineProviderAuth::AnthropicSetupToken {
            alias: alias.to_string(),
        });
    }
    None
}

/// `~/.codex/auth.json` — the credential file the upstream Codex CLI writes.
/// When present, offer a direct import instead of starting a fresh browser flow.
#[cfg(feature = "agent-runtime")]
fn codex_auth_json_path() -> Option<std::path::PathBuf> {
    directories::UserDirs::new().map(|u| u.home_dir().join(".codex").join("auth.json"))
}

#[cfg(feature = "agent-runtime")]
pub(crate) async fn run_inline_provider_auth(auth: InlineProviderAuth, config: &mut Config) {
    use dialoguer::Confirm;

    let codex_import = match &auth {
        InlineProviderAuth::Codex => codex_auth_json_path().filter(|path| path.exists()),
        InlineProviderAuth::AnthropicSetupToken { .. } => None,
    };
    let (prompt, skip_hint) = match &auth {
        InlineProviderAuth::Codex => (
            if codex_import.is_some() {
                t(
                    "cli-quickstart-auth-codex-import-prompt",
                    "Found an existing Codex login (~/.codex/auth.json) — import it now?",
                )
            } else {
                t(
                    "cli-quickstart-auth-codex-prompt",
                    "Sign in to OpenAI Codex with your ChatGPT account now?",
                )
            },
            t(
                "cli-quickstart-auth-codex-skip-hint",
                "  Finish later with: zeroclaw auth login --model-provider openai-codex",
            ),
        ),
        InlineProviderAuth::AnthropicSetupToken { alias } => (
            ta(
                "cli-quickstart-auth-anthropic-prompt",
                &[("alias", alias)],
                "Run `claude setup-token` for this Anthropic provider now?",
            ),
            ta(
                "cli-quickstart-auth-anthropic-skip-hint",
                &[("alias", alias)],
                "  Finish later with: claude setup-token",
            ),
        ),
    };
    if !Confirm::new()
        .with_prompt(prompt)
        .default(true)
        .interact()
        .unwrap_or(false)
    {
        println!("{skip_hint}");
        return;
    }

    let result = match auth {
        InlineProviderAuth::Codex => {
            let cmd = AuthCommands::Login {
                model_provider: "openai-codex".to_string(),
                profile: "default".to_string(),
                device_code: false,
                import: codex_import,
            };
            handle_auth_command(cmd, config).await
        }
        InlineProviderAuth::AnthropicSetupToken { alias } => {
            Box::pin(run_anthropic_setup_token_inline(&alias, config)).await
        }
    };
    if let Err(error) = result {
        let error = error.to_string();
        eprintln!(
            "{}",
            ta(
                "cli-quickstart-auth-failed",
                &[("error", &error)],
                "  Auth setup didn't complete.",
            )
        );
        println!("{skip_hint}");
    }
}

#[cfg(feature = "agent-runtime")]
async fn run_anthropic_setup_token_inline(alias: &str, config: &mut Config) -> Result<()> {
    let status = tokio::process::Command::new("claude")
        .arg("setup-token")
        .status()
        .await
        .context("failed to run `claude setup-token`; is the Claude CLI installed and on PATH?")?;
    if !status.success() {
        bail!("`claude setup-token` exited with status {status}");
    }

    let token = read_auth_input(&t(
        "cli-quickstart-auth-anthropic-token-prompt",
        "Paste the token from `claude setup-token`",
    ))?;
    if token.trim().is_empty() {
        bail!("Token cannot be empty");
    }

    let path = format!("providers.models.anthropic.{alias}.api_key");
    config.set_prop_persistent(&path, token.trim())?;
    Box::pin(config.save_dirty()).await?;
    println!(
        "{}",
        ta(
            "cli-quickstart-auth-anthropic-saved",
            &[("alias", alias)],
            "  Saved Claude setup token.",
        )
    );
    Ok(())
}

#[allow(clippy::too_many_lines)]
#[cfg(feature = "agent-runtime")]
pub async fn handle_auth_command(auth_command: AuthCommands, config: &Config) -> Result<()> {
    let auth_service = auth::AuthService::from_config(config);
    let auth_cli_formatter =
        |key: &str, args: &[(&str, &str)], fallback: &str| ta(key, args, fallback);

    match auth_command {
        AuthCommands::Login {
            model_provider,
            profile,
            device_code,
            import,
        } => {
            let provider: auth::AuthProvider = model_provider.parse()?;
            let client = reqwest::Client::new();
            let ctx = auth::AuthFlowContext {
                config,
                auth_service: &auth_service,
                client: &client,
                format_cli: &auth_cli_formatter,
            };
            provider
                .flow()
                .login(&ctx, &profile, device_code, import.as_deref())
                .await
        }

        AuthCommands::PasteRedirect {
            model_provider,
            profile,
            input,
        } => {
            let provider: auth::AuthProvider = model_provider.parse()?;
            let client = reqwest::Client::new();
            let ctx = auth::AuthFlowContext {
                config,
                auth_service: &auth_service,
                client: &client,
                format_cli: &auth_cli_formatter,
            };
            let input_str: Option<String> = match input {
                Some(value) => Some(value),
                None => Some(read_plain_input("Paste redirect URL or OAuth code")?),
            };
            provider
                .flow()
                .paste_redirect(&ctx, &profile, input_str.as_deref())
                .await
        }

        AuthCommands::PasteToken {
            model_provider,
            profile,
            token,
            auth_kind,
        } => {
            let model_provider = auth::normalize_model_provider(&model_provider)?;
            let token = match token {
                Some(token) => token.trim().to_string(),
                None => read_auth_input("Paste token")?,
            };
            if token.is_empty() {
                bail!("Token cannot be empty");
            }

            let kind = auth::anthropic_token::detect_auth_kind(&token, auth_kind.as_deref());
            let mut metadata = std::collections::HashMap::new();
            metadata.insert(
                "auth_kind".to_string(),
                kind.as_metadata_value().to_string(),
            );

            auth_service
                .store_model_provider_token(&model_provider, &profile, &token, metadata, true)
                .await?;
            println!(
                "{}",
                ta("cli-auth-saved", &[("profile", &profile)], "Saved profile")
            );
            println!(
                "{}",
                ta(
                    "cli-auth-active-for",
                    &[("provider", &model_provider), ("profile", &profile)],
                    "Active profile"
                )
            );
            Ok(())
        }

        AuthCommands::SetupToken {
            model_provider,
            profile,
        } => {
            let model_provider = auth::normalize_model_provider(&model_provider)?;
            let token = read_auth_input("Paste token")?;
            if token.is_empty() {
                bail!("Token cannot be empty");
            }

            let kind = auth::anthropic_token::detect_auth_kind(&token, Some("authorization"));
            let mut metadata = std::collections::HashMap::new();
            metadata.insert(
                "auth_kind".to_string(),
                kind.as_metadata_value().to_string(),
            );

            auth_service
                .store_model_provider_token(&model_provider, &profile, &token, metadata, true)
                .await?;
            println!(
                "{}",
                ta("cli-auth-saved", &[("profile", &profile)], "Saved profile")
            );
            println!(
                "{}",
                ta(
                    "cli-auth-active-for",
                    &[("provider", &model_provider), ("profile", &profile)],
                    "Active profile"
                )
            );
            Ok(())
        }

        AuthCommands::Refresh {
            model_provider,
            profile,
        } => {
            let provider: auth::AuthProvider = model_provider.parse()?;
            let client = reqwest::Client::new();
            let ctx = auth::AuthFlowContext {
                config,
                auth_service: &auth_service,
                client: &client,
                format_cli: &auth_cli_formatter,
            };
            let status = provider
                .flow()
                .refresh_status(&ctx, profile.as_deref())
                .await?;
            match status {
                auth::RefreshStatus::Refreshed { profile } => {
                    println!(
                        "{}",
                        ta(
                            "cli-auth-refresh-ok",
                            &[("profile", &profile)],
                            "Token refresh OK"
                        )
                    );
                    Ok(())
                }
                auth::RefreshStatus::NoProfile => {
                    bail!(
                        "No auth profile found. Run `zeroclaw auth login --model-provider <provider>` first.",
                    )
                }
            }
        }

        AuthCommands::Logout {
            model_provider,
            profile,
        } => {
            let model_provider = auth::normalize_model_provider(&model_provider)?;
            let removed = auth_service
                .remove_profile(&model_provider, &profile)
                .await?;
            if removed {
                println!(
                    "{}",
                    ta(
                        "cli-auth-removed",
                        &[("provider", &model_provider), ("profile", &profile)],
                        "Removed auth profile"
                    )
                );
            } else {
                println!(
                    "{}",
                    ta(
                        "cli-auth-not-found",
                        &[("provider", &model_provider), ("profile", &profile)],
                        "Auth profile not found"
                    )
                );
            }
            Ok(())
        }

        AuthCommands::Use {
            model_provider,
            profile,
        } => {
            let model_provider = auth::normalize_model_provider(&model_provider)?;
            auth_service
                .set_active_profile(&model_provider, &profile)
                .await?;
            println!(
                "{}",
                ta(
                    "cli-auth-active-for",
                    &[("provider", &model_provider), ("profile", &profile)],
                    "Active profile"
                )
            );
            Ok(())
        }

        AuthCommands::List => {
            let data = auth_service.load_profiles().await?;
            if data.profiles.is_empty() {
                println!("{}", t("cli-auth-none", "No auth profiles configured."));
                return Ok(());
            }

            for (id, profile) in &data.profiles {
                let active = data
                    .active_profiles
                    .get(&profile.model_provider)
                    .is_some_and(|active_id| active_id == id);
                let marker = if active { "*" } else { " " };
                println!("{marker} {id}");
            }

            Ok(())
        }

        AuthCommands::Status => {
            let data = auth_service.load_profiles().await?;
            if data.profiles.is_empty() {
                println!("{}", t("cli-auth-none", "No auth profiles configured."));
                return Ok(());
            }

            for (id, profile) in &data.profiles {
                let active = data
                    .active_profiles
                    .get(&profile.model_provider)
                    .is_some_and(|active_id| active_id == id);
                let marker = if active { "*" } else { " " };
                println!(
                    "{} {} kind={:?} account={} expires={}",
                    marker,
                    id,
                    profile.kind,
                    crate::security::redact(profile.account_id.as_deref().unwrap_or("unknown")),
                    format_expiry(profile)
                );
            }

            println!();
            println!("{}", t("cli-auth-active", "Active profiles:"));
            for (model_provider, profile_id) in &data.active_profiles {
                println!("  {model_provider}: {profile_id}");
            }

            Ok(())
        }

        AuthCommands::EmailLogin { channel, profile } => {
            let email_cfg = config.channels.email.get(&channel).ok_or_else(|| {
                anyhow::Error::msg(format!(
                    "No [channels.email.{channel}] block found in config. \
                     Add the block with an [channels.email.{channel}.oauth2] section first."
                ))
            })?;

            let oauth2 = email_cfg.oauth2.as_ref().ok_or_else(|| anyhow::Error::msg(format!(
                "[channels.email.{channel}] exists but has no [channels.email.{channel}.oauth2] block."
            )))?;

            let client = reqwest::Client::new();
            let device = auth::email_oauth2::start_device_code_flow(
                &client,
                &oauth2.device_code_url,
                &oauth2.client_id,
                &oauth2.scopes,
            )
            .await?;

            println!("Email OAuth2 device-code login started."); // i18n-exempt: interactive device-code CLI prompt
            println!("Visit:  {}", device.verification_uri); // i18n-exempt: interactive device-code CLI prompt
            println!("Code:   {}", device.user_code); // i18n-exempt: interactive device-code CLI prompt
            if let Some(ref uri) = device.verification_uri_complete {
                println!("Or open directly: {uri}"); // i18n-exempt: interactive device-code CLI prompt
            }
            println!("Waiting for authorization…"); // i18n-exempt: interactive device-code CLI prompt

            let token_set = auth::email_oauth2::poll_device_code_tokens(
                &client,
                &oauth2.token_url,
                &oauth2.client_id,
                &device,
            )
            .await?;

            let channel_alias = format!("email.{channel}");
            auth_service
                .store_email_oauth2_tokens(&channel_alias, &profile, token_set)
                .await?;
            println!("Saved profile {profile} for {channel_alias}"); // i18n-exempt: interactive device-code CLI prompt
            Ok(())
        }
    }
}

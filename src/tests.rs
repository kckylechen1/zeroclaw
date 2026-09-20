use super::*;
use clap::{CommandFactory, Parser};
use std::net::TcpListener;

#[test]
fn probe_config_dir_extracts_global_flag_in_all_forms() {
    fn argv(parts: &[&str]) -> std::vec::IntoIter<std::ffi::OsString> {
        parts
            .iter()
            .map(|s| std::ffi::OsString::from(*s))
            .collect::<Vec<_>>()
            .into_iter()
    }

    let command = Cli::command();

    // argv[0] is consumed by clap as the binary name.
    // Space form.
    assert_eq!(
        probe_config_dir(&command, argv(&["zeroclaw", "--config-dir", "/x"])),
        Some("/x".to_string())
    );
    // Equals form.
    assert_eq!(
        probe_config_dir(&command, argv(&["zeroclaw", "--config-dir=/y"])),
        Some("/y".to_string())
    );
    // Global arg: may appear *after* a subcommand.
    assert_eq!(
        probe_config_dir(
            &command,
            argv(&["zeroclaw", "status", "--config-dir", "/z"])
        ),
        Some("/z".to_string())
    );
    // Absent.
    assert_eq!(
        probe_config_dir(&command, argv(&["zeroclaw", "status"])),
        None
    );
    // `--` ends option parsing; later values must never redirect config.
    assert_eq!(
        probe_config_dir(
            &command,
            argv(&[
                "zeroclaw",
                "config",
                "set",
                "locale",
                "--",
                "--config-dir=/ignored",
            ])
        ),
        None
    );
    // Present but empty — returned verbatim for clap's validation path.
    assert_eq!(
        probe_config_dir(&command, argv(&["zeroclaw", "--config-dir", ""])),
        Some(String::new())
    );
}

#[test]
fn probe_config_dir_follows_clap_token_ownership() {
    fn argv(parts: &[&str]) -> std::vec::IntoIter<std::ffi::OsString> {
        parts
            .iter()
            .map(|s| std::ffi::OsString::from(*s))
            .collect::<Vec<_>>()
            .into_iter()
    }

    let command = Cli::command();
    let external_payload = [
        "zeroclaw",
        "props",
        "legacy-command",
        "--config-dir=/unintended",
    ];

    // The external subcommand owns every remaining token, including one
    // that looks like a global option.
    let cli = Cli::try_parse_from(external_payload)
        .expect("the deprecated external-subcommand path is valid clap input");
    assert!(cli.config_dir.is_none());
    assert_eq!(probe_config_dir(&command, argv(&external_payload)), None);

    // Option-looking and terminating tokens cannot satisfy the spaced
    // form's required value.
    assert!(Cli::try_parse_from(["zeroclaw", "--config-dir", "--help"]).is_err());
    assert_eq!(
        probe_config_dir(&command, argv(&["zeroclaw", "--config-dir", "--help"])),
        None
    );
    assert_eq!(
        probe_config_dir(&command, argv(&["zeroclaw", "--config-dir", "--"])),
        None
    );
}

#[test]
fn cli_quickstart_uses_advertised_local_provider_runtime_default() {
    let providers = vec![zeroclaw_runtime::quickstart::QuickstartTypeOption {
        kind: "lmstudio".into(),
        display_name: "LM Studio".into(),
        local: true,
        default_runtime_profile: Some("local_small".into()),
    }];

    assert_eq!(
        commands::quickstart::quickstart_runtime_profile_for_provider(
            "lmstudio",
            &providers,
            "unbounded"
        ),
        "local_small"
    );
}

#[test]
fn cli_quickstart_uses_advertised_remote_provider_runtime_default() {
    let providers = vec![zeroclaw_runtime::quickstart::QuickstartTypeOption {
        kind: "anthropic".into(),
        display_name: "Anthropic".into(),
        local: false,
        default_runtime_profile: Some("unbounded".into()),
    }];

    assert_eq!(
        commands::quickstart::quickstart_runtime_profile_for_provider(
            "anthropic",
            &providers,
            "unbounded"
        ),
        "unbounded"
    );
}

#[test]
fn cli_quickstart_uses_state_fallback_when_provider_has_no_override() {
    let providers = vec![zeroclaw_runtime::quickstart::QuickstartTypeOption {
        kind: "ollama".into(),
        display_name: "Ollama".into(),
        local: true,
        default_runtime_profile: None,
    }];

    assert_eq!(
        commands::quickstart::quickstart_runtime_profile_for_provider(
            "ollama",
            &providers,
            "unbounded"
        ),
        "unbounded"
    );
}

#[test]
fn cap_line_utf8_safe_no_panic_on_multibyte_boundary() {
    // Neutral multi-byte placeholder text; each CJK char is 3 bytes, so a
    // byte cap can land inside a character. Pre-fix this panicked via the
    // raw `String::truncate(cap)`.
    let mut line = "语言".repeat(64); // 128 chars, 384 bytes, all 3-byte
    let cap = 10; // byte index 10 is mid-character (10 % 3 != 0)
    assert!(
        !line.is_char_boundary(cap),
        "precondition: cap splits a char"
    );
    cap_line_utf8_safe(&mut line, cap);
    assert!(line.len() <= cap, "must not exceed the byte cap");
    assert!(
        line.is_char_boundary(line.len()),
        "result must end on a valid UTF-8 char boundary"
    );
    // cap 10 floors to byte 9 = three whole 3-byte chars.
    assert_eq!(
        line, "语言语",
        "should keep whole chars up to the floored cap"
    );
}

#[test]
fn cap_line_utf8_safe_is_noop_when_within_cap() {
    let mut line = String::from("héllo"); // 6 bytes
    cap_line_utf8_safe(&mut line, 1024);
    assert_eq!(line, "héllo");
}

#[test]
fn cap_line_utf8_safe_ascii_exact_cap() {
    let mut line = String::from("abcdefgh");
    cap_line_utf8_safe(&mut line, 4);
    assert_eq!(line, "abcd");
}

#[test]
#[cfg(feature = "agent-runtime")]
fn cli_definition_has_no_flag_conflicts() {
    Cli::command().debug_assert();
}

#[test]
#[cfg(feature = "agent-runtime")]
fn quickstart_inline_auth_uses_auth_mode_field() {
    let fields =
        std::collections::HashMap::from([("auth_mode".to_string(), " codex ".to_string())]);
    assert_eq!(
        commands::auth::quickstart_inline_auth("openai", "codex", &fields),
        Some(commands::auth::InlineProviderAuth::Codex)
    );

    let fields =
        std::collections::HashMap::from([("auth_mode".to_string(), "setup_token".to_string())]);
    assert_eq!(
        commands::auth::quickstart_inline_auth("anthropic", "max", &fields),
        Some(commands::auth::InlineProviderAuth::AnthropicSetupToken {
            alias: "max".to_string()
        })
    );

    assert_eq!(
        commands::auth::quickstart_inline_auth("openai", "api", &fields),
        None
    );
}

#[test]
#[cfg(feature = "agent-runtime")]
fn ensure_map_key_materializes_typed_provider_entries() {
    use crate::config::schema::Config;
    for (path, value) in [
        ("providers.models.openai.default.model", "gpt-4o"),
        ("providers.tts.openai.default.voice", "alloy"),
        ("providers.transcription.openai.default.model", "whisper-1"),
        ("channels.telegram.default.bot_token", "tok"),
    ] {
        let mut config = Config::default();
        assert!(
            config.set_prop(path, value).is_err(),
            "precondition: {path} should be unknown on a fresh config"
        );
        config.ensure_map_key_for_path(path);
        assert!(
            config.set_prop(path, value).is_ok(),
            "{path} must be settable after map-key materialization"
        );
    }
}

#[test]
#[cfg(feature = "agent-runtime")]
fn ensure_map_key_ignores_non_map_paths() {
    use crate::config::schema::Config;
    let mut config = Config::default();
    config.ensure_map_key_for_path("gateway.port");
    config.ensure_map_key_for_path("locale");
}

#[test]
#[cfg(feature = "agent-runtime")]
fn onboard_help_includes_model_flag() {
    let cmd = Cli::command();
    let onboard = cmd
        .get_subcommands()
        .find(|subcommand| subcommand.get_name() == "onboard")
        .expect("onboard subcommand must exist");

    let has_model_flag = onboard
        .get_arguments()
        .any(|arg| arg.get_id().as_str() == "model" && arg.get_long() == Some("model"));

    assert!(
        has_model_flag,
        "onboard help should include --model for quick setup overrides"
    );
}

#[test]
#[cfg(feature = "agent-runtime")]
fn gateway_admin_url_uses_unprefixed_admin_path_by_default() {
    assert_eq!(
        gateway_admin_url("127.0.0.1", 42617, None, "/admin/paircode"),
        "http://127.0.0.1:42617/admin/paircode"
    );
}

#[test]
#[cfg(feature = "agent-runtime")]
fn gateway_admin_url_prepends_configured_path_prefix() {
    assert_eq!(
        gateway_admin_url("localhost", 42617, Some("/zeroclaw"), "/admin/paircode/new"),
        "http://localhost:42617/zeroclaw/admin/paircode/new"
    );
}

#[test]
#[cfg(feature = "agent-runtime")]
fn onboard_cli_accepts_model_provider_and_api_key_in_quick_mode() {
    let cli = Cli::try_parse_from([
        "zeroclaw",
        "onboard",
        "--model-provider",
        "openrouter",
        "--model",
        "custom-model-946",
        "--api-key",
        "sk-issue946",
    ])
    .expect("quick onboard invocation should parse");

    match cli.command {
        Commands::Onboard {
            force,
            channels_only,
            api_key,
            model_provider,
            model,
            ..
        } => {
            assert!(!force);
            assert!(!channels_only);
            assert_eq!(model_provider.as_deref(), Some("openrouter"));
            assert_eq!(model.as_deref(), Some("custom-model-946"));
            assert_eq!(api_key.as_deref(), Some("sk-issue946"));
        }
        other => panic!("expected onboard command, got {other:?}"),
    }
}

#[test]
#[cfg(feature = "agent-runtime")]
fn completions_cli_parses_supported_shells() {
    for shell in ["bash", "fish", "zsh", "powershell", "elvish"] {
        let cli = Cli::try_parse_from(["zeroclaw", "completions", shell])
            .expect("completions invocation should parse");
        match cli.command {
            Commands::Completions { .. } => {}
            other => panic!("expected completions command, got {other:?}"),
        }
    }
}

#[test]
#[cfg(feature = "agent-runtime")]
fn completion_generation_mentions_binary_name() {
    let mut output = Vec::new();
    write_shell_completion(CompletionShell::Bash, &mut output)
        .expect("completion generation should succeed");
    let script = String::from_utf8(output).expect("completion output should be valid utf-8");
    assert!(
        script.contains("zeroclaw"),
        "completion script should reference binary name"
    );
}

#[test]
#[cfg(feature = "agent-runtime")]
fn bash_completion_avoids_infinite_recursion() {
    let mut output = Vec::new();
    write_shell_completion(CompletionShell::Bash, &mut output)
        .expect("completion generation should succeed");
    let script = String::from_utf8(output).expect("completion output should be valid utf-8");
    // The wrapper must capture the original clap-generated function body
    // (via declare -f) rather than calling _zeroclaw by name, which would
    // create an infinite recursion loop after _zeroclaw is redefined.
    assert!(
        script.contains("declare -f _zeroclaw"),
        "bash completion should use declare -f to capture the original _zeroclaw function body"
    );
    assert!(
        !script.contains("_zeroclaw_clap_orig() { _zeroclaw \"$@\"; }"),
        "bash completion must not define _zeroclaw_clap_orig as a simple forwarder to _zeroclaw"
    );
}

#[test]
#[cfg(feature = "agent-runtime")]
fn onboard_cli_accepts_force_flag() {
    let cli = Cli::try_parse_from(["zeroclaw", "onboard", "--force"])
        .expect("onboard --force should parse");

    match cli.command {
        Commands::Onboard { force, .. } => assert!(force),
        other => panic!("expected onboard command, got {other:?}"),
    }
}

#[test]
#[cfg(feature = "agent-runtime")]
fn onboard_cli_rejects_removed_interactive_flag() {
    // --interactive was removed; onboard auto-detects TTY instead.
    assert!(Cli::try_parse_from(["zeroclaw", "onboard", "--interactive"]).is_err());
}

#[test]
#[cfg(feature = "agent-runtime")]
fn onboard_cli_parses_quick_flag() {
    let cli = Cli::try_parse_from(["zeroclaw", "onboard", "--quick"])
        .expect("onboard --quick should parse");

    match cli.command {
        Commands::Onboard { quick, .. } => assert!(quick),
        other => panic!("expected onboard command, got {other:?}"),
    }
}

#[test]
#[cfg(feature = "agent-runtime")]
fn gateway_get_paircode_cli_accepts_port_and_host_overrides() {
    let cli = Cli::try_parse_from([
        "zeroclaw",
        "gateway",
        "get-paircode",
        "--new",
        "--port",
        "3001",
        "--host",
        "192.168.1.20",
    ])
    .expect("gateway get-paircode overrides should parse");

    match cli.command {
        Commands::Gateway {
            gateway_command:
                Some(zeroclaw::GatewayCommands::GetPaircode {
                    new,
                    rotate,
                    rotate_device,
                    port,
                    host,
                }),
        } => {
            assert!(new);
            assert!(!rotate);
            assert_eq!(rotate_device, None);
            assert_eq!(port, Some(3001));
            assert_eq!(host.as_deref(), Some("192.168.1.20"));
        }
        other => panic!("expected gateway get-paircode command, got {other:?}"),
    }
}

#[test]
#[cfg(feature = "agent-runtime")]
fn security_status_cli_requires_agent_and_parses_json_form() {
    let err = Cli::try_parse_from(["zeroclaw", "security", "status"])
        .expect_err("security status requires --agent");
    assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);

    let cli = Cli::try_parse_from(["zeroclaw", "security", "status", "--agent", "ops", "--json"])
        .expect("security status --agent --json should parse");
    match cli.command {
        Commands::Security {
            security_command: SecurityCommands::Status { agent, json },
        } => {
            assert_eq!(agent, "ops");
            assert!(json);
        }
        other => panic!("expected security status command, got {other:?}"),
    }
}

#[test]
#[cfg(feature = "agent-runtime")]
fn gateway_get_paircode_rotate_flags_parse_and_conflict() {
    let cli = Cli::try_parse_from(["zeroclaw", "gateway", "get-paircode", "--rotate"])
        .expect("gateway get-paircode --rotate should parse");
    match cli.command {
        Commands::Gateway {
            gateway_command: Some(zeroclaw::GatewayCommands::GetPaircode { rotate, .. }),
        } => assert!(rotate),
        other => panic!("expected gateway get-paircode command, got {other:?}"),
    }

    let cli = Cli::try_parse_from([
        "zeroclaw",
        "gateway",
        "get-paircode",
        "--rotate-device",
        "dash-1",
    ])
    .expect("gateway get-paircode --rotate-device should parse");
    match cli.command {
        Commands::Gateway {
            gateway_command: Some(zeroclaw::GatewayCommands::GetPaircode { rotate_device, .. }),
        } => assert_eq!(rotate_device.as_deref(), Some("dash-1")),
        other => panic!("expected gateway get-paircode command, got {other:?}"),
    }

    assert!(
        Cli::try_parse_from(["zeroclaw", "gateway", "get-paircode", "--new", "--rotate"]).is_err(),
        "--new and --rotate must conflict"
    );
    assert!(
        Cli::try_parse_from([
            "zeroclaw",
            "gateway",
            "get-paircode",
            "--rotate",
            "--rotate-device",
            "dash-1"
        ])
        .is_err(),
        "--rotate and --rotate-device must conflict"
    );
}

#[test]
#[cfg(feature = "agent-runtime")]
fn paircode_url_combines_host_port_override_with_configured_path_prefix() {
    assert_eq!(
        gateway_admin_url(
            "127.0.0.1",
            9001,
            Some("/agents/myagent"),
            "/admin/paircode/new"
        ),
        "http://127.0.0.1:9001/agents/myagent/admin/paircode/new",
    );
    assert_eq!(
        gateway_admin_url("192.168.1.20", 42617, Some("/gw"), "/admin/paircode"),
        "http://192.168.1.20:42617/gw/admin/paircode",
    );
}

#[test]
#[cfg(feature = "agent-runtime")]
fn paircode_no_code_message_explains_bare_command_does_not_mint() {
    let default = config::GatewayConfig::default();
    let msg = paircode_no_code_message(
        "127.0.0.1",
        42617,
        &default.host,
        default.port,
        &PaircodeAction::Show,
        true,
        Some("Pairing is active but no new code available (already paired or code expired)"),
    );

    assert!(msg.contains("only displays an existing active code; it does not mint"));
    assert!(msg.contains("zeroclaw gateway get-paircode --new"));
    assert!(msg.contains("zeroclaw gateway get-paircode --rotate"));
    assert!(msg.contains("open http://127.0.0.1:42617"));
}

#[test]
#[cfg(feature = "agent-runtime")]
fn paircode_no_code_message_preserves_host_port_on_suggestions() {
    let default = config::GatewayConfig::default();
    let msg = paircode_no_code_message(
        "192.168.1.20",
        9001,
        &default.host,
        default.port,
        &PaircodeAction::Show,
        true,
        None,
    );

    assert!(msg.contains("zeroclaw gateway get-paircode --new --port 9001 --host 192.168.1.20"));
    assert!(msg.contains("zeroclaw gateway get-paircode --rotate --port 9001 --host 192.168.1.20"));
    assert!(msg.contains("open http://192.168.1.20:9001"));
}

#[test]
#[cfg(feature = "agent-runtime")]
fn paircode_no_code_message_omits_configured_default_host_port() {
    let msg = paircode_no_code_message(
        "192.168.1.20",
        9001,
        "192.168.1.20",
        9001,
        &PaircodeAction::Show,
        true,
        None,
    );

    assert!(msg.contains("zeroclaw gateway get-paircode --new\n"));
    assert!(msg.contains("zeroclaw gateway get-paircode --rotate\n"));
    assert!(!msg.contains("--port 9001"));
    assert!(!msg.contains("--host 192.168.1.20"));
}

#[test]
#[cfg(feature = "agent-runtime")]
fn paircode_no_code_message_for_new_suggests_rotate() {
    let default = config::GatewayConfig::default();
    let msg = paircode_no_code_message(
        "127.0.0.1",
        42617,
        &default.host,
        default.port,
        &PaircodeAction::AddClient,
        true,
        Some("Pairing is active but no new code available (already paired or code expired)"),
    );

    assert!(msg.contains("did not mint a new pairing code"));
    assert!(msg.contains("zeroclaw gateway get-paircode --rotate"));
}

#[test]
fn gateway_addr_in_use_message_guides_default_gateway_recovery() {
    let default = config::GatewayConfig::default();
    let msg =
        gateway_addr_in_use_message("127.0.0.1", 42617, &default.host, default.port, Some(42618));

    assert!(msg.contains("Port 42617 is already in use"));
    assert!(msg.contains("open http://127.0.0.1:42617"));
    assert!(msg.contains("zeroclaw gateway get-paircode\n"));
    assert!(msg.contains("zeroclaw gateway start --port 42618"));
    assert!(msg.contains("lsof -nP -iTCP:42617 -sTCP:LISTEN"));
}

#[test]
fn gateway_addr_in_use_message_keeps_non_default_host_context() {
    let default = config::GatewayConfig::default();
    let msg = gateway_addr_in_use_message("0.0.0.0", 9001, &default.host, default.port, Some(9002));

    assert!(!msg.contains("open http://127.0.0.1:42617"));
    assert!(msg.contains("zeroclaw gateway get-paircode --port 9001 --host 0.0.0.0"));
    assert!(msg.contains("zeroclaw gateway start --port 9002 --host 0.0.0.0"));
    assert!(msg.contains("lsof -nP -iTCP:9001 -sTCP:LISTEN"));
}

#[test]
fn gateway_addr_in_use_message_omits_restart_when_no_available_port() {
    let default = config::GatewayConfig::default();
    let msg = gateway_addr_in_use_message("127.0.0.1", 42617, &default.host, default.port, None);

    assert!(msg.contains("zeroclaw gateway get-paircode\n"));
    assert!(!msg.contains("zeroclaw gateway start --port"));
    assert!(msg.contains("lsof -nP -iTCP:42617 -sTCP:LISTEN"));
}

#[test]
fn gateway_addr_in_use_message_skips_occupied_restart_hint_port() {
    let default = config::GatewayConfig::default();
    let (port, mut listeners) = reserve_consecutive_local_ports(3);
    let available_port = port + 2;
    drop(listeners.pop());

    let restart_port = available_gateway_restart_hint_port("127.0.0.1", port);
    let msg =
        gateway_addr_in_use_message("127.0.0.1", port, &default.host, default.port, restart_port);

    assert!(
        !msg.contains(&format!("zeroclaw gateway start --port {}", port + 1)),
        "{msg}"
    );
    assert!(
        msg.contains(&format!("zeroclaw gateway start --port {available_port}")),
        "{msg}"
    );
}

#[test]
fn gateway_addr_in_use_message_uses_configured_default_gateway_recovery() {
    let msg = gateway_addr_in_use_message("192.168.1.20", 9001, "192.168.1.20", 9001, None);

    assert!(msg.contains("open http://192.168.1.20:9001"));
    assert!(msg.contains("zeroclaw gateway get-paircode\n"));
    assert!(!msg.contains("get-paircode --port 9001"));
}

#[test]
fn gateway_restart_hint_uses_gateway_bind_fallback_for_hostnames() {
    let (port, mut listeners) = reserve_consecutive_local_ports(3);
    let available_port = port + 2;
    drop(listeners.pop());

    assert_eq!(
        available_gateway_restart_hint_port("localhost", port),
        Some(available_port)
    );
}

#[test]
fn gateway_bind_addr_resolver_accepts_bracketed_ipv6_hosts() {
    let addr = zeroclaw_infra::effective_gateway_bind_socket_addr("[::1]", 9001);

    assert_eq!(addr.port(), 9001);
    assert!(addr.is_ipv6());
}

#[test]
fn gateway_addr_in_use_detector_recognizes_nested_io_error() {
    let err = std::io::Error::from(ErrorKind::AddrInUse);
    let err = anyhow::Error::new(err).context("gateway bind failed");

    assert!(is_addr_in_use_error(&err));
}

fn reserve_consecutive_local_ports(count: u16) -> (u16, Vec<TcpListener>) {
    for _ in 0..100 {
        let Ok(first) = TcpListener::bind(("127.0.0.1", 0)) else {
            continue;
        };
        let port = first.local_addr().expect("listener has local addr").port();
        if port > u16::MAX - count {
            continue;
        }

        let mut listeners = vec![first];
        let mut reserved_all = true;
        for offset in 1..count {
            match TcpListener::bind(("127.0.0.1", port + offset)) {
                Ok(listener) => listeners.push(listener),
                Err(_) => {
                    reserved_all = false;
                    break;
                }
            }
        }

        if reserved_all {
            return (port, listeners);
        }
    }

    panic!("could not reserve {count} consecutive local ports");
}

#[test]
#[cfg(feature = "agent-runtime")]
fn onboard_cli_quick_and_channels_only_conflict() {
    // --quick and --channels-only should both parse at the CLI level
    // (the conflict is checked at runtime), but we verify both flags parse.
    let cli = Cli::try_parse_from(["zeroclaw", "onboard", "--quick", "--channels-only"]);
    assert!(
        cli.is_ok(),
        "--quick --channels-only should parse at CLI level"
    );
}

#[test]
#[cfg(feature = "agent-runtime")]
fn onboard_cli_bare_parses() {
    let cli = Cli::try_parse_from(["zeroclaw", "onboard"]).expect("bare onboard should parse");

    match cli.command {
        Commands::Onboard { section, .. } => assert!(section.is_none()),
        other => panic!("expected onboard command, got {other:?}"),
    }
}

#[test]
#[cfg(feature = "agent-runtime")]
fn onboard_cli_positional_sections_parse() {
    for w in zeroclaw_config::sections::QUICKSTART_SECTIONS {
        let cli = Cli::try_parse_from(["zeroclaw", "onboard", w.as_str()])
            .unwrap_or_else(|_| panic!("onboard {} should parse", w.as_str()));
        match cli.command {
            Commands::Onboard { section, .. } => assert_eq!(section, Some(*w)),
            other => panic!("expected onboard command, got {other:?}"),
        }
    }
}

#[test]
#[cfg(feature = "agent-runtime")]
fn homebrew_onboard_config_dir_detects_cellar_paths() {
    assert_eq!(
        resolve_homebrew_onboard_config_dir(
            Path::new("/opt/homebrew/Cellar/zeroclaw/0.8.0/bin/zeroclaw"),
            |_| None,
        ),
        Some(PathBuf::from("/opt/homebrew/var/zeroclaw")),
    );
    assert_eq!(
        resolve_homebrew_onboard_config_dir(
            Path::new("/usr/local/Cellar/zeroclaw/0.8.0/bin/zeroclaw"),
            |_| None,
        ),
        Some(PathBuf::from("/usr/local/var/zeroclaw")),
    );
}

#[test]
#[cfg(feature = "agent-runtime")]
fn homebrew_onboard_config_dir_detects_brew_bin_symlink_layout() {
    let temp = tempfile::tempdir().expect("tempdir");
    let prefix = temp.path().join("homebrew");
    std::fs::create_dir_all(prefix.join("Cellar")).expect("create Cellar marker");
    let exe = prefix.join("bin/zeroclaw");

    assert_eq!(
        resolve_homebrew_onboard_config_dir(&exe, |_| None),
        Some(prefix.join("var/zeroclaw")),
    );
}

#[test]
#[cfg(feature = "agent-runtime")]
fn homebrew_onboard_config_dir_preserves_explicit_runtime_paths() {
    let exe = Path::new("/opt/homebrew/Cellar/zeroclaw/0.8.0/bin/zeroclaw");

    for var in [
        "ZEROCLAW_CONFIG_DIR",
        "ZEROCLAW_DATA_DIR",
        "ZEROCLAW_WORKSPACE",
    ] {
        assert_eq!(
            resolve_homebrew_onboard_config_dir(exe, |name| {
                (name == var).then(|| "/tmp/zeroclaw-explicit".to_string())
            }),
            None,
            "{var} should take precedence over Homebrew detection",
        );
    }
}

#[test]
#[cfg(feature = "agent-runtime")]
fn homebrew_onboard_config_dir_treats_workspace_whitespace_as_explicit() {
    let exe = Path::new("/opt/homebrew/Cellar/zeroclaw/0.8.0/bin/zeroclaw");

    assert_eq!(
        resolve_homebrew_onboard_config_dir(exe, |name| {
            (name == "ZEROCLAW_WORKSPACE").then(|| "   ".to_string())
        }),
        None,
    );
}

#[test]
#[cfg(feature = "agent-runtime")]
fn apply_homebrew_onboard_config_dir_sets_detected_config_dir() {
    let exe = Path::new("/opt/homebrew/Cellar/zeroclaw/0.8.0/bin/zeroclaw");
    let mut applied = None;

    let detected = apply_homebrew_onboard_config_dir_with(
        exe,
        |_| None,
        |name, value| applied = Some((name, value.to_path_buf())),
    );

    assert_eq!(detected, Some(PathBuf::from("/opt/homebrew/var/zeroclaw")));
    assert_eq!(
        applied,
        Some((
            "ZEROCLAW_CONFIG_DIR",
            PathBuf::from("/opt/homebrew/var/zeroclaw"),
        )),
    );
}

#[test]
#[cfg(feature = "agent-runtime")]
fn apply_homebrew_onboard_config_dir_skips_explicit_config_dir() {
    let exe = Path::new("/opt/homebrew/Cellar/zeroclaw/0.8.0/bin/zeroclaw");
    let mut applied = None;

    let detected = apply_homebrew_onboard_config_dir_with(
        exe,
        |name| (name == "ZEROCLAW_CONFIG_DIR").then(|| "/tmp/zeroclaw".to_string()),
        |name, value| applied = Some((name, value.to_path_buf())),
    );

    assert_eq!(detected, None);
    assert_eq!(applied, None);
}

#[test]
#[cfg(feature = "agent-runtime")]
fn cli_parses_estop_default_engage() {
    let cli = Cli::try_parse_from(["zeroclaw", "estop"]).expect("estop command should parse");

    match cli.command {
        Commands::Estop {
            estop_command,
            level,
            domains,
            tools,
        } => {
            assert!(estop_command.is_none());
            assert!(level.is_none());
            assert!(domains.is_empty());
            assert!(tools.is_empty());
        }
        other => panic!("expected estop command, got {other:?}"),
    }
}

#[test]
#[cfg(feature = "agent-runtime")]
fn cli_parses_estop_resume_domain() {
    let cli = Cli::try_parse_from(["zeroclaw", "estop", "resume", "--domain", "*.chase.com"])
        .expect("estop resume command should parse");

    match cli.command {
        Commands::Estop {
            estop_command: Some(EstopSubcommands::Resume { domains, .. }),
            ..
        } => assert_eq!(domains, vec!["*.chase.com".to_string()]),
        other => panic!("expected estop resume command, got {other:?}"),
    }
}

#[test]
#[cfg(feature = "agent-runtime")]
fn agent_command_parses_with_temperature() {
    let cli = Cli::try_parse_from([
        "zeroclaw",
        "agent",
        "--agent",
        "morning-shift",
        "--temperature",
        "0.5",
    ])
    .expect("agent command with temperature should parse");

    match cli.command {
        Commands::Agent { temperature, .. } => {
            assert_eq!(temperature, Some(0.5));
        }
        other => panic!("expected agent command, got {other:?}"),
    }
}

#[test]
#[cfg(feature = "agent-runtime")]
fn agent_command_parses_without_temperature() {
    let cli = Cli::try_parse_from([
        "zeroclaw",
        "agent",
        "--agent",
        "morning-shift",
        "--message",
        "hello",
    ])
    .expect("agent command without temperature should parse");

    match cli.command {
        Commands::Agent { temperature, .. } => {
            assert_eq!(temperature, None);
        }
        other => panic!("expected agent command, got {other:?}"),
    }
}

#[test]
#[cfg(feature = "agent-runtime")]
fn agent_command_parses_session_state_file() {
    let cli = Cli::try_parse_from([
        "zeroclaw",
        "agent",
        "--agent",
        "morning-shift",
        "--session-state-file",
        "session.json",
    ])
    .expect("agent command with session state file should parse");

    match cli.command {
        Commands::Agent {
            session_state_file, ..
        } => {
            assert_eq!(session_state_file, Some(PathBuf::from("session.json")));
        }
        other => panic!("expected agent command, got {other:?}"),
    }
}

#[test]
#[cfg(feature = "agent-runtime")]
fn agent_uses_provider_temperature_when_unset() {
    // When the user doesn't pass --temperature, the agent CLI
    // resolves from the agent's model_provider entry's temperature,
    // bottoming out at 0.7.
    let mut config = Config::default();
    config
        .providers
        .models
        .ensure("openai", "default")
        .expect("known family")
        .temperature = Some(1.5);

    let user_temperature: Option<f64> = std::hint::black_box(None);
    let final_temperature = user_temperature.unwrap_or_else(|| {
        config
            .providers
            .models
            .find("openai", "default")
            .and_then(|e| e.temperature)
            .unwrap_or(0.7)
    });

    assert!((final_temperature - 1.5).abs() < f64::EPSILON);
}

#[test]
#[cfg(feature = "agent-runtime")]
fn config_set_materializes_missing_typed_provider_alias() {
    let mut config = Config::default();
    let path = "providers.models.deepseek.default.model";

    assert!(
        config
            .providers
            .models
            .find("deepseek", "default")
            .is_none(),
        "fresh config should not already contain the requested provider alias"
    );

    let created = commands::config::ensure_map_key_for_prop_path(&mut config, path)
        .expect("known typed provider path should be materialized");

    assert!(created, "missing provider alias should be created");
    config
        .set_prop_persistent(path, "deepseek-chat")
        .expect("materialized path should be writable");
    assert_eq!(
        config
            .providers
            .models
            .find("deepseek", "default")
            .and_then(|provider| provider.model.as_deref()),
        Some("deepseek-chat")
    );

    let known_paths: Vec<String> = config.prop_fields().into_iter().map(|f| f.name).collect();
    let api_key_path = zeroclaw_config::helpers::resolve_field_path(
        &known_paths,
        "providers.models.deepseek.default.api-key",
    );
    config
        .set_prop_persistent(&api_key_path, "sk-test-placeholder")
        .expect("kebab-case secret path should resolve to the materialized typed provider field");
    assert_eq!(
        config
            .providers
            .models
            .find("deepseek", "default")
            .and_then(|provider| provider.api_key.as_deref()),
        Some("sk-test-placeholder")
    );
}

#[test]
#[cfg(feature = "agent-runtime")]
fn config_set_materializes_missing_tts_provider_alias() {
    let mut config = Config::default();
    let path = "providers.tts.openai.alloy.voice";

    assert!(
        config
            .providers
            .tts
            .iter_entries()
            .all(|(family, alias, _)| !(family == "openai" && alias == "alloy")),
        "fresh config should not already contain the requested tts alias"
    );

    let created = commands::config::ensure_map_key_for_prop_path(&mut config, path)
        .expect("known typed tts provider path should be materialized");

    assert!(created, "missing tts alias should be created");
    config
        .set_prop_persistent(path, "alloy")
        .expect("materialized tts path should be writable");
    assert!(
        config
            .providers
            .tts
            .iter_entries()
            .any(|(family, alias, _)| family == "openai" && alias == "alloy"),
        "tts alias should resolve after materialization"
    );
}

#[test]
#[cfg(feature = "agent-runtime")]
fn config_set_materializes_missing_transcription_provider_alias() {
    let mut config = Config::default();
    let raw = "providers.transcription.groq.fast.model";

    assert!(
        config
            .providers
            .transcription
            .iter_aliases()
            .all(|(family, alias)| !(family == "groq" && alias == "fast")),
        "fresh config should not already contain the requested transcription alias"
    );

    // Mirror the CLI `config set` path exactly: resolve, materialize the
    // map key, then re-resolve so the now-present alias field is found.
    let known: Vec<String> = config.prop_fields().into_iter().map(|f| f.name).collect();
    let mut path = zeroclaw_config::helpers::resolve_field_path(&known, raw);
    let created = commands::config::ensure_map_key_for_prop_path(&mut config, &path)
        .expect("known typed transcription provider path should be materialized");
    assert!(created, "missing transcription alias should be created");
    let known: Vec<String> = config.prop_fields().into_iter().map(|f| f.name).collect();
    path = zeroclaw_config::helpers::resolve_field_path(&known, &path);

    config
        .set_prop_persistent(&path, "whisper-large-v3")
        .expect("materialized transcription path should be writable");
    assert!(
        config
            .providers
            .transcription
            .iter_aliases()
            .any(|(family, alias)| family == "groq" && alias == "fast"),
        "transcription alias should resolve after materialization"
    );
}

#[test]
fn config_set_does_not_materialize_non_provider_map_keys() {
    let mut config = Config::default();
    let created = commands::config::ensure_map_key_for_prop_path(
        &mut config,
        "cost.rates.providers.models.openai.gpt-4.1.input_per_mtok",
    )
    .expect("resource-key map paths should be ignored, not rejected");

    assert!(
        !created,
        "auto-materialization must stay scoped to alias-keyed sections, excluding #[resource_key] sections"
    );
    assert!(
        config.cost.rates.providers.models.openai.is_empty(),
        "no bogus model-id key should have been materialized under cost.rates",
    );
}

#[test]
fn ensure_map_key_materializes_non_provider_alias_sections() {
    for (path, value) in [
        ("risk_profiles.newprofile.level", "supervised"),
        ("channels.telegram.main.enabled", "true"),
        ("channels.telegram.main.bot_token", "tok"),
        ("peer_groups.pi400_owner.channel", "telegram.main"),
    ] {
        let mut config = Config::default();
        assert!(
            config.set_prop(path, value).is_err(),
            "precondition: {path} should be unknown on a fresh config"
        );
        let created = commands::config::ensure_map_key_for_prop_path(&mut config, path)
            .expect("newly-widened alias sections should materialize");
        assert!(created, "{path}'s alias should be created");
        assert!(
            config.set_prop(path, value).is_ok(),
            "{path} must be settable after map-key materialization"
        );
    }
}

#[test]
fn ensure_map_key_for_prop_path_refuses_reserved_default_agent() {
    let mut config = Config::default();

    let created =
        commands::config::ensure_map_key_for_prop_path(&mut config, "agents.default.enabled")
            .expect("agents is a known map-keyed section; refusal is not an error");
    assert!(
        !created,
        "must refuse to auto-create the reserved `default` agent alias"
    );
    assert!(
        config.agents.is_empty(),
        "no `agents.default` entry should have been left behind by the refused create"
    );

    let created =
        commands::config::ensure_map_key_for_prop_path(&mut config, "agents.researcher.enabled")
            .expect("non-reserved agent aliases should still materialize");
    assert!(
        created,
        "agents.<non-default> must still auto-materialize like every other widened section"
    );
    assert!(
        config.agents.contains_key("researcher"),
        "researcher alias should have been created"
    );
}

#[test]
fn config_set_materializes_agent_workspace_path() {
    let mut config = Config::default();
    let raw = "agents.assistant.workspace.path";

    let known: Vec<String> = config.prop_fields().into_iter().map(|f| f.name).collect();
    let mut path = zeroclaw_config::helpers::resolve_field_path(&known, raw);
    let created = commands::config::ensure_map_key_for_prop_path(&mut config, &path)
        .expect("agent alias and workspace path should materialize");
    assert!(created, "missing agent alias should be created");

    let known: Vec<String> = config.prop_fields().into_iter().map(|f| f.name).collect();
    path = zeroclaw_config::helpers::resolve_field_path(&known, &path);
    config
        .set_prop_persistent(&path, "/srv/zeroclaw/assistant")
        .expect("agent workspace path should be writable");

    assert_eq!(path, raw);
    assert_eq!(
        config
            .agents
            .get("assistant")
            .and_then(|agent| agent.workspace.path.as_deref()),
        Some(std::path::Path::new("/srv/zeroclaw/assistant"))
    );
}

#[test]
fn ensure_map_key_rolls_back_alias_on_unknown_tail_field() {
    let mut config = Config::default();
    let path = "risk_profiles.newprofile.not_a_real_field";

    let created = commands::config::ensure_map_key_for_prop_path(&mut config, path)
        .expect("section resolves; only the tail field is bogus");
    assert!(
        !created,
        "must not report success when the tail field doesn't resolve"
    );
    assert!(
        config
            .get_map_keys("risk_profiles")
            .unwrap_or_default()
            .is_empty(),
        "the tentatively-created alias must be rolled back, not left dangling",
    );
}

#[test]
fn ensure_map_key_for_prop_path_leaves_existing_hyphenated_alias_alone() {
    let mut config = Config::default();
    config.cron.insert(
        "morning-brief".to_string(),
        zeroclaw_config::schema::CronJobDecl::default(),
    );

    let created =
        commands::config::ensure_map_key_for_prop_path(&mut config, "cron.morning-brief.name")
            .expect("an existing loaded alias must never be rejected by the create grammar");
    assert!(
        !created,
        "the existing `morning-brief` alias must not be reported as newly created"
    );
    assert!(
        config
            .set_prop("cron.morning-brief.name", "Morning brief")
            .is_ok(),
        "setting a field on an existing hyphenated cron alias must succeed"
    );
    assert_eq!(
        config.get_prop("cron.morning-brief.name").ok(),
        Some("Morning brief".to_string())
    );

    let err = commands::config::ensure_map_key_for_prop_path(&mut config, "cron.bad-alias.name")
        .expect_err("creating a NEW hyphenated alias must still be rejected");
    assert!(
        err.to_string().contains("invalid character"),
        "new-alias grammar must be preserved: {err}"
    );
}

// `config init` alias tests. Every test in this module builds a bare
// `Config::default()`, whose `config_path` points at the developer's real
// `~/.zeroclaw/config.toml`, and no gate catches a write from `src/`. These
// stay safe only by calling `init_map_alias` and in-memory readers such as
// `get_map_keys` — never `save()`, `save_dirty()`, a persisting `set_prop`,
// `ensure_disk_at_current_version`, or the real `ConfigCommands::Init` arm.
// End-to-end coverage of the handler lives in `tests/component/`.

#[test]
fn config_init_materializes_new_map_alias() {
    for (arg, section) in [
        ("risk_profiles.strict", "risk_profiles"),
        ("peer_groups.pi400_owner", "peer_groups"),
    ] {
        let mut config = Config::default();
        let created = commands::config::init_map_alias(&mut config, arg)
            .expect("alias-shaped section arguments should materialize");
        assert_eq!(created.as_deref(), Some(arg));
        let alias = arg.rsplit('.').next().expect("alias segment");
        assert!(
            config
                .get_map_keys(section)
                .unwrap_or_default()
                .iter()
                .any(|k| k == alias),
            "{arg} should be present under {section}"
        );
    }
}

#[test]
fn config_init_alias_is_idempotent() {
    let mut config = Config::default();
    commands::config::init_map_alias(&mut config, "risk_profiles.strict").expect("first create");
    let again = commands::config::init_map_alias(&mut config, "risk_profiles.strict")
        .expect("second create");
    assert!(again.is_none(), "an existing alias is not re-reported");
    assert_eq!(
        config.get_map_keys("risk_profiles").unwrap_or_default(),
        vec!["strict".to_string()],
    );
}

#[test]
fn config_init_ignores_plain_section_prefixes() {
    let mut config = Config::default();
    for arg in ["channels.telegram", "gateway"] {
        assert!(
            commands::config::init_map_alias(&mut config, arg)
                .expect("plain prefixes are not an error")
                .is_none(),
            "{arg} has no trailing alias segment; init_defaults keeps ownership"
        );
    }
}

#[test]
fn config_init_ignores_resource_keyed_sections() {
    let mut config = Config::default();
    assert!(
        commands::config::init_map_alias(&mut config, "cost.rates.providers.models.openai.gpt-5")
            .expect("resource-keyed sections are ignored, not rejected")
            .is_none()
    );
    assert!(config.cost.rates.providers.models.openai.is_empty());
}

#[test]
fn config_init_refuses_reserved_default_agent() {
    let mut config = Config::default();
    let err = commands::config::init_map_alias(&mut config, "agents.default")
        .expect_err("the reserved agent guard must surface, not exit 0");
    assert!(
        err.to_string().contains("reserved"),
        "message should name the reserved alias: {err}"
    );
    assert!(config.agents.is_empty());

    assert_eq!(
        commands::config::init_map_alias(&mut config, "agents.researcher")
            .expect("non-reserved agent aliases still materialize")
            .as_deref(),
        Some("agents.researcher"),
    );
}

#[test]
fn config_init_rejects_invalid_alias_key() {
    let mut config = Config::default();
    assert!(
        commands::config::init_map_alias(&mut config, "risk_profiles.Bad-Name").is_err(),
        "validate_alias_key's refusal must propagate"
    );
}

#[test]
#[cfg(feature = "agent-runtime")]
fn config_set_materializes_missing_channel_alias() {
    let mut config = Config::default();
    let path = "channels.telegram.default.bot_token";

    assert!(
        !config.channels.telegram.contains_key("default"),
        "fresh config should not already contain the default channel alias"
    );

    let created = commands::config::ensure_map_key_for_prop_path(&mut config, path)
        .expect("known channel path should be materialized");

    assert!(created, "missing channel alias should be created");
    config
        .set_prop_persistent(path, "test-token")
        .expect("materialized channel path should be writable");
    assert_eq!(
        config
            .channels
            .telegram
            .get("default")
            .unwrap()
            .bot_token
            .as_str(),
        "test-token"
    );
}

#[test]
#[cfg(feature = "agent-runtime")]
fn agent_fallback_uses_hardcoded_when_config_uses_default() {
    // Test that when config uses default value (0.7), fallback still works
    let config = Config::default();

    // Simulate None temperature (user didn't provide --temperature)
    let user_temperature: Option<f64> = std::hint::black_box(None);
    let final_temperature = user_temperature.unwrap_or_else(|| {
        config
            .providers
            .models
            .iter_entries()
            .next()
            .and_then(|(_, _, e)| e.temperature)
            .unwrap_or(0.7)
    });

    assert!((final_temperature - 0.7).abs() < f64::EPSILON);
}

#[tokio::test]
#[cfg(feature = "agent-runtime")]
async fn gate_security_posture_fails_closed_unless_allowed() {
    use crate::config::schema::Config;

    // Clean posture: no gate, no nag.
    let clean = Config::default();
    assert!(clean.degraded_security.is_empty());
    let handle = gate_security_posture(&clean, false).expect("clean posture must pass");
    assert!(handle.is_none(), "clean posture must not spawn a nag");

    // Degraded posture, not allowed: must refuse to serve.
    let mut degraded = Config::default();
    degraded.degraded_security = vec!["security".to_string()];
    assert!(
        gate_security_posture(&degraded, false).is_err(),
        "degraded posture must fail closed when not explicitly allowed"
    );

    // Degraded posture, explicitly allowed: boots and returns a nag handle.
    let nag = gate_security_posture(&degraded, true)
        .expect("degraded posture must boot when allowed")
        .expect("allowed degraded posture must spawn a nag task");
    nag.abort();

    // Whole-config loss (sentinel marker) is degraded too: same fail-closed
    // behavior so a defaulted security posture cannot serve silently.
    let mut whole = Config::default();
    whole.degraded_security = vec![crate::config::migration::WHOLE_CONFIG_SENTINEL.to_string()];
    assert!(
        gate_security_posture(&whole, false).is_err(),
        "whole-config loss must fail closed when not explicitly allowed"
    );
}

#[tokio::test]
#[cfg(feature = "agent-runtime")]
async fn models_set_persists_model_and_preserves_slash_bearing_ids() {
    use crate::config::schema::{AnthropicModelProviderConfig, Config, ModelProviderConfig};

    let tmp = tempfile::TempDir::new().expect("temp dir");
    let config_path = tmp.path().join("config.toml");

    std::fs::write(
        &config_path,
        format!(
            "schema_version = {}\n\n[providers.models.anthropic.default]\nmodel = \"claude-opus-4-7\"\n",
            crate::config::migration::CURRENT_SCHEMA_VERSION,
        ),
    )
    .unwrap();

    let mut config = Config {
        config_path: config_path.clone(),
        data_dir: tmp.path().join("workspace"),
        schema_version: crate::config::migration::CURRENT_SCHEMA_VERSION,
        ..Config::default()
    };
    config.providers.models.anthropic.insert(
        "default".to_string(),
        AnthropicModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("claude-opus-4-7".to_string()),
                ..Default::default()
            },
        },
    );

    // ── Test 1: Normal model ID persists via the dispatch boundary ──
    dispatch_models_command(
        ModelCommands::Set {
            model: "claude-sonnet-4-6".to_string(),
        },
        &mut config,
    )
    .await
    .expect("normal model ID must persist");

    let contents = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        contents.contains("claude-sonnet-4-6"),
        "normal model ID must be persisted to config.toml; got:\n{contents}"
    );

    // ── Test 2: Slash-bearing model ID preserved as-is ──
    dispatch_models_command(
        ModelCommands::Set {
            model: "anthropic/claude-sonnet-4-20250514".to_string(),
        },
        &mut config,
    )
    .await
    .expect("slash-bearing model ID must persist");

    let contents = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        contents.contains("anthropic/claude-sonnet-4-20250514"),
        "slash-bearing model ID must be stored as-is; got:\n{contents}"
    );

    // ── Test 3: No configured provider → error surfaced by dispatch ──
    let mut empty_config = Config {
        config_path: tmp.path().join("empty.toml"),
        data_dir: tmp.path().join("empty_workspace"),
        schema_version: crate::config::migration::CURRENT_SCHEMA_VERSION,
        ..Config::default()
    };
    let err = dispatch_models_command(
        ModelCommands::Set {
            model: "any-model".to_string(),
        },
        &mut empty_config,
    )
    .await
    .expect_err("empty config must fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("No model provider configured"),
        "error must mention missing provider; got: {msg}"
    );
}

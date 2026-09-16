use super::*;

#[test]
fn validate_url_blocks_ipv6_ssrf() {
    let security = Arc::new(SecurityPolicy::default());
    let tool = BrowserTool::new(security, vec!["*".into()], None).unwrap();
    assert!(tool.validate_url("https://[::1]/").is_err());
    assert!(tool.validate_url("https://[::ffff:127.0.0.1]/").is_err());
    assert!(
        tool.validate_url("https://[::ffff:10.0.0.1]:8080/")
            .is_err()
    );
}

#[test]
fn browser_backend_parser_accepts_supported_values() {
    assert_eq!(
        BrowserBackendKind::parse("agent_browser").unwrap(),
        BrowserBackendKind::AgentBrowser
    );
    assert_eq!(
        BrowserBackendKind::parse("rust-native").unwrap(),
        BrowserBackendKind::RustNative
    );
    assert_eq!(
        BrowserBackendKind::parse("computer_use").unwrap(),
        BrowserBackendKind::ComputerUse
    );
    assert_eq!(
        BrowserBackendKind::parse("auto").unwrap(),
        BrowserBackendKind::Auto
    );
}

#[test]
fn browser_backend_parser_rejects_unknown_values() {
    assert!(BrowserBackendKind::parse("playwright").is_err());
}

#[test]
fn browser_tool_default_backend_is_agent_browser() {
    let security = Arc::new(SecurityPolicy::default());
    let tool = BrowserTool::new(security, vec!["example.com".into()], None).unwrap();
    assert_eq!(
        tool.configured_backend().unwrap(),
        BrowserBackendKind::AgentBrowser
    );
}

#[test]
fn agent_browser_command_inherits_headed_env_by_default() {
    let headed_key = std::ffi::OsStr::new("AGENT_BROWSER_HEADED");
    let security = Arc::new(SecurityPolicy::default());
    let tool = BrowserTool::new(security, vec!["example.com".into()], None).unwrap();
    let cmd = tool.agent_browser_command();

    assert_eq!(
        cmd.as_std()
            .get_envs()
            .find(|(key, _)| *key == headed_key)
            .map(|(_, value)| value),
        None
    );
}

#[test]
fn agent_browser_command_clears_headed_env_when_configured_false() {
    let headed_key = std::ffi::OsStr::new("AGENT_BROWSER_HEADED");
    let security = Arc::new(SecurityPolicy::default());
    let tool = BrowserTool::new_with_backend(
        security,
        vec!["example.com".into()],
        None,
        "agent_browser".into(),
        Some(false),
        true,
        "http://127.0.0.1:9515".into(),
        None,
        ComputerUseConfig::default(),
        Vec::new(),
    )
    .unwrap();
    let cmd = tool.agent_browser_command();

    assert_eq!(
        cmd.as_std()
            .get_envs()
            .find(|(key, _)| *key == headed_key)
            .map(|(_, value)| value),
        Some(None)
    );
}

#[test]
fn agent_browser_command_sets_headed_env_when_configured() {
    let headed_key = std::ffi::OsStr::new("AGENT_BROWSER_HEADED");
    let security = Arc::new(SecurityPolicy::default());
    let tool = BrowserTool::new_with_backend(
        security,
        vec!["example.com".into()],
        None,
        "agent_browser".into(),
        Some(true),
        true,
        "http://127.0.0.1:9515".into(),
        None,
        ComputerUseConfig::default(),
        Vec::new(),
    )
    .unwrap();
    let cmd = tool.agent_browser_command();

    assert_eq!(
        cmd.as_std()
            .get_envs()
            .find(|(key, _)| *key == headed_key)
            .and_then(|(_, value)| value)
            .and_then(|value| value.to_str()),
        Some("1")
    );
}

#[test]
fn browser_tool_accepts_auto_backend_config() {
    let security = Arc::new(SecurityPolicy::default());
    let tool = BrowserTool::new_with_backend(
        security,
        vec!["example.com".into()],
        None,
        "auto".into(),
        None,
        true,
        "http://127.0.0.1:9515".into(),
        None,
        ComputerUseConfig::default(),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(tool.configured_backend().unwrap(), BrowserBackendKind::Auto);
}

#[test]
fn browser_tool_accepts_computer_use_backend_config() {
    let security = Arc::new(SecurityPolicy::default());
    let tool = BrowserTool::new_with_backend(
        security,
        vec!["example.com".into()],
        None,
        "computer_use".into(),
        None,
        true,
        "http://127.0.0.1:9515".into(),
        None,
        ComputerUseConfig::default(),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(
        tool.configured_backend().unwrap(),
        BrowserBackendKind::ComputerUse
    );
}

#[test]
fn computer_use_endpoint_rejects_public_http_by_default() {
    let security = Arc::new(SecurityPolicy::default());
    let tool = BrowserTool::new_with_backend(
        security,
        vec!["example.com".into()],
        None,
        "computer_use".into(),
        None,
        true,
        "http://127.0.0.1:9515".into(),
        None,
        ComputerUseConfig {
            endpoint: "http://computer-use.example.com/v1/actions".into(),
            ..ComputerUseConfig::default()
        },
        Vec::new(),
    )
    .unwrap();

    assert!(tool.computer_use_endpoint_url().is_err());
}

#[test]
fn computer_use_endpoint_requires_https_for_public_remote() {
    let security = Arc::new(SecurityPolicy::default());
    let tool = BrowserTool::new_with_backend(
        security,
        vec!["example.com".into()],
        None,
        "computer_use".into(),
        None,
        true,
        "http://127.0.0.1:9515".into(),
        None,
        ComputerUseConfig {
            endpoint: "https://computer-use.example.com/v1/actions".into(),
            allow_remote_endpoint: true,
            ..ComputerUseConfig::default()
        },
        Vec::new(),
    )
    .unwrap();

    assert!(tool.computer_use_endpoint_url().is_ok());
}

#[test]
fn computer_use_coordinate_validation_applies_limits() {
    let security = Arc::new(SecurityPolicy::default());
    let tool = BrowserTool::new_with_backend(
        security,
        vec!["example.com".into()],
        None,
        "computer_use".into(),
        None,
        true,
        "http://127.0.0.1:9515".into(),
        None,
        ComputerUseConfig {
            max_coordinate_x: Some(100),
            max_coordinate_y: Some(100),
            ..ComputerUseConfig::default()
        },
        Vec::new(),
    )
    .unwrap();

    assert!(
        tool.validate_coordinate("x", 50, tool.computer_use.max_coordinate_x)
            .is_ok()
    );
    assert!(
        tool.validate_coordinate("x", 101, tool.computer_use.max_coordinate_x)
            .is_err()
    );
    assert!(
        tool.validate_coordinate("y", -1, tool.computer_use.max_coordinate_y)
            .is_err()
    );
}

#[test]
fn browser_tool_name() {
    let security = Arc::new(SecurityPolicy::default());
    let tool = BrowserTool::new(security, vec!["example.com".into()], None).unwrap();
    assert_eq!(tool.name(), "browser");
}

#[test]
fn browser_tool_validates_url() {
    let security = Arc::new(SecurityPolicy::default());
    let tool = BrowserTool::new(security, vec!["example.com".into()], None).unwrap();

    // Valid
    assert!(tool.validate_url("https://example.com").is_ok());
    assert!(tool.validate_url("https://sub.example.com/path").is_ok());

    // Invalid - not in allowlist
    assert!(tool.validate_url("https://other.com").is_err());

    // Invalid - private host
    assert!(tool.validate_url("https://localhost").is_err());
    assert!(tool.validate_url("https://127.0.0.1").is_err());

    // Invalid - not https
    assert!(tool.validate_url("ftp://example.com").is_err());

    // file:// URLs blocked (local file exfiltration risk)
    assert!(tool.validate_url("file:///tmp/test.html").is_err());
}

#[test]
fn browser_tool_empty_allowlist_blocks() {
    let security = Arc::new(SecurityPolicy::default());
    let tool = BrowserTool::new(security, vec![], None).unwrap();
    assert!(tool.validate_url("https://example.com").is_err());
}

#[test]
fn computer_use_only_action_detection_is_correct() {
    assert!(is_computer_use_only_action("mouse_move"));
    assert!(is_computer_use_only_action("mouse_click"));
    assert!(is_computer_use_only_action("mouse_drag"));
    assert!(is_computer_use_only_action("key_type"));
    assert!(is_computer_use_only_action("key_press"));
    assert!(is_computer_use_only_action("screen_capture"));
    assert!(!is_computer_use_only_action("open"));
    assert!(!is_computer_use_only_action("snapshot"));
}

#[test]
fn unavailable_action_error_preserves_backend_context() {
    assert_eq!(
        unavailable_action_for_backend_error("mouse_move", ResolvedBackend::AgentBrowser),
        "Action 'mouse_move' is unavailable for backend 'agent_browser'"
    );
    assert_eq!(
        unavailable_action_for_backend_error("mouse_move", ResolvedBackend::RustNative),
        "Action 'mouse_move' is unavailable for backend 'rust_native'"
    );
}

#[test]
fn recoverable_error_detection_matches_session_patterns() {
    for message in [
        "invalid session id",
        "No Such Window",
        "session not created",
        "connection reset by peer",
        "broken pipe while writing webdriver command",
        "WebDriver request timed out",
    ] {
        let err = anyhow::Error::msg(message);
        assert!(is_recoverable_rust_native_error(&err), "{message}");
    }

    let allowlist_error =
        anyhow::Error::msg("URL host 'localhost' is not in browser allowlist [example.com]");
    assert!(!is_recoverable_rust_native_error(&allowlist_error));
}

#[test]
fn non_recoverable_error_detection_rejects_policy_errors() {
    for message in [
        "Blocked by security policy",
        "URL host '127.0.0.1' is private and disallowed",
        "Action 'mouse_move' is unavailable for backend 'rust_native'",
    ] {
        let err = anyhow::Error::msg(message);
        assert!(!is_recoverable_rust_native_error(&err), "{message}");
    }
}

#[cfg(feature = "browser-native")]
#[test]
fn reset_session_is_idempotent_without_client() {
    tokio_test::block_on(async {
        let mut state = native_backend::NativeBrowserState::default();
        state.reset_session().await;
        state.reset_session().await;
    });
}

#[test]
fn ensure_browser_env_sets_home_when_missing() {
    let original_home = std::env::var_os("HOME");
    unsafe { std::env::remove_var("HOME") };

    let mut cmd = Command::new("true");
    ensure_browser_env(&mut cmd);
    // Function completes without panic — HOME and CHROMIUM_FLAGS set on cmd.

    if let Some(home) = original_home {
        unsafe { std::env::set_var("HOME", home) };
    }
}

#[test]
fn ensure_browser_env_sets_chromium_flags() {
    let original = std::env::var_os("CHROMIUM_FLAGS");
    unsafe { std::env::remove_var("CHROMIUM_FLAGS") };

    let mut cmd = Command::new("true");
    ensure_browser_env(&mut cmd);

    if let Some(val) = original {
        unsafe { std::env::set_var("CHROMIUM_FLAGS", val) };
    }
}

#[test]
fn is_service_environment_detects_invocation_id() {
    let original = std::env::var_os("INVOCATION_ID");
    unsafe { std::env::set_var("INVOCATION_ID", "test-unit-id") };

    assert!(is_service_environment());

    if let Some(val) = original {
        unsafe { std::env::set_var("INVOCATION_ID", val) };
    } else {
        unsafe { std::env::remove_var("INVOCATION_ID") };
    }
}

#[test]
fn is_service_environment_detects_journal_stream() {
    let original = std::env::var_os("JOURNAL_STREAM");
    unsafe { std::env::set_var("JOURNAL_STREAM", "8:12345") };

    assert!(is_service_environment());

    if let Some(val) = original {
        unsafe { std::env::set_var("JOURNAL_STREAM", val) };
    } else {
        unsafe { std::env::remove_var("JOURNAL_STREAM") };
    }
}

#[test]
fn is_service_environment_false_in_normal_context() {
    let inv = std::env::var_os("INVOCATION_ID");
    let journal = std::env::var_os("JOURNAL_STREAM");
    unsafe { std::env::remove_var("INVOCATION_ID") };
    unsafe { std::env::remove_var("JOURNAL_STREAM") };

    if std::env::var_os("HOME").is_some() {
        assert!(!is_service_environment());
    }

    if let Some(val) = inv {
        unsafe { std::env::set_var("INVOCATION_ID", val) };
    }
    if let Some(val) = journal {
        unsafe { std::env::set_var("JOURNAL_STREAM", val) };
    }
}

#[test]
fn windows_command_name_selection() {
    // Verify the cfg-based command name logic used in is_agent_browser_available
    // and run_command selects the correct binary name per platform.
    let cmd = if cfg!(target_os = "windows") {
        "agent-browser.cmd"
    } else {
        "agent-browser"
    };

    if cfg!(target_os = "windows") {
        assert_eq!(cmd, "agent-browser.cmd");
    } else {
        assert_eq!(cmd, "agent-browser");
    }
}

// ── allowed_private_hosts opt-in tests ──────────────────────

fn private_host_tool(allowed_domains: Vec<&str>, allowed_private_hosts: Vec<&str>) -> BrowserTool {
    let security = Arc::new(SecurityPolicy::default());
    BrowserTool::new_with_backend(
        security,
        allowed_domains.into_iter().map(String::from).collect(),
        None,
        "agent_browser".into(),
        None,
        true,
        "http://127.0.0.1:9515".into(),
        None,
        ComputerUseConfig::default(),
        allowed_private_hosts
            .into_iter()
            .map(String::from)
            .collect(),
    )
    .unwrap()
}

#[test]
fn wildcard_private_allowlist_permits_localhost() {
    let tool = private_host_tool(vec![], vec!["*"]);
    assert!(tool.validate_url("http://localhost:8080").is_ok());
    assert!(tool.validate_url("https://localhost:8443").is_ok());
}

#[test]
fn wildcard_private_allowlist_permits_rfc1918() {
    let tool = private_host_tool(vec![], vec!["*"]);
    assert!(tool.validate_url("http://192.168.1.5").is_ok());
    assert!(tool.validate_url("http://10.0.0.1").is_ok());
    assert!(tool.validate_url("http://172.16.0.1").is_ok());
}

#[test]
fn wildcard_private_allowlist_does_not_loosen_file_scheme() {
    // file:// is always blocked, regardless of allowed_private_hosts.
    let tool = private_host_tool(vec!["*"], vec!["*"]);
    let err = tool
        .validate_url("file:///etc/passwd")
        .unwrap_err()
        .to_string();
    assert!(err.contains("file://"));
}

#[test]
fn allowed_private_hosts_entry_permits_listed_host() {
    let tool = private_host_tool(vec![], vec!["10.0.0.1"]);
    assert!(tool.validate_url("http://10.0.0.1").is_ok());
}

#[test]
fn allowed_private_hosts_does_not_permit_unlisted_host() {
    let tool = private_host_tool(vec![], vec!["10.0.0.1"]);
    let err = tool
        .validate_url("http://10.0.0.2")
        .unwrap_err()
        .to_string();
    assert!(err.contains("local/private"));
}

#[test]
fn empty_private_allowlist_still_rejects_private() {
    let tool = private_host_tool(vec!["*"], vec![]);
    let err = tool
        .validate_url("https://localhost")
        .unwrap_err()
        .to_string();
    assert!(err.contains("local/private"));
}

#[test]
fn wildcard_private_allowlist_satisfies_allowlist_requirement() {
    // allowed_domains empty + allowed_private_hosts=["*"] should not surface
    // the "no allowed_domains configured" error for private hosts.
    let tool = private_host_tool(vec![], vec!["*"]);
    assert!(tool.validate_url("http://localhost").is_ok());
}

#[test]
fn specific_private_host_alone_satisfies_allowlist_requirement() {
    let tool = private_host_tool(vec![], vec!["192.168.1.5"]);
    assert!(tool.validate_url("http://192.168.1.5").is_ok());
}

#[test]
fn wildcard_private_allowlist_does_not_widen_public_allowlist() {
    // Public hosts are still subject to allowed_domains when private hosts
    // are wide-open — the bypass is scoped to private/local hosts only.
    let tool = private_host_tool(vec!["example.com"], vec!["*"]);
    let err = tool
        .validate_url("https://other.com")
        .unwrap_err()
        .to_string();
    assert!(err.contains("allowed_domains"));
}

#[test]
fn userinfo_url_targeting_private_host_rejected_under_wildcard_public_allowlist() {
    // Default-shipped posture: allowed_domains = ["*"], no private
    // allowlist. `extract_host` would otherwise treat
    // `example.com@127.0.0.1` as the host and accept it.
    let tool = private_host_tool(vec!["*"], vec![]);
    let err = tool
        .validate_url("http://example.com@127.0.0.1/")
        .unwrap_err()
        .to_string();
    assert!(err.contains("userinfo"), "got: {err}");
}

#[test]
fn userinfo_url_targeting_private_host_rejected_under_wildcard_private_allowlist() {
    // Even with the private bypass wide open, userinfo is rejected before
    // host classification — so this is a parser-mismatch defense, not a
    // policy decision the operator can opt around.
    let tool = private_host_tool(vec!["*"], vec!["*"]);
    let err = tool
        .validate_url("http://example.com@127.0.0.1/")
        .unwrap_err()
        .to_string();
    assert!(err.contains("userinfo"), "got: {err}");
}

#[test]
fn userinfo_url_with_password_rejected() {
    // `user:pass@host` form — same parser hole, same fix.
    let tool = private_host_tool(vec!["*"], vec![]);
    let err = tool
        .validate_url("https://user:pass@10.0.0.1/")
        .unwrap_err()
        .to_string();
    assert!(err.contains("userinfo"), "got: {err}");
}

#[test]
fn query_only_url_targeting_private_host_rejected_under_wildcard_public_allowlist() {
    let tool = private_host_tool(vec!["*"], vec![]);
    let err = tool
        .validate_url("http://127.0.0.1?x")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("local/private host"),
        "expected private-host block, got: {err}",
    );
}

#[test]
fn fragment_only_url_targeting_private_host_rejected_under_wildcard_public_allowlist() {
    let tool = private_host_tool(vec!["*"], vec![]);
    let err = tool
        .validate_url("http://127.0.0.1#x")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("local/private host"),
        "expected private-host block, got: {err}",
    );
}

// ============ Screenshot path validation tests ============

use zeroclaw_config::policy::AutonomyLevel;

fn screenshot_tool_with_workspace(ws: &std::path::Path) -> BrowserTool {
    let security = Arc::new(SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        workspace_dir: ws.to_path_buf(),
        allowed_roots: vec![ws.to_path_buf()],
        ..SecurityPolicy::default()
    });
    BrowserTool::new(security, vec!["*".into()], None).unwrap()
}

#[tokio::test]
async fn validate_screenshot_path_allows_path_inside_workspace() {
    let tmp = tempfile::TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let shots = ws.join("shots");
    tokio::fs::create_dir_all(&shots).await.unwrap();

    let tool = screenshot_tool_with_workspace(&ws);
    let mut action = BrowserAction::Screenshot {
        path: Some("shots/page.png".into()),
        full_page: false,
    };

    // Canonicalize the expected workspace path first (macOS fix)
    let expected_canonical = std::fs::canonicalize(&ws).unwrap();

    tool.validate_screenshot_path(&mut action).await.unwrap();

    // Verify path is replaced with canonical form
    if let BrowserAction::Screenshot { path, .. } = action {
        let canonical_path = path.unwrap();
        // Compare canonical forms, not raw strings
        assert!(canonical_path.starts_with(expected_canonical.to_string_lossy().as_ref()));
        assert!(canonical_path.ends_with("page.png"));
    } else {
        panic!("action should still be Screenshot");
    }
}

#[tokio::test]
async fn validate_screenshot_path_rejects_path_outside_workspace() {
    let tmp = tempfile::TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let outside = tmp.path().join("outside");
    tokio::fs::create_dir_all(&ws).await.unwrap();
    tokio::fs::create_dir_all(&outside).await.unwrap();

    // Create a file in the outside directory so canonicalize succeeds
    let outside_file = outside.join("page.png");
    tokio::fs::write(&outside_file, b"test").await.unwrap();

    // Use absolute path that's not in allowed_roots
    let security = Arc::new(SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        workspace_dir: ws.clone(),
        allowed_roots: vec![ws.clone()], // outside is NOT in allowed_roots
        ..SecurityPolicy::default()
    });

    let tool = BrowserTool::new(security, vec!["*".into()], None).unwrap();
    let mut action = BrowserAction::Screenshot {
        path: Some(outside_file.to_string_lossy().to_string()),
        full_page: false,
    };

    let err = tool
        .validate_screenshot_path(&mut action)
        .await
        .unwrap_err();
    // Should be rejected as outside workspace
    assert!(
        err.to_string().contains("outside-workspace")
            || err.to_string().contains("outside/page.png"),
        "Expected outside-workspace rejection, got: {}",
        err
    );
}

#[tokio::test]
async fn validate_screenshot_path_rejects_traversal() {
    let tmp = tempfile::TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    tokio::fs::create_dir_all(&ws).await.unwrap();

    let tool = screenshot_tool_with_workspace(&ws);
    let mut action = BrowserAction::Screenshot {
        path: Some("../../etc/passwd".into()),
        full_page: false,
    };

    let err = tool
        .validate_screenshot_path(&mut action)
        .await
        .unwrap_err();
    // String-level traversal should be rejected with path-not-allowed error
    assert!(
        err.to_string().contains("not in the workspace allowlist")
            || err.to_string().contains("../../etc/passwd"),
        "Expected traversal rejection, got: {}",
        err
    );
}

#[tokio::test]
async fn validate_screenshot_path_noop_when_path_none() {
    let tmp = tempfile::TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    tokio::fs::create_dir_all(&ws).await.unwrap();

    let tool = screenshot_tool_with_workspace(&ws);
    let mut action = BrowserAction::Screenshot {
        path: None,
        full_page: false,
    };

    tool.validate_screenshot_path(&mut action).await.unwrap();
    assert!(matches!(
        action,
        BrowserAction::Screenshot { path: None, .. }
    ));
}

#[tokio::test]
async fn validate_screenshot_path_rejects_runtime_config_target() {
    let tmp = tempfile::TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let config_dir = tmp.path().join("config");
    tokio::fs::create_dir_all(&ws).await.unwrap();
    tokio::fs::create_dir_all(&config_dir).await.unwrap();

    // Create an actual config.toml file in the config directory
    let config_path = config_dir.join("config.toml");
    tokio::fs::write(&config_path, b"").await.unwrap();

    // Create the config file so is_runtime_config_path detects it
    tokio::fs::write(&config_path, b"test").await.unwrap();

    let security = Arc::new(SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        workspace_dir: ws.clone(),
        allowed_roots: vec![ws.clone(), config_dir.clone()],
        config_path: Some(config_path.clone()),
        ..SecurityPolicy::default()
    });

    let tool = BrowserTool::new(security, vec!["*".into()], None).unwrap();

    let mut action = BrowserAction::Screenshot {
        path: Some(config_path.to_string_lossy().to_string()),
        full_page: false,
    };

    // Should be rejected as runtime-config target
    let err = tool
        .validate_screenshot_path(&mut action)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("runtime config") || err.to_string().contains("Refusing"),
        "Expected runtime-config rejection, got: {}",
        err
    );
}

#[tokio::test]
#[cfg(unix)]
async fn validate_screenshot_path_rejects_existing_symlink_target() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let outside = tmp.path().join("outside");
    tokio::fs::create_dir_all(&ws).await.unwrap();
    tokio::fs::create_dir_all(&outside).await.unwrap();

    // Create a symlink inside workspace pointing outside
    let link_path = ws.join("page.png");
    let target_path = outside.join("real.txt");
    tokio::fs::write(&target_path, b"real").await.unwrap();
    symlink(&target_path, &link_path).unwrap();

    let tool = screenshot_tool_with_workspace(&ws);
    let mut action = BrowserAction::Screenshot {
        path: Some("page.png".into()),
        full_page: false,
    };

    let err = tool
        .validate_screenshot_path(&mut action)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("symlink"));
}

/// Path-identity regression: a valid UTF-8 alias can resolve (through a
/// symlink) to a canonical parent whose name contains non-UTF-8 bytes. The
/// allowlist validates the byte-preserving `PathBuf`, but the backends
/// consume the destination as a UTF-8 string — a lossy conversion would
/// silently rewrite the pathname and name a location that never passed the
/// policy. `execute_action` must reject such a target before either local
/// backend receives the action. If the validator call is removed, this
/// fails (the backends would otherwise succeed or error without the
/// specific allowlist rejection).
#[tokio::test]
// APFS rejects non-UTF-8 path components (errno 92), so this fixture cannot
// be constructed on macOS; keep the regression on Linux/other Unix FS.
#[cfg(all(unix, not(target_os = "macos")))]
async fn execute_action_rejects_non_utf8_canonical_target_before_backend_dispatch() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;

    let tmp = tempfile::TempDir::new().unwrap();
    let ws = tmp.path().join("ws");

    // A real directory (inside the workspace allowlist) whose name carries a
    // raw non-UTF-8 byte, so the outside-workspace gate does not fire first.
    let mut raw_name = b"nonutf8-".to_vec();
    raw_name.push(0xFF);
    let non_utf8_dir = ws.join(std::path::PathBuf::from(OsString::from_vec(raw_name)));
    tokio::fs::create_dir_all(non_utf8_dir.join("shots"))
        .await
        .unwrap();

    // UTF-8 symlink alias inside the workspace -> the non-UTF-8 directory.
    symlink(&non_utf8_dir, ws.join("alias")).unwrap();

    let tool = screenshot_tool_with_workspace(&ws);

    // AgentBrowser: the rejection must come from the validator, before
    // dispatch (the backend would otherwise never see this exact error).
    let action = BrowserAction::Screenshot {
        path: Some("alias/shots/page.png".into()),
        full_page: false,
    };
    let err = tool
        .execute_action(action, ResolvedBackend::AgentBrowser)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("non-UTF-8"),
        "a non-UTF-8 canonical target must be rejected by the validator before backend \
             dispatch, got: {err}"
    );

    // RustNative: same gate, same rejection, before the local write.
    let action2 = BrowserAction::Screenshot {
        path: Some("alias/shots/page.png".into()),
        full_page: false,
    };
    let err = tool
        .execute_action(action2, ResolvedBackend::RustNative)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("non-UTF-8"),
        "a non-UTF-8 canonical target must be rejected for rust_native too, got: {err}"
    );
}

/// ComputerUse shares the same canonical target validator, so a non-UTF-8
/// canonical destination is rejected locally — before the sidecar round
/// trip — and is never forwarded.
#[tokio::test]
#[cfg(all(unix, not(target_os = "macos")))]
async fn computer_use_rejects_non_utf8_canonical_target_before_sidecar() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;

    let tmp = tempfile::TempDir::new().unwrap();
    let ws = tmp.path().join("ws");

    let mut raw_name = b"nonutf8-".to_vec();
    raw_name.push(0xFF);
    let non_utf8_dir = ws.join(std::path::PathBuf::from(OsString::from_vec(raw_name)));
    tokio::fs::create_dir_all(non_utf8_dir.join("shots"))
        .await
        .unwrap();
    symlink(&non_utf8_dir, ws.join("alias")).unwrap();

    // ComputerUse tool whose workspace is the temp `ws` (the shared helper
    // pins `current_dir`).
    let security = Arc::new(SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        workspace_dir: ws.clone(),
        allowed_roots: vec![ws.clone()],
        ..SecurityPolicy::default()
    });
    let tool = BrowserTool::new_with_backend(
        security,
        vec!["*".into()],
        None,
        "computer_use".into(),
        None,
        true,
        "http://127.0.0.1:9515".into(),
        None,
        test_computer_use_config(),
        Vec::new(),
    )
    .unwrap();

    let err = tool
        .validate_screenshot_path_for_computer_use(
            "screenshot",
            json!({"path": "alias/shots/page.png"}),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("non-UTF-8"),
        "computer_use must reject a non-UTF-8 canonical target before the sidecar call, got: {err}"
    );
}

#[tokio::test]
#[cfg(unix)]
async fn validate_screenshot_path_allows_existing_regular_file_target() {
    let tmp = tempfile::TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    tokio::fs::create_dir_all(&ws).await.unwrap();

    // Create a regular file (not symlink) inside workspace
    let file_path = ws.join("existing.png");
    tokio::fs::write(&file_path, b"existing").await.unwrap();

    let tool = screenshot_tool_with_workspace(&ws);
    let mut action = BrowserAction::Screenshot {
        path: Some("existing.png".into()),
        full_page: false,
    };

    // Should succeed - regular files are OK
    tool.validate_screenshot_path(&mut action).await.unwrap();
}

#[tokio::test]
async fn execute_action_rejects_malicious_screenshot_before_local_backend_dispatch() {
    // Production-boundary regression for the `execute_action` wiring
    // (line ~1302): a screenshot action carrying a traversal path must be
    // rejected by `validate_screenshot_path` before either local backend
    // (AgentBrowser or RustNative) receives it. If that call is removed,
    // the validation error never fires and this assertion fails — the
    // backend-specific error does not mention the path or the allowlist.
    let tmp = tempfile::TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    tokio::fs::create_dir_all(&ws).await.unwrap();

    let tool = screenshot_tool_with_workspace(&ws);
    let action = BrowserAction::Screenshot {
        path: Some("../etc/passwd".into()),
        full_page: false,
    };

    let err = tool
        .execute_action(action, ResolvedBackend::AgentBrowser)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("not in the workspace allowlist"),
        "traversal path must be rejected by the screenshot-path validator before backend \
             dispatch (the specific allowlist rejection, not any error echoing the path), got: {err}"
    );

    // The mut-borrow contract still holds for the second local backend.
    let action2 = BrowserAction::Screenshot {
        path: Some("../etc/passwd".into()),
        full_page: false,
    };
    let err = tool
        .execute_action(action2, ResolvedBackend::RustNative)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("not in the workspace allowlist"),
        "traversal path must be rejected at execute_action for rust_native too, got: {err}"
    );
}

/// `Tool::execute` raw-input boundary: a present non-string `path` must be
/// rejected up front — the same contract the ComputerUse path enforces —
/// rather than silently coerced to `None` (which would make the local
/// backends take an inline screenshot while ComputerUse rejects the same
/// input).
#[tokio::test]
async fn execute_rejects_present_non_string_screenshot_path() {
    // Parser boundary (no backend dependency): a present non-string path
    // must be rejected at parse time, never coerced to `None`. This is the
    // mutation-sensitive assertion — reverting the parser's non-string
    // branch back to `None` coercion makes `expect_err` fail regardless of
    // whether a backend is available in the test environment.
    let parse_err = parse_browser_action("screenshot", &json!({ "path": 123 }))
        .expect_err("a present non-string path must be rejected by the parser")
        .to_string();
    assert!(
        parse_err.contains("must be a string"),
        "the parser must name the string contract, got: {parse_err}"
    );

    let tmp = tempfile::TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    tokio::fs::create_dir_all(&ws).await.unwrap();
    let tool = screenshot_tool_with_workspace(&ws);

    // Local backend path (default). The non-string path must produce a
    // rejection ToolResult, never a silent inline screenshot.
    let result = tool
        .execute(json!({
            "action": "screenshot",
            "path": 123,
        }))
        .await
        .expect("execute must not panic on a non-string path");
    assert!(
        !result.success,
        "a present non-string path must be rejected, not coerced to None; got: {:?}",
        result.output
    );

    // ComputerUse path: same contract, non-string path rejected.
    let tool = browser_tool_with_computer_use(test_computer_use_config());
    let result = tool
        .execute(json!({
            "action": "screenshot",
            "path": json!({"nested": "object"}),
        }))
        .await
        .expect("execute must not panic on a non-string path");
    assert!(
        !result.success,
        "computer_use must reject a present non-string path too; got: {:?}",
        result.output
    );
}

// ============ ComputerUse dispatch tests ============

fn test_computer_use_config() -> ComputerUseConfig {
    ComputerUseConfig {
        endpoint: "http://127.0.0.1:8787".to_string(),
        api_key: None,
        timeout_ms: 5000,
        allow_remote_endpoint: true,
        window_allowlist: vec![],
        max_coordinate_x: None,
        max_coordinate_y: None,
    }
}

#[cfg(test)]
fn browser_tool_with_computer_use(config: ComputerUseConfig) -> BrowserTool {
    let security = Arc::new(SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        workspace_dir: std::env::current_dir().unwrap(),
        allowed_roots: vec![std::env::current_dir().unwrap()],
        ..SecurityPolicy::default()
    });
    BrowserTool::new_with_backend(
        security,
        vec!["*".into()],
        None,
        "computer_use".into(),
        None,
        true,
        "http://127.0.0.1:9515".into(),
        None,
        config,
        Vec::new(),
    )
    .unwrap()
}

#[tokio::test]
async fn computer_use_dispatch_rejects_traversal_path_before_sidecar() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Start a mock server to pass the endpoint reachability check
    let server = MockServer::start().await;

    // Mock the reachability check (GET request)
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    // Mock the POST endpoint - should NOT be called because traversal is rejected before sidecar
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let mut config = test_computer_use_config();
    config.endpoint = server.uri();
    let tool = browser_tool_with_computer_use(config);

    let args = json!({
        "action": "screenshot",
        "path": "../etc/passwd"
    });

    // Validation happens in execute_computer_use_action, returns ToolResult with error
    let result = tool.execute(args).await.unwrap();
    assert!(!result.success, "Expected validation to fail");
    let error = result.error.expect("Expected error in result");
    assert!(
        error.contains("not in the workspace allowlist") || error.contains("../etc/passwd"),
        "Expected traversal rejection, got: {}",
        error
    );
}

#[tokio::test]
async fn computer_use_dispatch_rejects_runtime_config_target_before_sidecar() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let config_dir = tmp.path().join("config");
    tokio::fs::create_dir_all(&ws).await.unwrap();
    tokio::fs::create_dir_all(&config_dir).await.unwrap();
    let config_path = config_dir.join("config.toml");
    tokio::fs::write(&config_path, b"").await.unwrap();

    // POST must never be reached: the ComputerUse runtime-config guard
    // rejects before any sidecar action request.
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let security = Arc::new(SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        workspace_dir: ws.clone(),
        allowed_roots: vec![ws.clone(), config_dir.clone()],
        config_path: Some(config_path.clone()),
        ..SecurityPolicy::default()
    });

    let mut config = test_computer_use_config();
    config.endpoint = server.uri();
    let tool = BrowserTool::new_with_backend(
        security,
        vec!["*".into()],
        None,
        "computer_use".into(),
        None,
        true,
        "http://127.0.0.1:9515".into(),
        None,
        config,
        Vec::new(),
    )
    .unwrap();

    let args = json!({
        "action": "screenshot",
        "path": config_path.to_string_lossy().to_string()
    });

    // Validation happens in execute_computer_use_action, returns ToolResult with error.
    let result = tool.execute(args).await.unwrap();
    assert!(!result.success, "Expected runtime-config rejection");
    let error = result.error.expect("Expected error in result");
    assert!(
        error.contains("runtime config") || error.contains("Refusing"),
        "Expected runtime-config rejection, got: {}",
        error
    );
}

#[tokio::test]
#[cfg(unix)]
async fn computer_use_dispatch_rejects_symlink_target_before_sidecar() {
    use std::os::unix::fs::symlink;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let outside = tmp.path().join("outside");
    tokio::fs::create_dir_all(&ws).await.unwrap();
    tokio::fs::create_dir_all(&outside).await.unwrap();

    // Create a symlink inside the workspace pointing outside.
    let link_path = ws.join("page.png");
    let target_path = outside.join("real.txt");
    tokio::fs::write(&target_path, b"real").await.unwrap();
    symlink(&target_path, &link_path).unwrap();

    // POST must never be reached: the ComputerUse symlink-target guard
    // rejects before any sidecar action request.
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let security = Arc::new(SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        workspace_dir: ws.clone(),
        allowed_roots: vec![ws.clone()],
        ..SecurityPolicy::default()
    });

    let mut config = test_computer_use_config();
    config.endpoint = server.uri();
    let tool = BrowserTool::new_with_backend(
        security,
        vec!["*".into()],
        None,
        "computer_use".into(),
        None,
        true,
        "http://127.0.0.1:9515".into(),
        None,
        config,
        Vec::new(),
    )
    .unwrap();

    let args = json!({
        "action": "screenshot",
        "path": "page.png"
    });

    // Validation happens in execute_computer_use_action, returns ToolResult with error.
    let result = tool.execute(args).await.unwrap();
    assert!(!result.success, "Expected symlink-target rejection");
    let error = result.error.expect("Expected error in result");
    assert!(
        error.contains("symlink"),
        "Expected symlink-target rejection, got: {}",
        error
    );
}

#[tokio::test]
async fn computer_use_dispatch_writes_validated_png_locally_without_forwarding_path() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    tokio::fs::create_dir_all(&ws).await.unwrap();

    // Create the page.png file so canonicalize succeeds
    let page_path = ws.join("page.png");
    tokio::fs::write(&page_path, b"test").await.unwrap();

    // Mock the reachability check (GET request)
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    // Mock the POST endpoint to return PNG data
    Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "data": {"png_base64": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="}
            })))
            .expect(1)
            .mount(&server)
            .await;

    // Setup security policy that allows the temp directory
    let security = Arc::new(SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        workspace_dir: ws.clone(),
        allowed_roots: vec![tmp.path().to_path_buf()], // Allow the entire temp directory
        ..SecurityPolicy::default()
    });

    let mut config = test_computer_use_config();
    config.endpoint = server.uri();
    let tool = BrowserTool::new_with_backend(
        security,
        vec!["*".into()],
        None,
        "computer_use".into(),
        None,
        true,
        "http://127.0.0.1:9515".into(),
        None,
        config,
        Vec::new(),
    )
    .unwrap();

    // Use absolute path to the created file
    let args = json!({
        "action": "screenshot",
        "path": page_path.to_string_lossy().to_string()
    });

    // Should succeed - path is validated locally but NOT forwarded to the
    // sidecar. The sidecar returns PNG bytes and ZeroClaw performs the
    // validated local write.
    let result = tool.execute(args).await.unwrap();
    assert!(
        result.success,
        "Expected success, got error: {:?}",
        result.error
    );

    // The fail-closed contract: exactly one sidecar action request, and the
    // destination path is absent from its params.
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let params = body.get("params").unwrap().as_object().unwrap();
    assert!(
        !params.contains_key("path"),
        "Path should not be forwarded to sidecar"
    );

    // ZeroClaw performed the validated local write: the pre-existing file
    // was overwritten with the decoded PNG bytes from the sidecar.
    let expected_png = base64::engine::general_purpose::STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==")
            .unwrap();
    let written = tokio::fs::read(&page_path).await.unwrap();
    assert_eq!(
        written, expected_png,
        "local screenshot write must match the sidecar PNG"
    );
}

#[tokio::test]
async fn computer_use_dispatch_does_not_forward_path_and_writes_local_target() {
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Positive remote-sidecar contract: the validated destination is NOT
    // transmitted to the sidecar (the path is removed before the request),
    // and the returned PNG is written only to the validated local target.
    // A non-loopback sidecar address exercises the same flow — the old
    // filesystem-sharing rejection (endpoint_is_remote_filesystem) is gone.
    let tmp = tempfile::TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    tokio::fs::create_dir_all(&ws).await.unwrap();

    let server = MockServer::start().await;
    // The sidecar must NOT receive a `path` field in the screenshot params.
    Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(
                serde_json::json!({"action": "screenshot"}),
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "success": true,
                    "data": { "png_base64": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==" }
                })),
            )
            .mount(&server)
            .await;

    let mut config = test_computer_use_config();
    config.endpoint = server.uri();

    let security = Arc::new(SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        workspace_dir: ws.clone(),
        allowed_roots: vec![ws.clone()],
        ..SecurityPolicy::default()
    });
    let tool = BrowserTool::new_with_backend(
        security,
        vec!["*".into()],
        None,
        "computer_use".into(),
        None,
        true,
        "http://127.0.0.1:9515".into(),
        None,
        config,
        Vec::new(),
    )
    .unwrap();

    let result = tool
        .execute(json!({
            "action": "screenshot",
            "path": "screenshot.png"
        }))
        .await
        .expect("execute must succeed");
    assert!(
        result.success,
        "a valid screenshot from the sidecar must write the validated local target: {:?}",
        result.error
    );

    // The local target was written with the PNG bytes.
    let written = tokio::fs::read(ws.join("screenshot.png"))
        .await
        .expect("the validated local target must be written");
    assert!(
        written.starts_with(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']),
        "the written bytes must be a PNG, not arbitrary decoded data"
    );

    // The destination was not transmitted: every sidecar request body must
    // be free of a `path` field.
    let requests = server.received_requests().await.expect("infallible");
    for req in &requests {
        let body: serde_json::Value = req.body_json().expect("request body is JSON");
        assert!(
            body.get("params").and_then(|p| p.get("path")).is_none(),
            "the validated destination must NOT be forwarded to the sidecar: {body}"
        );
    }
}

/// Fail-closed contract for a path-bearing screenshot: the tool must NOT
/// report success (or write the destination) unless the sidecar returned a
/// well-formed ComputerUseResponse with success != false and a valid
/// non-empty PNG payload. A malformed or unsuccessful 2xx body must fail
/// and leave the destination unwritten.
#[tokio::test]
async fn computer_use_dispatch_fails_closed_on_malformed_screenshot_response() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let not_a_png_b64 = base64::engine::general_purpose::STANDARD.encode(b"not a png");
    // Each case is (name, wire body, expected error fragment). The
    // non-JSON case sends genuinely non-JSON bytes via `set_body_raw`
    // (a `set_body_json(json!("..."))` would transmit a *valid* JSON
    // string, exercising a different branch). Every case is a 200 so the
    // failure must come from response handling, not the HTTP layer.
    let cases: Vec<(&str, ResponseTemplate, &str)> = vec![
        (
            "non-json-2xx",
            ResponseTemplate::new(200).set_body_raw(b"this is not json {{{".to_vec(), "text/plain"),
            "non-JSON",
        ),
        (
            "success-false",
            ResponseTemplate::new(200).set_body_json(json!({"success": false, "error": "boom"})),
            "boom",
        ),
        (
            "empty-base64",
            ResponseTemplate::new(200)
                .set_body_json(json!({"success": true, "data": {"png_base64": ""}})),
            "empty screenshot payload",
        ),
        (
            "non-png-bytes",
            ResponseTemplate::new(200)
                .set_body_json(json!({"success": true, "data": {"png_base64": not_a_png_b64}})),
            "non-PNG screenshot payload",
        ),
    ];

    for (name, response_template, expected_error_fragment) in cases {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        tokio::fs::create_dir_all(&ws).await.unwrap();

        // Reachability probe (GET) so the action POST is actually issued.
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        // The action POST must happen exactly once. If a pre-dispatch
        // failure short-circuits before the sidecar request, the POST
        // never fires and the test would otherwise pass on an unwritten
        // file alone — so the exact-once expectation makes that impossible.
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(response_template)
            .expect(1)
            .mount(&server)
            .await;

        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            workspace_dir: ws.clone(),
            allowed_roots: vec![ws.clone()],
            ..SecurityPolicy::default()
        });
        let mut config = test_computer_use_config();
        config.endpoint = server.uri();
        let tool = BrowserTool::new_with_backend(
            security,
            vec!["*".into()],
            None,
            "computer_use".into(),
            None,
            true,
            "http://127.0.0.1:9515".into(),
            None,
            config,
            Vec::new(),
        )
        .unwrap();

        let target = ws.join("shot.png");
        let result = tool
            .execute(json!({
                "action": "screenshot",
                "path": "shot.png"
            }))
            .await;

        // A malformed/unsuccessful sidecar response must fail the tool:
        // either as an Ok(success=false) ToolResult or as an Err — never a
        // success. Each shape must surface its own expected error, and the
        // destination must remain unwritten.
        let error_text = match &result {
            Ok(r) => {
                assert!(
                    !r.success,
                    "{name}: must fail closed, got success with output {:?}",
                    r.output
                );
                r.error.clone().unwrap_or_default()
            }
            Err(e) => e.to_string(),
        };
        assert!(
            error_text.contains(expected_error_fragment),
            "{name}: expected error containing {expected_error_fragment:?}, got: {error_text}"
        );
        assert!(
            !tokio::fs::try_exists(&target)
                .await
                .expect("filesystem must be readable"),
            "{name}: the destination must NOT be written on a failed screenshot"
        );
    }
}

#[tokio::test]
async fn computer_use_dispatch_rejects_non_string_path_before_sidecar() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Start a mock server to pass the endpoint reachability check in resolve_backend()
    let server = MockServer::start().await;

    // Mock the reachability check (GET request) - should return 200
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    // Mock the screenshot action (POST request) - should NOT be called because
    // path validation happens before the sidecar request. Exact zero is
    // asserted so a regression that forwards a non-string path fails here.
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "data": {"ok": true}
        })))
        .expect(0)
        .mount(&server)
        .await;

    let mut config = test_computer_use_config();
    config.endpoint = server.uri();
    let tool = browser_tool_with_computer_use(config);

    // Integer path - should fail before reaching sidecar
    let args = json!({
        "action": "screenshot",
        "path": 12345
    });
    // Validation happens in execute_computer_use_action, returns ToolResult with error
    let result = tool.execute(args).await.unwrap();
    assert!(!result.success, "Expected validation to fail");
    let error = result.error.expect("Expected error in result");
    assert!(
        error.contains("string") || error.contains("path"),
        "Expected non-string path error, got: {}",
        error
    );

    // Array path
    let args = json!({
        "action": "screenshot",
        "path": ["path1", "path2"]
    });
    let result = tool.execute(args).await.unwrap();
    assert!(!result.success, "Expected validation to fail");
    let error = result.error.expect("Expected error in result");
    assert!(
        error.contains("string") || error.contains("path"),
        "Expected non-string path error, got: {}",
        error
    );

    // Object path
    let args = json!({
        "action": "screenshot",
        "path": {"key": "value"}
    });
    let result = tool.execute(args).await.unwrap();
    assert!(!result.success, "Expected validation to fail");
    let error = result.error.expect("Expected error in result");
    assert!(
        error.contains("string") || error.contains("path"),
        "Expected non-string path error, got: {}",
        error
    );
}

#[tokio::test]
async fn computer_use_dispatch_passes_through_empty_string_path() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    // Empty string path → inline PNG semantics, no path validation, forwarded to sidecar
    Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "data": {"png_base64": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="}
            })))
            .expect(1)
            .mount(&server)
            .await;

    let mut config = test_computer_use_config();
    config.endpoint = server.uri();
    let tool = browser_tool_with_computer_use(config);

    // Empty string path → inline PNG, no local write
    let args = json!({
        "action": "screenshot",
        "path": ""
    });

    let result = tool.execute(args).await.unwrap();
    assert!(
        result.success,
        "Expected success, got error: {:?}",
        result.error
    );
}

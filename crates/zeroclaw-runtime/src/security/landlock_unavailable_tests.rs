use super::*;
use crate::platform::NativeRuntime;
use crate::security::detect::{create_sandbox, sandbox_posture};
use crate::security::{AutonomyLevel, SecurityPolicy};
use crate::tools::{ShellTool, Tool};
use serde_json::json;
use std::process::Command;
use std::sync::Arc;
use zeroclaw_config::schema::{RiskProfileConfig, SandboxBackend, SandboxConfig};

const CHILD_MODE: &str = "ZEROCLAW_TEST_LANDLOCK_UNAVAILABLE_ERRNO";
const CHILD_COMPLETE: &str = "landlock unavailable fixture completed";

#[test]
fn landlock_unavailable_enosys_rejects_probe_and_spawn() {
    run_isolated(
        "security::landlock::unavailable_tests::landlock_unavailable_enosys_rejects_probe_and_spawn",
        libc::ENOSYS,
    );
}

#[test]
fn landlock_unavailable_disabled_rejects_probe_and_spawn() {
    run_isolated(
        "security::landlock::unavailable_tests::landlock_unavailable_disabled_rejects_probe_and_spawn",
        libc::EOPNOTSUPP,
    );
}

#[test]
fn landlock_enforcement_accepts_full_and_partial_but_rejects_none() {
    assert!(LandlockSandbox::require_enforcement(RulesetStatus::FullyEnforced).is_ok());
    assert!(LandlockSandbox::require_enforcement(RulesetStatus::PartiallyEnforced).is_ok());
    let error = LandlockSandbox::require_enforcement(RulesetStatus::NotEnforced).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::EOPNOTSUPP));
}

fn run_isolated(test_name: &str, errno: i32) {
    let mode = errno.to_string();
    if std::env::var(CHILD_MODE).as_deref() == Ok(mode.as_str()) {
        assert_unavailable_rejects_probe_and_spawn(errno);
        println!("{CHILD_COMPLETE}");
        return;
    }

    // Seccomp is irreversible. Re-exec only this fixture so neither the
    // normal test runner nor any concurrently running test inherits it.
    let output = Command::new(std::env::current_exe().expect("locate test executable"))
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(CHILD_MODE, mode)
        .output()
        .expect("run isolated Landlock fixture");
    assert!(
        output.status.success(),
        "isolated fixture failed: status={:?}, stdout={}, stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(CHILD_COMPLETE),
        "the isolated child must actually execute the selected fixture"
    );
}

fn assert_unavailable_rejects_probe_and_spawn(errno: i32) {
    let directory = tempfile::TempDir::new().expect("create fixture directory");
    let control = directory.path().join("unwrapped-command-ran");
    let denied = directory.path().join("sandboxed-command-ran");

    // Model an instance retained before Landlock becomes unavailable. This
    // also exercises the enforcement boundary on hosts with no native
    // Landlock support, independently of the constructor's stricter probe.
    let sandbox = LandlockSandbox {
        workspace_dir: None,
    };
    deny_landlock_create_ruleset(errno);

    // Query the real syscall, proving that the fixture installed the intended
    // failure rather than relying on the runner's kernel configuration.
    let version = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            1u32, // LANDLOCK_CREATE_RULESET_VERSION
        )
    };
    assert_eq!(version, -1);
    assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(errno));

    for result in [
        LandlockSandbox::new(),
        LandlockSandbox::probe(),
        LandlockSandbox::with_workspace(Some(directory.path().to_path_buf())),
    ] {
        assert_eq!(
            result
                .expect_err("unavailable Landlock must not be admitted")
                .kind(),
            std::io::ErrorKind::Unsupported,
        );
    }
    assert!(!sandbox.is_available());

    // Positive control after seccomp installation: exec and writing this
    // path still work. Only the production sandbox wrapper must prevent it.
    let status = marker_command(&control)
        .status()
        .expect("run unwrapped control command");
    assert!(status.success());
    assert_eq!(std::fs::read_to_string(&control).unwrap(), "executed");

    let mut command = marker_command(&denied);
    sandbox
        .wrap_command(&mut command)
        .expect("best-effort ruleset construction does not enforce it");
    let error = command
        .output()
        .expect_err("pre_exec must refuse a ruleset that is not enforced");
    assert_eq!(error.raw_os_error(), Some(libc::EOPNOTSUPP));
    assert!(!denied.exists(), "the sandboxed command must not execute");

    assert_factory_blocks_unavailable_shell(directory.path());
}

fn assert_factory_blocks_unavailable_shell(workspace: &Path) {
    let profile = RiskProfileConfig {
        sandbox_enabled: Some(true),
        sandbox_backend: Some("landlock".to_owned()),
        ..RiskProfileConfig::default()
    };
    let config = profile.sandbox_config();
    let sandbox = create_sandbox(&config, "native", Some(workspace));
    assert!(!sandbox.is_available());
    assert_eq!(sandbox.name(), "blocked");
    let posture = sandbox_posture(&config, "native", Some(workspace));
    assert_eq!(posture.requested_backend, "landlock");
    assert_eq!(posture.active_backend, sandbox.name());
    assert_eq!(posture.active_description, sandbox.description());
    assert!(!posture.fallback);

    let security = Arc::new(SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        workspace_dir: workspace.to_path_buf(),
        allowed_commands: vec!["*".to_owned()],
        block_high_risk_commands: false,
        ..SecurityPolicy::default()
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build shell fixture runtime");
    let tool = ShellTool::new_with_sandbox(
        security.clone(),
        Arc::new(NativeRuntime::new()),
        sandbox.clone(),
    );
    let error = runtime
        .block_on(tool.execute(json!({
            "command": "printf executed > factory-denied",
            "approved": true,
        })))
        .expect_err("the production shell tool must propagate factory denial");
    assert!(error.to_string().contains(sandbox.description()));
    assert!(!workspace.join("factory-denied").exists());

    // Intentional disabling remains a positive execution path, even with an
    // unavailable backend. These use the same factory and ShellTool consumer.
    for (config, marker) in [
        (
            SandboxConfig {
                enabled: Some(false),
                backend: SandboxBackend::Landlock,
                ..SandboxConfig::default()
            },
            "explicitly-disabled",
        ),
        (
            SandboxConfig {
                enabled: Some(true),
                backend: SandboxBackend::None,
                ..SandboxConfig::default()
            },
            "explicit-none",
        ),
    ] {
        let sandbox = create_sandbox(&config, "native", Some(workspace));
        assert_eq!(sandbox.name(), "none");
        let tool =
            ShellTool::new_with_sandbox(security.clone(), Arc::new(NativeRuntime::new()), sandbox);
        let result = runtime
            .block_on(tool.execute(json!({
                "command": format!("printf executed > {marker}"),
                "approved": true,
            })))
            .expect("an intentionally disabled sandbox permits the control");
        assert!(result.success, "{result:?}");
        assert_eq!(
            std::fs::read_to_string(workspace.join(marker)).unwrap(),
            "executed"
        );
    }

    let automatic = create_sandbox(&SandboxConfig::default(), "native", Some(workspace));
    assert_ne!(automatic.name(), "blocked");
    assert_ne!(automatic.name(), "landlock");
}

fn marker_command(path: &Path) -> Command {
    let mut command = Command::new("sh");
    command
        .args(["-c", "printf executed > \"$1\"", "landlock-fixture"])
        .arg(path);
    command
}

fn deny_landlock_create_ruleset(errno: i32) {
    let mut filter = [
        // Load the native syscall number from struct seccomp_data.
        instruction(
            libc::BPF_LD | libc::BPF_W | libc::BPF_ABS,
            u32::try_from(std::mem::offset_of!(libc::seccomp_data, nr)).unwrap(),
        ),
        libc::sock_filter {
            code: u16::try_from(libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K).unwrap(),
            jt: 0,
            jf: 1,
            k: u32::try_from(libc::SYS_landlock_create_ruleset).unwrap(),
        },
        instruction(
            libc::BPF_RET | libc::BPF_K,
            libc::SECCOMP_RET_ERRNO | u32::try_from(errno).unwrap(),
        ),
        instruction(libc::BPF_RET | libc::BPF_K, libc::SECCOMP_RET_ALLOW),
    ];
    let program = libc::sock_fprog {
        len: u16::try_from(filter.len()).unwrap(),
        filter: filter.as_mut_ptr(),
    };

    // SAFETY: This runs only inside the isolated fixture process. The kernel
    // copies the complete filter while the stack array remains alive. The
    // filter returns errno only for Landlock ruleset creation and allows all
    // other native syscalls; it never relaxes an existing restriction.
    let no_new_privs = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    assert_eq!(no_new_privs, 0, "{}", std::io::Error::last_os_error());
    let installed = unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER,
            &program,
            0,
            0,
        )
    };
    assert_eq!(installed, 0, "{}", std::io::Error::last_os_error());
}

fn instruction(code: u32, value: u32) -> libc::sock_filter {
    libc::sock_filter {
        code: u16::try_from(code).unwrap(),
        jt: 0,
        jf: 0,
        k: value,
    }
}

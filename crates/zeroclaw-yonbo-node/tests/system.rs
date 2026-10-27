#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn process_protocol_and_lifecycle() {
    let output = std::process::Command::new("python3")
        .args([
            "-B",
            "tests/system/protocol.py",
            env!("CARGO_BIN_EXE_yonbo-node"),
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("Python 3 is required for the process protocol oracle");
    print!("{}", String::from_utf8_lossy(&output.stdout));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

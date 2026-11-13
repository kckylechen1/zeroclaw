use std::{fs, path::Path, process::Command};

#[test]
fn replay_cli_reports_suite_and_fails_on_failed_expectation() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals");
    let binary = env!("CARGO_BIN_EXE_zeroclaw-eval");
    let output = Command::new(binary)
        .args(["run", "--suite"])
        .arg(&fixtures)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["all_passed"], true);
    assert!(report["total"].as_u64().unwrap() > 0);

    let suite = tempfile::tempdir().unwrap();
    let original = fs::read_to_string(fixtures.join("smoke_greeting.json")).unwrap();
    let failing = original.replace(
        "\"response_contains\": [\"Hello\"]",
        "\"response_contains\": [\"impossible result\"]",
    );
    assert_ne!(original, failing);
    fs::write(suite.path().join("failed.json"), failing).unwrap();
    let output = Command::new(binary)
        .args(["run", "--suite"])
        .arg(suite.path())
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["failed"], 1);
}

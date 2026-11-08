use super::*;
use std::path::Path;

fn limits() -> ProcessLimits {
    ProcessLimits {
        timeout: Duration::from_secs(5),
        stdout_bytes: 1024,
        stderr_bytes: 1024,
        stdout_lines: None,
        overflow: OverflowBehavior::Drain,
    }
}

fn shell(script: &str) -> Command {
    let mut command = Command::new("sh");
    command.arg("-c").arg(script);
    command
}

async fn wait_for_file(path: &Path) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("fixture child must start");
}

#[tokio::test]
async fn captures_both_pipes_and_preserves_exit_status() {
    let output = run_bounded(
        &mut shell("printf 'hello'; printf 'diagnostic' >&2; exit 7"),
        limits(),
    )
    .await
    .unwrap();
    assert!(matches!(output.outcome, ProcessOutcome::Exited(status) if status.code() == Some(7)));
    assert_eq!(output.stdout, b"hello");
    assert_eq!(output.stderr, b"diagnostic");
    assert!(!output.stdout_truncated);
    assert!(!output.stderr_truncated);
}

#[tokio::test]
async fn draining_caps_both_pipes_without_cancelling_a_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let committed = dir.path().join("committed");
    let mut command = shell(
        "i=0; while test $i -lt 4096; do printf x; printf y >&2; i=$((i+1)); done; printf done > \"$1\"",
    );
    command.arg("fixture").arg(&committed);
    let output = run_bounded(&mut command, limits()).await.unwrap();
    assert!(matches!(output.outcome, ProcessOutcome::Exited(status) if status.success()));
    assert_eq!(output.stdout.len(), 1024);
    assert_eq!(output.stderr.len(), 1024);
    assert!(output.stdout_truncated);
    assert!(output.stderr_truncated);
    assert!(
        committed.exists(),
        "capture exhaustion must not cancel writes"
    );
}

#[tokio::test]
async fn read_only_producer_stops_at_the_line_budget() {
    let output = run_bounded(
        &mut shell("while :; do printf 'match\\n'; done"),
        ProcessLimits {
            stdout_lines: Some(1),
            overflow: OverflowBehavior::Terminate,
            ..limits()
        },
    )
    .await
    .unwrap();
    assert!(matches!(output.outcome, ProcessOutcome::OutputLimit));
    assert_eq!(output.stdout, b"match\n");
    assert!(output.stdout_truncated);
}

#[tokio::test]
async fn byte_budget_stops_output_without_a_newline() {
    let output = run_bounded(
        &mut shell("while :; do printf 'xxxxxxxxxxxxxxxx'; done"),
        ProcessLimits {
            stdout_bytes: 37,
            overflow: OverflowBehavior::Terminate,
            ..limits()
        },
    )
    .await
    .unwrap();
    assert!(matches!(output.outcome, ProcessOutcome::OutputLimit));
    assert_eq!(output.stdout.len(), 37);
}

#[tokio::test]
async fn timeout_kills_the_owned_descendant_group() {
    let dir = tempfile::tempdir().unwrap();
    let escaped = dir.path().join("escaped");
    let started = dir.path().join("started");
    let release = dir.path().join("release");
    let mut command = shell(
        "(while ! test -e \"$3\"; do sleep 0.01; done; printf escaped > \"$1\") & printf ready > \"$2\"; wait",
    );
    command
        .arg("fixture")
        .arg(&escaped)
        .arg(&started)
        .arg(&release);
    let output = run_bounded(
        &mut command,
        ProcessLimits {
            timeout: Duration::from_secs(1),
            ..limits()
        },
    )
    .await
    .unwrap();
    assert!(started.exists(), "fixture must start before its deadline");
    assert!(matches!(output.outcome, ProcessOutcome::TimedOut));
    std::fs::write(&release, b"continue").unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!escaped.exists(), "descendant survived the timeout");
}

#[tokio::test]
async fn dropping_the_running_future_kills_descendants() {
    let dir = tempfile::tempdir().unwrap();
    let escaped = dir.path().join("escaped");
    let started = dir.path().join("started");
    let release = dir.path().join("release");
    let mut command = shell(
        "(while ! test -e \"$3\"; do sleep 0.01; done; printf escaped > \"$1\") & printf ready > \"$2\"; wait",
    );
    command
        .arg("fixture")
        .arg(&escaped)
        .arg(&started)
        .arg(&release);
    let task = zeroclaw_spawn::spawn!(async move { run_bounded(&mut command, limits()).await });
    wait_for_file(&started).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    std::fs::write(&release, b"continue").unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!escaped.exists(), "descendant survived future cancellation");
}

#[tokio::test]
async fn normal_exit_with_inherited_pipes_has_a_bounded_drain() {
    let output = tokio::time::timeout(
        Duration::from_secs(2),
        run_bounded(&mut shell("sleep 1 & printf done"), limits()),
    )
    .await
    .expect("a descendant retaining stdout cannot keep the owner waiting")
    .unwrap();
    assert!(matches!(output.outcome, ProcessOutcome::Exited(status) if status.success()));
    assert_eq!(output.stdout, b"done");
    assert!(output.stdout_truncated || output.stderr_truncated);
}

#[tokio::test]
async fn spawn_failure_remains_an_io_error() {
    let dir = tempfile::tempdir().unwrap();
    let error = run_bounded(&mut Command::new(dir.path().join("missing")), limits())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
}

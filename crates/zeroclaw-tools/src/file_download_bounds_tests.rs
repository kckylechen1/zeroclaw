use super::*;
use crate::helpers::http_body::test_support::stalled_response;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use zeroclaw_config::autonomy::AutonomyLevel;

fn tool(workspace: &Path, url: &str, timeout_secs: u64) -> FileDownloadTool {
    FileDownloadTool::new(
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            workspace_dir: workspace.to_owned(),
            max_actions_per_hour: 100,
            ..SecurityPolicy::default()
        }),
        FileDownloadConfig {
            url: Some(url.into()),
            timeout_secs,
            ..FileDownloadConfig::default()
        },
    )
}

fn parts(workspace: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(workspace)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .contains(".part-")
        })
        .collect()
}

#[tokio::test]
async fn cancelling_execute_removes_owned_partial_and_preserves_existing_files() {
    let server = stalled_response("200 OK", Some(4096), b"partial bytes".to_vec()).await;
    let workspace = TempDir::new().unwrap();
    std::fs::write(
        workspace.path().join("out.bin"),
        "previous complete download",
    )
    .unwrap();
    let other_partial = workspace.path().join(".other.part-fixture");
    std::fs::write(&other_partial, "another owner").unwrap();
    let tool = tool(workspace.path(), &server.url, 30);
    let task = ::zeroclaw_spawn::spawn!(async move {
        tool.execute(json!({"document_id":"fixture", "dest_path":"out.bin"}))
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if parts(workspace.path()).iter().any(|path| {
                path != &other_partial && path.metadata().is_ok_and(|meta| meta.len() > 0)
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("download must begin streaming before cancellation");
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(parts(workspace.path()), vec![other_partial.clone()]);
    assert_eq!(
        std::fs::read_to_string(other_partial).unwrap(),
        "another owner"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("out.bin")).unwrap(),
        "previous complete download"
    );
}

#[tokio::test]
async fn body_timeout_removes_partial_without_publishing_destination() {
    let server = stalled_response("200 OK", Some(4096), b"partial bytes".to_vec()).await;
    let workspace = TempDir::new().unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        tool(workspace.path(), &server.url, 1)
            .execute(json!({"document_id":"fixture", "dest_path":"out.bin"})),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!result.success);
    assert!(parts(workspace.path()).is_empty());
    assert!(!workspace.path().join("out.bin").exists());
}

#[tokio::test]
async fn oversized_error_preview_finishes_before_body_eof() {
    let prefix = format!("{}{}", "x".repeat(4094), "世界".repeat(100)).into_bytes();
    let server = stalled_response("500 Internal Server Error", Some(1_000_000), prefix).await;
    let workspace = TempDir::new().unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        tool(workspace.path(), &server.url, 30)
            .execute(json!({"document_id":"fixture", "dest_path":"out.bin"})),
    )
    .await
    .expect("bounded error preview must not wait for EOF")
    .unwrap();
    assert!(!result.success);
    assert!(result.error.as_deref().unwrap().contains("500"));
    assert!(result.output.contains("[truncated]"));
    assert!(result.output.len() < RESPONSE_BODY_LIMIT_BYTES + 64);
    assert!(parts(workspace.path()).is_empty());
    assert!(!workspace.path().join("out.bin").exists());
}

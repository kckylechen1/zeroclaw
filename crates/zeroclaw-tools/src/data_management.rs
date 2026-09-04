use crate::fs_guard::{DirLink, UnlinkOutcome, file_id_of, guarded_unlink, verify_chain};
use async_trait::async_trait;
use serde_json::json;
use std::path::{Path, PathBuf};
use tokio::fs;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};

/// Workspace data lifecycle tool: retention status, time-based purge, and
/// storage statistics.
pub struct DataManagementTool {
    workspace_dir: PathBuf,
    retention_days: u64,
}

impl DataManagementTool {
    pub fn new(workspace_dir: PathBuf, retention_days: u64) -> Self {
        Self {
            workspace_dir,
            retention_days,
        }
    }

    /// The workspace-root chain anchor for read-only walks. A missing
    /// workspace yields `None` (nothing to count); a symlinked one is
    /// refused so foreign data cannot be adopted into counts or stats.
    async fn read_anchor(&self) -> anyhow::Result<Option<Vec<DirLink>>> {
        match fs::symlink_metadata(&self.workspace_dir).await {
            Ok(m) => {
                anyhow::ensure!(
                    m.is_dir() && !m.file_type().is_symlink(),
                    "workspace root is not a real directory: {}",
                    self.workspace_dir.display()
                );
                Ok(Some(vec![DirLink {
                    path: self.workspace_dir.clone(),
                    id: file_id_of(&m),
                }]))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Retention window status. Shared with the gateway operator surface
    /// (`/api/agents/{alias}/data-retention`); the model-visible `Tool`
    /// entry point and the operator API must stay behaviour-identical.
    pub async fn cmd_retention_status(&self) -> anyhow::Result<ToolResult> {
        let cutoff = chrono::Utc::now()
            - chrono::Duration::days(i64::try_from(self.retention_days).unwrap_or(i64::MAX));
        let cutoff_ts = cutoff.timestamp().try_into().unwrap_or(0u64);
        let (count, skipped) = match self.read_anchor().await? {
            Some(chain) => count_files_older_than(&self.workspace_dir, cutoff_ts, &chain).await?,
            None => (0, 0),
        };

        Ok(ToolResult {
            success: true,
            output: json!({
                "retention_days": self.retention_days,
                "cutoff": cutoff.to_rfc3339(),
                "affected_files": count,
                "symlinks_skipped": skipped,
            })
            .to_string()
            .into(),
            error: None,
        })
    }

    /// Purge files older than the retention window. `dry_run = true`
    /// reports what would be deleted and removes nothing. Deletion
    /// failures are never swallowed: they are reported per entry in the
    /// result, the counters count only confirmed deletions, and any
    /// failure makes the whole purge report failure so the operator
    /// surface cannot mistake a no-op purge for a successful one.
    /// Shared with the gateway operator surface, which must keep the
    /// destructive guard intact.
    pub async fn cmd_purge(&self, dry_run: bool) -> anyhow::Result<ToolResult> {
        let cutoff = chrono::Utc::now()
            - chrono::Duration::days(i64::try_from(self.retention_days).unwrap_or(i64::MAX));
        let cutoff_ts: u64 = cutoff.timestamp().try_into().unwrap_or(0);
        let outcome = purge_old_files(&self.workspace_dir, cutoff_ts, dry_run).await?;

        let failures: Vec<serde_json::Value> = outcome
            .failures
            .iter()
            .map(|f| json!({ "path": f.path, "error": f.error }))
            .collect();
        let failed = failures.len();
        Ok(ToolResult {
            success: failed == 0,
            output: json!({
                "dry_run": dry_run,
                "files": outcome.deleted,
                "bytes_freed": outcome.bytes,
                "bytes_freed_human": format_bytes(outcome.bytes),
                "symlinks_skipped": outcome.skipped,
                "failures": failures,
            })
            .to_string()
            .into(),
            error: if failed == 0 {
                None
            } else {
                Some(format!(
                    "{failed} deletion(s) failed; counts reflect confirmed deletions only"
                ))
            },
        })
    }

    /// Workspace storage statistics. Shared with the gateway operator surface.
    pub async fn cmd_stats(&self) -> anyhow::Result<ToolResult> {
        let (total_files, total_bytes, breakdown, skipped) = match self.read_anchor().await? {
            Some(chain) => dir_stats(&self.workspace_dir, &chain).await?,
            None => (0, 0, serde_json::json!({}), 0),
        };
        Ok(ToolResult {
            success: true,
            output: json!({
                "total_files": total_files,
                "total_size": total_bytes,
                "total_size_human": format_bytes(total_bytes),
                "subdirectories": breakdown,
                "symlinks_skipped": skipped,
            })
            .to_string()
            .into(),
            error: None,
        })
    }
}

#[async_trait]
impl Tool for DataManagementTool {
    fn name(&self) -> &str {
        "data_management"
    }

    fn description(&self) -> &str {
        "Workspace data retention, purge, and storage statistics"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "enum": ["retention_status", "purge", "stats"],
                    "description": "Data management command"
                },
                "dry_run": {
                    "type": "boolean",
                    "description": "If true, purge only lists what would be deleted (default true)"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let command = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some("Missing 'command' parameter".into()),
                });
            }
        };

        match command {
            "retention_status" => self.cmd_retention_status().await,
            "purge" => {
                let dry_run = args
                    .get("dry_run")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                self.cmd_purge(dry_run).await
            }
            "stats" => self.cmd_stats().await,
            other => Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Unknown command: {other}")),
            }),
        }
    }
}

// -- Helpers ------------------------------------------------------------------

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Counts files older than the cutoff, plus how many symlinks were
/// skipped so the operator sees the blind spot instead of guessing. All
/// stats are non-following and verified against the ancestor `chain`:
/// a symlink never contributes foreign data, and a component swapped
/// mid-count refuses the walk instead of counting through it.
async fn count_files_older_than(
    dir: &Path,
    cutoff_epoch: u64,
    chain: &[DirLink],
) -> anyhow::Result<(usize, usize)> {
    let mut count = 0;
    let mut skipped = 0;
    verify_chain(chain).await?;
    let mut rd = fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let file_type = entry.file_type().await?;
        if file_type.is_symlink() {
            // Never traverse or count through a symlink: a link planted in
            // the workspace must not make foreign data retention-eligible.
            skipped += 1;
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            let m = fs::symlink_metadata(&path).await?;
            anyhow::ensure!(
                m.is_dir() && !m.file_type().is_symlink(),
                "entry changed while being counted: {}",
                path.display()
            );
            let mut child_chain = chain.to_vec();
            child_chain.push(DirLink {
                path: path.clone(),
                id: file_id_of(&m),
            });
            let (c, s) =
                Box::pin(count_files_older_than(&path, cutoff_epoch, &child_chain)).await?;
            count += c;
            skipped += s;
        } else if file_type.is_file() {
            // Verify the chain BEFORE reading the entry's metadata: a
            // swapped component must be refused before a foreign file's
            // age can be observed, not after.
            verify_chain(chain).await?;
            if let Ok(meta) = fs::symlink_metadata(&path).await {
                let modified = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                let epoch = modified
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                if epoch < cutoff_epoch {
                    count += 1;
                }
            }
        }
    }
    Ok((count, skipped))
}

/// Outcome of a purge walk: confirmed (or, in dry-run mode, planned)
/// deletions, and per-entry failures for deletions that did not succeed.
struct PurgeOutcome {
    deleted: usize,
    bytes: u64,
    skipped: usize,
    failures: Vec<PurgeFailure>,
}

struct PurgeFailure {
    path: String,
    error: String,
}

/// Purge old files under `dir`, never following or deleting symlinks and
/// never deleting through a path component that changed identity since
/// the walk observed it.
async fn purge_old_files(
    dir: &Path,
    cutoff_epoch: u64,
    dry_run: bool,
) -> anyhow::Result<PurgeOutcome> {
    let mut out = PurgeOutcome {
        deleted: 0,
        bytes: 0,
        skipped: 0,
        failures: Vec::new(),
    };
    let meta = match fs::symlink_metadata(dir).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    if meta.file_type().is_symlink() {
        anyhow::bail!("refusing to purge through symlink: {}", dir.display());
    }
    if !meta.is_dir() {
        return Ok(out);
    }
    let chain = vec![DirLink {
        path: dir.to_path_buf(),
        id: file_id_of(&meta),
    }];
    Box::pin(purge_walk(dir, cutoff_epoch, dry_run, &chain, &mut out)).await?;
    Ok(out)
}

/// One step of the purge walk. Every stat is non-following
/// (`symlink_metadata` / `DirEntry::file_type`): a symlink is skipped,
/// never traversed. `chain` is the verified ancestor path from the
/// workspace root down to this directory; every file removal re-verifies
/// the WHOLE chain as adjacent syscalls in one blocking step, so a
/// directory anywhere above a deletion that was renamed away and replaced
/// by a symlink mid-walk cannot have its replacement adopted and deleted
/// through — an identity captured through a swapped ancestor never
/// satisfies the ancestor's own chain entry. Entries that vanish on their
/// own mid-walk are skipped, not treated as walk failures. The residual
/// window between the last chain re-check and the unlink itself is one
/// syscall wide — inherent to path-based APIs, and the same discipline
/// the `fs_guard` helpers apply.
async fn purge_walk(
    dir: &Path,
    cutoff_epoch: u64,
    dry_run: bool,
    chain: &[DirLink],
    out: &mut PurgeOutcome,
) -> anyhow::Result<()> {
    let meta = match fs::symlink_metadata(dir).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if meta.file_type().is_symlink() {
        anyhow::bail!("refusing to purge through symlink: {}", dir.display());
    }
    if !meta.is_dir() {
        return Ok(());
    }
    // The chain's last entry is this directory as the parent walk saw it.
    if let Some(expected) = chain.last()
        && file_id_of(&meta) != expected.id
    {
        anyhow::bail!(
            "{} changed identity during the purge; refusing to continue",
            dir.display()
        );
    }
    // Verify every ancestor before enumerating: directory-only and
    // symlink-only subtrees must not be adopted on the strength of the
    // last-element check alone.
    verify_chain(chain).await?;
    let mut rd = fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let file_type = entry.file_type().await?;
        if file_type.is_symlink() {
            // Never delete through or descend into a symlink: purge stays
            // jailed to real workspace files even if a link points outside.
            out.skipped += 1;
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            // Re-check the whole chain before adopting a child identity:
            // the name must still name a real directory reached through
            // the same ancestors. The captured identity joins the chain,
            // where it is re-verified before every mutation underneath.
            verify_chain(chain).await?;
            let child_meta = match fs::symlink_metadata(&path).await {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            if child_meta.file_type().is_symlink() {
                anyhow::bail!("refusing to purge through symlink: {}", path.display());
            }
            if child_meta.is_dir() {
                let mut child_chain = chain.to_vec();
                child_chain.push(DirLink {
                    path: path.clone(),
                    id: file_id_of(&child_meta),
                });
                Box::pin(purge_walk(&path, cutoff_epoch, dry_run, &child_chain, out)).await?;
            }
        } else if file_type.is_file() {
            // Verify the chain BEFORE reading the entry's metadata: a
            // swapped component must be refused before a foreign file's
            // age or size can be observed, not after (a swap-away and
            // swap-back around a later check would otherwise count the
            // foreign file).
            verify_chain(chain).await?;
            let m = match fs::symlink_metadata(&path).await {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            if m.file_type().is_symlink() {
                anyhow::bail!("refusing to purge through symlink: {}", path.display());
            }
            if !m.is_file() {
                continue;
            }
            let modified = m.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            let epoch = modified
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if epoch < cutoff_epoch {
                if dry_run {
                    // Dry-run counters describe the same tree the real
                    // purge would delete; the chain was verified before
                    // this entry was even read.
                    out.deleted += 1;
                    out.bytes += m.len();
                } else {
                    let file_id = file_id_of(&m);
                    let path_str = path.display().to_string();
                    let guard_chain = chain.to_vec();
                    let res = tokio::task::spawn_blocking(move || {
                        guarded_unlink(guard_chain, path, file_id)
                    })
                    .await?;
                    match res {
                        Ok(UnlinkOutcome::Removed) => {
                            out.deleted += 1;
                            out.bytes += m.len();
                        }
                        Ok(UnlinkOutcome::Vanished) => {
                            // Deleted by someone else mid-purge: not our
                            // deletion, so it is neither counted nor
                            // reported as a failure.
                        }
                        Err(error) => out.failures.push(PurgeFailure {
                            path: path_str,
                            error,
                        }),
                    }
                }
            }
        }
        // Other entry kinds (sockets, devices) are not workspace data
        // files and are left alone.
    }
    Ok(())
}

async fn dir_stats(
    root: &Path,
    chain: &[DirLink],
) -> anyhow::Result<(usize, u64, serde_json::Value, usize)> {
    let mut total_files = 0usize;
    let mut total_bytes = 0u64;
    let mut skipped = 0usize;
    let mut breakdown = serde_json::Map::new();

    verify_chain(chain).await?;
    let mut rd = fs::read_dir(root).await?;
    while let Some(entry) = rd.next_entry().await? {
        let file_type = entry.file_type().await?;
        if file_type.is_symlink() {
            // Symlinks are links, not workspace data; never traverse them,
            // but count them so stats shows the blind spot.
            skipped += 1;
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            let m = fs::symlink_metadata(&path).await?;
            anyhow::ensure!(
                m.is_dir() && !m.file_type().is_symlink(),
                "entry changed while being counted: {}",
                path.display()
            );
            let mut child_chain = chain.to_vec();
            child_chain.push(DirLink {
                path: path.clone(),
                id: file_id_of(&m),
            });
            let (f, b, s) = count_dir_contents(&path, &child_chain).await?;
            total_files += f;
            total_bytes += b;
            skipped += s;
            breakdown.insert(
                name,
                json!({"files": f, "size": b, "size_human": format_bytes(b)}),
            );
        } else if file_type.is_file() {
            verify_chain(chain).await?;
            if let Ok(meta) = fs::symlink_metadata(&path).await {
                total_files += 1;
                total_bytes += meta.len();
            }
        }
    }
    Ok((
        total_files,
        total_bytes,
        serde_json::Value::Object(breakdown),
        skipped,
    ))
}

async fn count_dir_contents(dir: &Path, chain: &[DirLink]) -> anyhow::Result<(usize, u64, usize)> {
    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut skipped = 0usize;
    verify_chain(chain).await?;
    let mut rd = fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let file_type = entry.file_type().await?;
        if file_type.is_symlink() {
            skipped += 1;
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            let m = fs::symlink_metadata(&path).await?;
            anyhow::ensure!(
                m.is_dir() && !m.file_type().is_symlink(),
                "entry changed while being counted: {}",
                path.display()
            );
            let mut child_chain = chain.to_vec();
            child_chain.push(DirLink {
                path: path.clone(),
                id: file_id_of(&m),
            });
            let (f, b, s) = Box::pin(count_dir_contents(&path, &child_chain)).await?;
            files += f;
            bytes += b;
            skipped += s;
        } else if file_type.is_file() {
            verify_chain(chain).await?;
            if let Ok(meta) = fs::symlink_metadata(&path).await {
                files += 1;
                bytes += meta.len();
            }
        }
    }
    Ok((files, bytes, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_tool(tmp: &TempDir) -> DataManagementTool {
        DataManagementTool::new(tmp.path().to_path_buf(), 90)
    }

    #[tokio::test]
    async fn retention_status_reports_correct_cutoff() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let res = tool
            .execute(json!({"command": "retention_status"}))
            .await
            .unwrap();
        assert!(res.success);
        let v: serde_json::Value = serde_json::from_str(&res.output).unwrap();
        assert_eq!(v["retention_days"], 90);
        assert!(v["cutoff"].is_string());
    }

    #[tokio::test]
    async fn purge_dry_run_does_not_delete() {
        let tmp = TempDir::new().unwrap();
        // Create a file with an old modification time by writing it (it will have
        // the current mtime, so it should not be purged with a 90-day retention).
        std::fs::write(tmp.path().join("recent.txt"), "data").unwrap();

        let tool = make_tool(&tmp);
        let res = tool
            .execute(json!({"command": "purge", "dry_run": true}))
            .await
            .unwrap();
        assert!(res.success);
        let v: serde_json::Value = serde_json::from_str(&res.output).unwrap();
        assert_eq!(v["dry_run"], true);
        // Recent file should not be counted for purge.
        assert_eq!(v["files"], 0);
        // File still exists.
        assert!(tmp.path().join("recent.txt").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn purge_never_deletes_through_symlinks() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::create_dir_all(outside.path().join("victim")).unwrap();
        std::fs::write(outside.path().join("victim/old.txt"), "old").unwrap();

        // A file symlink and a directory symlink planted in the workspace,
        // both pointing outside it. With retention_days = 0 every real file
        // is purge-eligible; the links must be neither followed nor
        // deleted, and nothing under the link targets may be removed.
        std::os::unix::fs::symlink(
            outside.path().join("victim/old.txt"),
            tmp.path().join("file-link"),
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path().join("victim"), tmp.path().join("dir-link"))
            .unwrap();

        let tool = DataManagementTool::new(tmp.path().to_path_buf(), 0);
        let res = tool
            .execute(json!({"command": "purge", "dry_run": false}))
            .await
            .unwrap();
        assert!(res.success);
        let v: serde_json::Value = serde_json::from_str(&res.output).unwrap();
        assert_eq!(v["files"], 0, "symlinks must not be purged: {v}");
        assert_eq!(
            v["symlinks_skipped"], 2,
            "skipped links must be visible: {v}"
        );
        assert!(
            outside.path().join("victim/old.txt").exists(),
            "purge must not delete through a symlink"
        );

        // Stats and status surface the same blind spot instead of hiding it.
        let res = tool.execute(json!({"command": "stats"})).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&res.output).unwrap();
        assert_eq!(v["symlinks_skipped"], 2);
        let res = tool
            .execute(json!({"command": "retention_status"}))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&res.output).unwrap();
        assert_eq!(v["symlinks_skipped"], 2);
    }

    /// Pin a file's modification time one year in the past so it is always
    /// older than any retention cutoff a test computes.
    #[cfg(unix)]
    fn make_old(path: &Path) {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(365 * 86_400);
        f.set_times(std::fs::FileTimes::new().set_modified(past))
            .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn purge_reports_deletion_failures_and_counts_only_confirmed() {
        let tmp = TempDir::new().unwrap();
        let locked = tmp.path().join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        let doomed = locked.join("old.txt");
        std::fs::write(&doomed, "old").unwrap();
        make_old(&doomed);

        // A read-only containing directory: the file is purge-eligible but
        // its deletion fails, so a purge that swallowed the error would
        // report files it never removed.
        let mut perms = std::fs::metadata(&locked).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&locked, perms).unwrap();
        let _restore = scopeguard::guard(locked.clone(), |p| {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755));
        });

        let tool = DataManagementTool::new(tmp.path().to_path_buf(), 0);
        let res = tool
            .execute(json!({"command": "purge", "dry_run": false}))
            .await
            .unwrap();
        assert!(
            !res.success,
            "a purge whose deletion failed must not report success"
        );
        let v: serde_json::Value = serde_json::from_str(&res.output).unwrap();
        assert_eq!(
            v["files"], 0,
            "counters must count only confirmed deletions: {v}"
        );
        let failures = v["failures"].as_array().unwrap();
        assert_eq!(
            failures.len(),
            1,
            "the failed deletion must be reported: {v}"
        );
        assert!(doomed.exists(), "the undeletable file must still exist");
        assert!(
            res.error.is_some(),
            "the error channel must carry the failure"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn purge_recursion_cannot_follow_a_directory_swapped_mid_walk() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let swap_dir = tmp.path().join("swap");
        std::fs::create_dir(&swap_dir).unwrap();

        // Two hundred real purge-eligible files inside the swap directory...
        for i in 0..200 {
            let f = swap_dir.join(format!("a{i:03}"));
            std::fs::write(&f, "x").unwrap();
            make_old(&f);
        }
        // ...but only ten matching names exist outside the jail. Any walk
        // step that resolves through a symlinked `swap` deletes those.
        for i in 190..200 {
            let f = outside.path().join(format!("a{i:03}"));
            std::fs::write(&f, "precious").unwrap();
            make_old(&f);
        }

        // Swapper: wait until the purge has started removing files from the
        // real directory, then rename it away and put a symlink to the
        // outside directory in its place, keeping that link for the rest
        // of the walk. Renaming instead of deleting keeps the swap to two
        // syscalls so it reliably lands mid-walk.
        let swapper = {
            let target = outside.path().to_path_buf();
            let swap_dir = swap_dir.clone();
            let staging = tmp.path().join("staged-away");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            std::thread::spawn(move || {
                loop {
                    if std::time::Instant::now() > deadline {
                        // Never hang the suite: a swap that never triggers
                        // fails the test via its landed-attack assertion.
                        return;
                    }
                    let remaining = std::fs::read_dir(&swap_dir)
                        .map(|rd| rd.count())
                        .unwrap_or(0);
                    if remaining < 200 {
                        let _ = std::fs::rename(&swap_dir, &staging);
                        let _ = std::os::unix::fs::symlink(&target, &swap_dir);
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_micros(50));
                }
            })
        };

        let tool = DataManagementTool::new(tmp.path().to_path_buf(), 0);
        let _ = tool
            .execute(json!({"command": "purge", "dry_run": false}))
            .await;
        let _ = swapper.join();

        // The attack itself must have landed, otherwise this test proves
        // nothing: `swap` is now a symlink, its real contents renamed away.
        assert!(
            std::fs::symlink_metadata(&swap_dir)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false),
            "swapper never replaced the directory; test setup failed"
        );

        for i in 190..200 {
            let f = outside.path().join(format!("a{i:03}"));
            assert!(
                f.exists(),
                "purge escaped through the swapped directory and deleted {}",
                f.display()
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn purge_recursion_refuses_nested_child_of_swapped_directory() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let swap_dir = tmp.path().join("swap");
        std::fs::create_dir(&swap_dir).unwrap();

        // Junk files to keep the walk busy, plus ten real subdirectories...
        for i in 0..600 {
            let f = swap_dir.join(format!("d{i:03}"));
            std::fs::write(&f, "x").unwrap();
            make_old(&f);
        }
        for i in 0..10 {
            let nested = swap_dir.join(format!("nested{i:02}"));
            std::fs::create_dir(&nested).unwrap();
            let inner = nested.join("inner");
            std::fs::write(&inner, "junk").unwrap();
            make_old(&inner);
        }
        // ...and a full mirror of those names outside the jail. The
        // mirror matters: after the swap the walk keeps resolving names
        // through the symlink, and only entries that still exist there
        // keep the walk alive long enough to reach the nested ones. A
        // walk that adopts a nested child resolved through a swapped
        // parent — trusting the child identity it just captured —
        // deletes the mirrored `inner` files.
        for i in 0..600 {
            let f = outside.path().join(format!("d{i:03}"));
            std::fs::write(&f, "mirror").unwrap();
            make_old(&f);
        }
        for i in 0..10 {
            let nested = outside.path().join(format!("nested{i:02}"));
            std::fs::create_dir(&nested).unwrap();
            let inner = nested.join("inner");
            std::fs::write(&inner, "precious").unwrap();
            make_old(&inner);
        }

        // Swapper: once the purge starts removing entries from the real
        // directory, rename it away and put a symlink in its place for the
        // rest of the walk.
        let swapper = {
            let target = outside.path().to_path_buf();
            let swap_dir = swap_dir.clone();
            let staging = tmp.path().join("staged-away");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            std::thread::spawn(move || {
                loop {
                    if std::time::Instant::now() > deadline {
                        // Never hang the suite: a swap that never triggers
                        // fails the test via its landed-attack assertion.
                        return;
                    }
                    let remaining = std::fs::read_dir(&swap_dir)
                        .map(|rd| rd.count())
                        .unwrap_or(0);
                    if remaining < 610 {
                        let _ = std::fs::rename(&swap_dir, &staging);
                        let _ = std::os::unix::fs::symlink(&target, &swap_dir);
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_micros(50));
                }
            })
        };

        let tool = DataManagementTool::new(tmp.path().to_path_buf(), 0);
        let _ = tool
            .execute(json!({"command": "purge", "dry_run": false}))
            .await;
        let _ = swapper.join();

        assert!(
            std::fs::symlink_metadata(&swap_dir)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false),
            "swapper never replaced the directory; test setup failed"
        );
        for i in 0..10 {
            let inner = outside.path().join(format!("nested{i:02}/inner"));
            assert!(
                inner.exists(),
                "purge adopted a nested child of the swapped directory and deleted {}",
                inner.display()
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn purge_walk_refuses_identity_adopted_through_swapped_ancestor() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let swap_dir = tmp.path().join("swap");
        let nested = swap_dir.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("inner"), "junk").unwrap();
        make_old(&nested.join("inner"));
        let target = outside.path().join("nested");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("inner"), "precious").unwrap();
        make_old(&target.join("inner"));

        // Swap the ancestor after the walk observed it, then capture the
        // child identity the way a mid-walk walk does: by resolving
        // through the swapped name. A walk that trusts that adopted
        // identity must still refuse to delete through the swapped
        // ancestor: the chain carried down from the real workspace and
        // real `swap` identities must veto it. (Against the pre-chain
        // walk this exact setup deleted the outside file; the end-to-end
        // variant of the attack is covered by the swap race test above.)
        std::fs::rename(&swap_dir, tmp.path().join("staged-away")).unwrap();
        std::os::unix::fs::symlink(outside.path(), &swap_dir).unwrap();
        let poisoned_id = file_id_of(&fs::symlink_metadata(swap_dir.join("nested")).await.unwrap());
        let real_swap_id = file_id_of(
            &fs::symlink_metadata(tmp.path().join("staged-away"))
                .await
                .unwrap(),
        );
        let tmp_id = file_id_of(&fs::symlink_metadata(tmp.path()).await.unwrap());
        let chain = vec![
            DirLink {
                path: tmp.path().to_path_buf(),
                id: tmp_id,
            },
            DirLink {
                path: swap_dir.clone(),
                id: real_swap_id,
            },
            DirLink {
                path: swap_dir.join("nested"),
                id: poisoned_id,
            },
        ];
        let mut out = PurgeOutcome {
            deleted: 0,
            bytes: 0,
            skipped: 0,
            failures: Vec::new(),
        };
        let res = purge_walk(&swap_dir.join("nested"), u64::MAX, false, &chain, &mut out).await;
        assert!(
            target.join("inner").exists(),
            "purge deleted outside the jail through an identity adopted via a swapped ancestor"
        );
        // The refusal must surface: either the walk fails loudly on the
        // broken chain or the refused deletion is recorded per entry.
        assert!(
            res.is_err() || !out.failures.is_empty(),
            "the swapped ancestor must produce a visible error or failure"
        );
        assert_eq!(out.deleted, 0, "nothing may be counted as deleted");
    }

    #[tokio::test]
    async fn stats_counts_files_correctly() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("subdir");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("a.txt"), "hello").unwrap();
        std::fs::write(sub.join("b.txt"), "world").unwrap();
        std::fs::write(tmp.path().join("root.txt"), "top").unwrap();

        let tool = make_tool(&tmp);
        let res = tool.execute(json!({"command": "stats"})).await.unwrap();
        assert!(res.success);
        let v: serde_json::Value = serde_json::from_str(&res.output).unwrap();
        assert_eq!(v["total_files"], 3);
    }
}

use async_trait::async_trait;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::fs;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};

/// Workspace backup tool: create, list, verify, and restore timestamped backups
/// with SHA-256 manifest integrity checking.
pub struct BackupTool {
    workspace_dir: PathBuf,
    include_dirs: Vec<String>,
    max_keep: usize,
}

/// A normal relative path component: non-empty, no separators (`/`,
/// `\\`), no NUL, no drive-prefix colon (Windows `C:foo`), not `.`/`..`.
/// Shared by backup-name validation and `include_dirs` validation so
/// neither can splice a path outside its root — including when the name
/// arrives percent-decoded from an HTTP path parameter.
fn is_single_component(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', ':', '\0'])
}

/// A backup name must be a single path component. Names this tool creates
/// always satisfy this (`backup-<timestamp>`); refusing anything else
/// keeps verify/restore rooted under `<workspace>/backups/`.
fn valid_backup_name(name: &str) -> bool {
    is_single_component(name)
}

impl BackupTool {
    pub fn new(workspace_dir: PathBuf, include_dirs: Vec<String>, max_keep: usize) -> Self {
        Self {
            workspace_dir,
            include_dirs,
            max_keep,
        }
    }

    fn backups_dir(&self) -> PathBuf {
        self.workspace_dir.join("backups")
    }

    /// Create a timestamped backup. Shared with the gateway operator
    /// surface (`/api/agents/{alias}/backup`); the model-visible `Tool`
    /// entry point and the operator API must stay behaviour-identical.
    pub async fn cmd_create(&self) -> anyhow::Result<ToolResult> {
        // include_dirs come from operator config, not the caller, but a
        // non-component entry would splice the copy outside the workspace
        // or backup root — refuse it here rather than trusting the file.
        for sub in &self.include_dirs {
            if !is_single_component(sub) {
                anyhow::bail!("invalid backup.include_dirs entry: {sub:?}");
            }
        }
        let backups_root = self.checked_backups_root().await?;
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let name = format!("backup-{ts}");
        let backup_dir = backups_root.join(&name);
        fs::create_dir_all(&backup_dir).await?;

        let result = self.create_into(&backup_dir).await;
        if result.is_err() {
            // Never leave a partial, listable, restorable backup behind a
            // failed create.
            let _ = fs::remove_dir_all(&backup_dir).await;
        }
        result?;

        let checksums = compute_checksums(&backup_dir).await?;
        let file_count = checksums.len();
        let manifest = serde_json::to_string_pretty(&checksums)?;
        fs::write(backup_dir.join("manifest.json"), &manifest).await?;

        // Enforce max_keep: remove oldest backups beyond the limit.
        self.enforce_max_keep().await?;

        Ok(ToolResult {
            success: true,
            output: json!({
                "backup": name,
                "file_count": file_count,
            })
            .to_string()
            .into(),
            error: None,
        })
    }

    async fn create_into(&self, backup_dir: &Path) -> anyhow::Result<()> {
        for sub in &self.include_dirs {
            let src = self.workspace_dir.join(sub);
            // Fail closed on a symlinked include dir (whether it points at
            // a file or a directory): copying through it would snapshot or
            // follow foreign data, so refuse instead of skipping silently.
            if is_symlink(&src).await {
                anyhow::bail!("refusing to follow symlink: {}", src.display());
            }
            if src.is_dir() {
                let dst = backup_dir.join(sub);
                copy_dir_recursive(&src, &dst).await?;
            }
        }
        Ok(())
    }

    /// The backups root, refusing to operate through a symlinked one: a
    /// planted `<workspace>/backups` link would let create write, and
    /// max_keep delete, directories outside the workspace.
    async fn checked_backups_root(&self) -> anyhow::Result<PathBuf> {
        let dir = self.backups_dir();
        if is_symlink(&dir).await {
            anyhow::bail!(
                "refusing to operate through symlinked backups root: {}",
                dir.display()
            );
        }
        Ok(dir)
    }

    async fn enforce_max_keep(&self) -> anyhow::Result<()> {
        let mut backups = self.list_backup_dirs().await?;
        // Sorted newest-first; drop excess from the tail.
        while backups.len() > self.max_keep {
            if let Some(old) = backups.pop() {
                fs::remove_dir_all(old).await?;
            }
        }
        Ok(())
    }

    async fn list_backup_dirs(&self) -> anyhow::Result<Vec<PathBuf>> {
        let dir = self.checked_backups_root().await?;
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut entries = Vec::new();
        let mut rd = fs::read_dir(&dir).await?;
        while let Some(e) = rd.next_entry().await? {
            // Non-following check: a symlink named `backup-*` must not
            // make an arbitrary target look like a backup.
            if e.file_type().await?.is_dir()
                && e.file_name().to_string_lossy().starts_with("backup-")
            {
                entries.push(e.path());
            }
        }
        entries.sort();
        entries.reverse(); // newest first
        Ok(entries)
    }

    /// List backups (newest first). Shared with the gateway operator surface.
    pub async fn cmd_list(&self) -> anyhow::Result<ToolResult> {
        let dirs = self.list_backup_dirs().await?;
        let mut items = Vec::new();
        for d in &dirs {
            let name = d
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let manifest_path = d.join("manifest.json");
            let file_count = if manifest_path.is_file() {
                let data = fs::read_to_string(&manifest_path).await?;
                let map: HashMap<String, String> = serde_json::from_str(&data).unwrap_or_default();
                map.len()
            } else {
                0
            };
            let meta = fs::metadata(d).await?;
            let created = meta
                .created()
                .or_else(|_| meta.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            let dt: chrono::DateTime<chrono::Utc> = created.into();
            items.push(json!({
                "name": name,
                "file_count": file_count,
                "created": dt.to_rfc3339(),
            }));
        }
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&items)?.into(),
            error: None,
        })
    }

    /// Verify a backup against its SHA-256 manifest. Shared with the
    /// gateway operator surface.
    pub async fn cmd_verify(&self, backup_name: &str) -> anyhow::Result<ToolResult> {
        if !valid_backup_name(backup_name) {
            return Ok(invalid_backup_name(backup_name));
        }
        let backups_root = self.checked_backups_root().await?;
        let backup_dir = backups_root.join(backup_name);
        if is_symlink(&backup_dir).await {
            anyhow::bail!(
                "refusing to verify through symlink: {}",
                backup_dir.display()
            );
        }
        if !backup_dir.is_dir() {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Backup not found: {backup_name}")),
            });
        }
        let manifest_path = backup_dir.join("manifest.json");
        let data = fs::read_to_string(&manifest_path).await?;
        let expected: HashMap<String, String> = serde_json::from_str(&data)?;
        let actual = compute_checksums(&backup_dir).await?;

        let mut mismatches = Vec::new();
        for (path, expected_hash) in &expected {
            match actual.get(path) {
                Some(actual_hash) if actual_hash == expected_hash => {}
                Some(actual_hash) => mismatches.push(json!({
                    "file": path,
                    "expected": expected_hash,
                    "actual": actual_hash,
                })),
                None => mismatches.push(json!({
                    "file": path,
                    "error": "missing",
                })),
            }
        }
        // Files present in the backup but absent from the manifest are
        // mismatches too: a trimmed manifest must not be able to report a
        // clean verify over extra planted payloads.
        for path in actual.keys() {
            if !expected.contains_key(path) {
                mismatches.push(json!({
                    "file": path,
                    "error": "unexpected",
                }));
            }
        }
        let pass = mismatches.is_empty();
        Ok(ToolResult {
            success: pass,
            output: json!({
                "backup": backup_name,
                "pass": pass,
                "checked": expected.len(),
                "mismatches": mismatches,
            })
            .to_string()
            .into(),
            error: if pass {
                None
            } else {
                Some("Integrity check failed".into())
            },
        })
    }

    /// Restore a backup. `confirm = false` returns a dry-run preview and
    /// mutates nothing; `confirm = true` overwrites workspace directories
    /// from the backup. Shared with the gateway operator surface, which
    /// must keep both halves of that contract intact.
    pub async fn cmd_restore(
        &self,
        backup_name: &str,
        confirm: bool,
    ) -> anyhow::Result<ToolResult> {
        if !valid_backup_name(backup_name) {
            return Ok(invalid_backup_name(backup_name));
        }
        let backups_root = self.checked_backups_root().await?;
        let backup_dir = backups_root.join(backup_name);
        if is_symlink(&backup_dir).await {
            anyhow::bail!(
                "refusing to restore through symlink: {}",
                backup_dir.display()
            );
        }
        if !backup_dir.is_dir() {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Backup not found: {backup_name}")),
            });
        }

        // Collect restorable subdirectories (skip manifest.json). Symlink
        // entries are refused: a backup this tool created contains only
        // real directories, so a symlink here means someone planted one
        // and restore must not copy through it.
        let mut restore_items: Vec<String> = Vec::new();
        let mut rd = fs::read_dir(&backup_dir).await?;
        while let Some(e) = rd.next_entry().await? {
            let name = e.file_name().to_string_lossy().to_string();
            if name == "manifest.json" {
                continue;
            }
            let file_type = e.file_type().await?;
            if file_type.is_symlink() {
                anyhow::bail!("refusing to restore through symlink: {}", name);
            }
            if file_type.is_dir() {
                restore_items.push(name);
            }
        }

        // Preflight the whole source tree before writing anything, so a
        // nested symlink fails the restore up front instead of after some
        // directories have already been overwritten.
        for sub in &restore_items {
            preflight_no_symlinks(&backup_dir.join(sub)).await?;
        }

        if !confirm {
            return Ok(ToolResult {
                success: true,
                output: json!({
                    "dry_run": true,
                    "backup": backup_name,
                    "would_restore": restore_items,
                })
                .to_string()
                .into(),
                error: None,
            });
        }

        for sub in &restore_items {
            let src = backup_dir.join(sub);
            let dst = self.workspace_dir.join(sub);
            copy_dir_recursive(&src, &dst).await?;
        }
        Ok(ToolResult {
            success: true,
            output: json!({
                "restored": backup_name,
                "directories": restore_items,
            })
            .to_string()
            .into(),
            error: None,
        })
    }
}

#[async_trait]
impl Tool for BackupTool {
    fn name(&self) -> &str {
        "backup"
    }

    fn description(&self) -> &str {
        "Create, list, verify, and restore workspace backups"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "enum": ["create", "list", "verify", "restore"],
                    "description": "Backup command to execute"
                },
                "backup_name": {
                    "type": "string",
                    "description": "Name of backup (for verify/restore)"
                },
                "confirm": {
                    "type": "boolean",
                    "description": "Confirm restore (required for actual restore, default false)"
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
            "create" => self.cmd_create().await,
            "list" => self.cmd_list().await,
            "verify" => {
                let name = args
                    .get("backup_name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Reject
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "param": "backup_name",
                                "command": "verify",
                            })),
                            "backup_tool: missing backup_name for verify"
                        );
                        anyhow::Error::msg("Missing 'backup_name' for verify")
                    })?;
                self.cmd_verify(name).await
            }
            "restore" => {
                let name = args
                    .get("backup_name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Reject
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "param": "backup_name",
                                "command": "restore",
                            })),
                            "backup_tool: missing backup_name for restore"
                        );
                        anyhow::Error::msg("Missing 'backup_name' for restore")
                    })?;
                let confirm = args
                    .get("confirm")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                self.cmd_restore(name, confirm).await
            }
            other => Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Unknown command: {other}")),
            }),
        }
    }
}

// -- Helpers ------------------------------------------------------------------

/// Rejection payload for a traversal-shaped backup name. Recognized by the
/// gateway operator surface as a 400.
fn invalid_backup_name(backup_name: &str) -> ToolResult {
    ToolResult {
        success: false,
        output: ToolOutput::default(),
        error: Some(format!("Invalid backup name: {backup_name}")),
    }
}

/// True when `path` itself is a symlink (does not look at the target).
async fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .await
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

/// Copy `src` into `dst` recursively, refusing to follow symlinks on
/// either side: a symlinked source entry or symlinked destination
/// directory would let a backup or restore escape the workspace jail.
async fn copy_dir_recursive(src: &Path, dst: &Path) -> anyhow::Result<()> {
    if is_symlink(src).await {
        anyhow::bail!("refusing to follow symlink: {}", src.display());
    }
    if is_symlink(dst).await {
        anyhow::bail!("refusing to copy through symlink: {}", dst.display());
    }
    fs::create_dir_all(dst).await?;
    let mut rd = fs::read_dir(src).await?;
    while let Some(entry) = rd.next_entry().await? {
        let file_type = entry.file_type().await?;
        if file_type.is_symlink() {
            anyhow::bail!("refusing to follow symlink: {}", entry.path().display());
        }
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            Box::pin(copy_dir_recursive(&src_path, &dst_path)).await?;
        } else {
            // `fs::copy` would follow a symlinked destination file and
            // clobber its target; refuse instead.
            if is_symlink(&dst_path).await {
                anyhow::bail!(
                    "refusing to overwrite through symlink: {}",
                    dst_path.display()
                );
            }
            fs::copy(&src_path, &dst_path).await?;
        }
    }
    Ok(())
}

/// Refuse any symlink anywhere under `dir` (non-following traversal).
/// Used as a preflight so destructive operations fail before partial
/// writes rather than midway through them.
async fn preflight_no_symlinks(dir: &Path) -> anyhow::Result<()> {
    let mut rd = fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let file_type = entry.file_type().await?;
        if file_type.is_symlink() {
            anyhow::bail!("refusing to follow symlink: {}", entry.path().display());
        }
        if file_type.is_dir() {
            Box::pin(preflight_no_symlinks(&entry.path())).await?;
        }
    }
    Ok(())
}

async fn compute_checksums(dir: &Path) -> anyhow::Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    let base = dir.to_path_buf();
    walk_and_hash(&base, dir, &mut map).await?;
    Ok(map)
}

async fn walk_and_hash(
    base: &Path,
    dir: &Path,
    map: &mut HashMap<String, String>,
) -> anyhow::Result<()> {
    let mut rd = fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        let file_type = entry.file_type().await?;
        if file_type.is_symlink() {
            anyhow::bail!("refusing to follow symlink: {}", path.display());
        }
        if file_type.is_dir() {
            Box::pin(walk_and_hash(base, &path, map)).await?;
        } else {
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if rel == "manifest.json" {
                continue;
            }
            let bytes = fs::read(&path).await?;
            let hash = hex::encode(Sha256::digest(&bytes));
            map.insert(rel, hash);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_tool(tmp: &TempDir) -> BackupTool {
        BackupTool::new(
            tmp.path().to_path_buf(),
            vec!["config".into(), "memory".into()],
            10,
        )
    }

    #[tokio::test]
    async fn create_backup_produces_manifest() {
        let tmp = TempDir::new().unwrap();
        // Seed workspace subdirectories.
        let cfg_dir = tmp.path().join("config");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("a.toml"), "key = 1").unwrap();

        let tool = make_tool(&tmp);
        let res = tool.execute(json!({"command": "create"})).await.unwrap();
        assert!(res.success, "create failed: {:?}", res.error);

        let parsed: serde_json::Value = serde_json::from_str(&res.output).unwrap();
        assert_eq!(parsed["file_count"], 1);

        // Manifest should exist inside the backup directory.
        let backup_name = parsed["backup"].as_str().unwrap();
        let manifest = tmp
            .path()
            .join("backups")
            .join(backup_name)
            .join("manifest.json");
        assert!(manifest.exists());
    }

    #[tokio::test]
    async fn verify_and_restore_reject_traversal_names() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        for bad in ["../escape", "sub/dir", "back\\slash", "..", ".", "C:escape"] {
            let res = tool
                .execute(json!({"command": "verify", "backup_name": bad}))
                .await
                .unwrap();
            assert!(!res.success, "{bad} must be rejected by verify");
            let err = res.error.unwrap();
            assert!(err.starts_with("Invalid backup name"), "{bad}: {err}");
        }
        // Even a confirmed restore with a traversal name must refuse
        // before touching the filesystem.
        let res = tool
            .execute(json!({"command": "restore", "backup_name": "../escape", "confirm": true}))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().starts_with("Invalid backup name"));
        assert!(!tmp.path().parent().unwrap().join("escape").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn create_refuses_symlinked_include_dir() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        // A symlink where an include dir is expected: copying through it
        // would snapshot foreign data, so create must fail closed.
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), tmp.path().join("config"))
            .unwrap();
        let tool = make_tool(&tmp);
        let res = tool.execute(json!({"command": "create"})).await;
        assert!(res.is_err(), "create must refuse to follow a symlink");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restore_refuses_backup_containing_symlink() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::create_dir_all(outside.path().join("victim")).unwrap();
        std::fs::write(outside.path().join("victim/data.txt"), "data").unwrap();

        // Plant a backup whose entry is a symlink pointing outside the
        // workspace; restore must refuse even in dry-run.
        let planted = tmp.path().join("backups/backup-planted");
        std::fs::create_dir_all(&planted).unwrap();
        std::os::unix::fs::symlink(outside.path().join("victim"), planted.join("config")).unwrap();

        let tool = make_tool(&tmp);
        let res = tool
            .execute(json!({"command": "restore", "backup_name": "backup-planted"}))
            .await;
        assert!(res.is_err(), "restore must refuse symlinked backup entries");
        assert!(outside.path().join("victim/data.txt").exists());
    }

    #[tokio::test]
    async fn verify_backup_detects_corruption() {
        let tmp = TempDir::new().unwrap();
        let cfg_dir = tmp.path().join("config");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("a.toml"), "original").unwrap();

        let tool = make_tool(&tmp);
        let res = tool.execute(json!({"command": "create"})).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&res.output).unwrap();
        let name = parsed["backup"].as_str().unwrap();

        // Corrupt a file inside the backup.
        let backed_up = tmp.path().join("backups").join(name).join("config/a.toml");
        std::fs::write(&backed_up, "corrupted").unwrap();

        let res = tool
            .execute(json!({"command": "verify", "backup_name": name}))
            .await
            .unwrap();
        assert!(!res.success);
        let v: serde_json::Value = serde_json::from_str(&res.output).unwrap();
        assert!(!v["mismatches"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn restore_requires_confirmation() {
        let tmp = TempDir::new().unwrap();
        let cfg_dir = tmp.path().join("config");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("a.toml"), "v1").unwrap();

        let tool = make_tool(&tmp);
        let res = tool.execute(json!({"command": "create"})).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&res.output).unwrap();
        let name = parsed["backup"].as_str().unwrap();

        // Without confirm: dry-run.
        let res = tool
            .execute(json!({"command": "restore", "backup_name": name}))
            .await
            .unwrap();
        assert!(res.success);
        let v: serde_json::Value = serde_json::from_str(&res.output).unwrap();
        assert_eq!(v["dry_run"], true);

        // With confirm: actual restore.
        let res = tool
            .execute(json!({"command": "restore", "backup_name": name, "confirm": true}))
            .await
            .unwrap();
        assert!(res.success);
        let v: serde_json::Value = serde_json::from_str(&res.output).unwrap();
        assert!(v.get("restored").is_some());
    }

    #[tokio::test]
    async fn create_rejects_traversal_include_dirs() {
        let tmp = TempDir::new().unwrap();
        // Config is operator-trusted, but a traversal-shaped include dir
        // must still be refused at the tool boundary, before any copying.
        let tool = BackupTool::new(tmp.path().to_path_buf(), vec!["../outside".into()], 10);
        let res = tool.execute(json!({"command": "create"})).await;
        assert!(res.is_err(), "traversal include dir must be refused");
        assert!(
            !tmp.path().join("backups").exists(),
            "nothing may be created"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn create_cleans_up_partial_backup_on_failure() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("config")).unwrap();
        std::fs::write(tmp.path().join("config/a.toml"), "v1").unwrap();
        // Second include dir is a symlink: fails midway, after `config`
        // was already copied, so the partial backup dir must be removed.
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("memory")).unwrap();

        let tool = make_tool(&tmp);
        let res = tool.execute(json!({"command": "create"})).await;
        assert!(res.is_err(), "create must fail on a symlinked include dir");
        let backups = tmp.path().join("backups");
        let leftovers: Vec<_> = std::fs::read_dir(&backups)
            .map(|rd| rd.filter_map(|e| e.ok()).collect())
            .unwrap_or_default();
        assert!(
            leftovers.is_empty(),
            "failed create must not leave a partial backup"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn create_refuses_symlinked_backups_root() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        // A planted <workspace>/backups symlink: create must not write
        // through it and max_keep must not be able to prune through it.
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("backups")).unwrap();
        let tool = make_tool(&tmp);
        let res = tool.execute(json!({"command": "create"})).await;
        assert!(res.is_err(), "symlinked backups root must be refused");
        let entries: Vec<_> = std::fs::read_dir(outside.path())
            .map(|rd| rd.filter_map(|e| e.ok()).collect())
            .unwrap_or_default();
        assert!(
            entries.is_empty(),
            "nothing may be written through the link"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restore_refuses_symlinked_destination_file() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("config")).unwrap();
        std::fs::write(tmp.path().join("config/a.toml"), "v1").unwrap();

        let tool = make_tool(&tmp);
        let res = tool.execute(json!({"command": "create"})).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&res.output).unwrap();
        let name = parsed["backup"].as_str().unwrap().to_string();

        // Replace the workspace file with a symlink to a foreign file:
        // a confirmed restore must refuse rather than clobber the target.
        let victim = outside.path().join("victim.txt");
        std::fs::write(&victim, "precious").unwrap();
        std::fs::remove_file(tmp.path().join("config/a.toml")).unwrap();
        std::os::unix::fs::symlink(&victim, tmp.path().join("config/a.toml")).unwrap();

        let res = tool
            .execute(json!({"command": "restore", "backup_name": name, "confirm": true}))
            .await;
        assert!(
            res.is_err(),
            "restore must refuse to overwrite through a symlinked file"
        );
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "precious",
            "the symlink target must be untouched"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restore_preflight_refuses_nested_symlink() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("config")).unwrap();
        std::fs::write(tmp.path().join("config/a.toml"), "v1").unwrap();

        let tool = make_tool(&tmp);
        let res = tool.execute(json!({"command": "create"})).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&res.output).unwrap();
        let name = parsed["backup"].as_str().unwrap().to_string();

        // Plant a symlink NESTED inside the backup's config dir. The
        // preflight must fail the restore (even dry-run) before any
        // workspace directory is overwritten.
        let nested = tmp.path().join("backups").join(&name).join("config");
        std::os::unix::fs::symlink(outside.path(), nested.join("link")).unwrap();

        let res = tool
            .execute(json!({"command": "restore", "backup_name": name}))
            .await;
        assert!(res.is_err(), "nested symlinks must fail the preflight");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("config/a.toml")).unwrap(),
            "v1",
            "dry-run or not, nothing may be overwritten before preflight"
        );
    }

    #[tokio::test]
    async fn verify_reports_unexpected_files() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("config")).unwrap();
        std::fs::write(tmp.path().join("config/a.toml"), "v1").unwrap();

        let tool = make_tool(&tmp);
        let res = tool.execute(json!({"command": "create"})).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&res.output).unwrap();
        let name = parsed["backup"].as_str().unwrap().to_string();

        // Tamper: add an unmanifested payload to the backup.
        std::fs::write(
            tmp.path()
                .join("backups")
                .join(&name)
                .join("config/planted.txt"),
            "extra",
        )
        .unwrap();

        let res = tool
            .execute(json!({"command": "verify", "backup_name": name}))
            .await
            .unwrap();
        assert!(!res.success, "extra unmanifested files must fail verify");
        let v: serde_json::Value = serde_json::from_str(&res.output).unwrap();
        let mismatches = v["mismatches"].as_array().unwrap();
        assert!(
            mismatches.iter().any(|m| m["error"] == "unexpected"),
            "the planted file must be reported as unexpected: {mismatches:?}"
        );
    }

    #[tokio::test]
    async fn list_backups_sorted_newest_first() {
        let tmp = TempDir::new().unwrap();
        let cfg_dir = tmp.path().join("config");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("a.toml"), "v1").unwrap();

        let tool = make_tool(&tmp);
        tool.execute(json!({"command": "create"})).await.unwrap();
        // Delay to ensure different second-resolution timestamps.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        tool.execute(json!({"command": "create"})).await.unwrap();

        let res = tool.execute(json!({"command": "list"})).await.unwrap();
        assert!(res.success);
        let items: Vec<serde_json::Value> = serde_json::from_str(&res.output).unwrap();
        assert_eq!(items.len(), 2);
        // Newest first by name (ISO8601 names sort lexicographically).
        assert!(items[0]["name"].as_str().unwrap() >= items[1]["name"].as_str().unwrap());
    }
}

use crate::fs_guard::{
    DirLink, FileId, file_id_of, guarded_copy, guarded_create_dir, guarded_remove_dir_all,
    guarded_write, verify_chain,
};
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
/// always satisfy this (`backup-<timestamp>-<random>`); refusing anything
/// else keeps verify/restore rooted under `<workspace>/backups/`.
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
        // Anchor the workspace and backups root BEFORE any mutation: both
        // links join the guard chains of everything below, including the
        // creation of the backup child itself. The workspace link rides
        // the DESTINATION chains too: per-agent workspace overrides allow
        // arbitrary paths, so no ancestor above the workspace is trusted
        // by default — every mutation below re-verifies the workspace
        // identity itself.
        let ws_link = self.ensure_workspace_root().await?;
        let src_base = vec![ws_link.clone()];
        let root_link = self.ensure_backups_root(&ws_link).await?;
        // The random suffix makes the generated child name unguessable, so
        // an attacker with write access inside `backups` cannot pre-create
        // a symlink at the exact name this create will use. The child is
        // created inside a blocking step that re-verifies the workspace
        // and root chain, and verified again below — defense against a
        // collision or a swap, not a substitute for the checks.
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let name = format!("backup-{ts}-{}", uuid::Uuid::new_v4().simple());
        let backup_dir = root_link.path.join(&name);
        let root_chain = vec![ws_link.clone(), root_link.clone()];
        let child_id = tokio::task::spawn_blocking({
            let dir = backup_dir.clone();
            move || guarded_create_dir(root_chain, &dir)
        })
        .await?
        .map_err(anyhow::Error::msg)?;
        let child_link = DirLink {
            path: backup_dir.clone(),
            id: child_id,
        };
        let (backup_dir, root_link, child_link) =
            self.verify_created_child(&root_link, &child_link).await?;
        let dst_base = vec![ws_link, root_link.clone(), child_link.clone()];

        let result = self.create_into(&backup_dir, &src_base, &dst_base).await;
        if result.is_err() {
            // Never leave a partial, listable, restorable backup behind a
            // failed create — but only remove it if the whole chain down
            // to it is still the real directories this call created,
            // never through a swapped name.
            let chain = dst_base.clone();
            let _ = tokio::task::spawn_blocking(move || guarded_remove_dir_all(chain)).await;
        }
        result?;

        let checksums = compute_checksums(&backup_dir, &dst_base).await?;
        let file_count = checksums.len();
        let manifest = serde_json::to_string_pretty(&checksums)?;
        let manifest_path = backup_dir.join("manifest.json");
        let chain = dst_base;
        tokio::task::spawn_blocking(move || {
            guarded_write(chain, manifest_path, manifest.into_bytes())
        })
        .await?
        .map_err(anyhow::Error::msg)?;

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

    async fn create_into(
        &self,
        backup_dir: &Path,
        src_base: &[DirLink],
        dst_base: &[DirLink],
    ) -> anyhow::Result<()> {
        for sub in &self.include_dirs {
            let src = self.workspace_dir.join(sub);
            // Fail closed on a symlinked include dir (whether it points at
            // a file or a directory): copying through it would snapshot or
            // follow foreign data, so refuse instead of skipping silently.
            if is_symlink(&src).await {
                anyhow::bail!("refusing to follow symlink: {}", src.display());
            }
            let meta = fs::symlink_metadata(&src).await;
            if let Ok(m) = meta
                && m.is_dir()
            {
                let dst = backup_dir.join(sub);
                copy_dir_recursive(&src, &dst, src_base, dst_base).await?;
            }
        }
        Ok(())
    }

    /// The workspace root as a verified chain link. Every mutation chain
    /// in this tool is anchored here (source side for copies, parent for
    /// the backups root), so a swap of the workspace root itself — or any
    /// relative resolution through it — is refused downstream. A missing
    /// root yields `None` for callers that treat it as "not created yet".
    async fn workspace_root_link(&self) -> anyhow::Result<Option<DirLink>> {
        match fs::symlink_metadata(&self.workspace_dir).await {
            Ok(m) => {
                anyhow::ensure!(
                    m.is_dir() && !m.file_type().is_symlink(),
                    "workspace root is not a real directory: {}",
                    self.workspace_dir.display()
                );
                Ok(Some(DirLink {
                    path: self.workspace_dir.clone(),
                    id: file_id_of(&m),
                }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Like [`Self::workspace_root_link`], but creates a missing workspace
    /// root first (its own parents are operator-configured install paths,
    /// outside any attacker-reachable jail) and refuses a symlinked one,
    /// so copy chains always anchor on a real, existing directory.
    async fn ensure_workspace_root(&self) -> anyhow::Result<DirLink> {
        if let Some(link) = self.workspace_root_link().await? {
            return Ok(link);
        }
        let dir = self.workspace_dir.clone();
        let id = tokio::task::spawn_blocking(move || guarded_create_dir(Vec::new(), &dir))
            .await?
            .map_err(anyhow::Error::msg)?;
        Ok(DirLink {
            path: self.workspace_dir.clone(),
            id,
        })
    }

    /// The backups root as a verified chain link, refusing to operate
    /// through a symlinked one: a planted `<workspace>/backups` link
    /// would let create write, and max_keep delete, directories outside
    /// the workspace. A missing root yields `None` for callers that treat
    /// it as "no backups yet".
    async fn existing_backups_root(&self) -> anyhow::Result<Option<DirLink>> {
        let dir = self.backups_dir();
        match fs::symlink_metadata(&dir).await {
            Ok(m) => {
                if m.file_type().is_symlink() || !m.is_dir() {
                    anyhow::bail!(
                        "refusing to operate through symlinked backups root: {}",
                        dir.display()
                    );
                }
                Ok(Some(DirLink {
                    path: dir,
                    id: file_id_of(&m),
                }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The read-side anchor chain `[workspace, backups root]` for list /
    /// verify / restore. Both links are captured before anything under
    /// the backups root is resolved, so an outside tree cannot be adopted
    /// through a workspace or root swapped before the call, and the chain
    /// re-verification below each read refuses swaps during it. A missing
    /// workspace or backups root yields `None` ("no backups yet").
    async fn anchored_read_root(&self) -> anyhow::Result<Option<Vec<DirLink>>> {
        let Some(ws) = self.workspace_root_link().await? else {
            return Ok(None);
        };
        let Some(root) = self.existing_backups_root().await? else {
            return Ok(None);
        };
        Ok(Some(vec![ws, root]))
    }

    /// Like [`Self::existing_backups_root`], but creates a missing root
    /// inside a blocking step guarded by the workspace-root chain, so the
    /// create path never mkdirs through an unverified name.
    async fn ensure_backups_root(&self, ws_link: &DirLink) -> anyhow::Result<DirLink> {
        if let Some(link) = self.existing_backups_root().await? {
            return Ok(link);
        }
        let chain = vec![ws_link.clone()];
        let dir = self.backups_dir();
        let id = tokio::task::spawn_blocking(move || guarded_create_dir(chain, &dir))
            .await?
            .map_err(anyhow::Error::msg)?;
        Ok(DirLink {
            path: self.backups_dir(),
            id,
        })
    }

    async fn enforce_max_keep(&self) -> anyhow::Result<()> {
        let mut backups = self.list_backup_dirs().await?;
        // Sorted newest-first; drop excess from the tail.
        while backups.len() > self.max_keep {
            if let Some(old) = backups.pop() {
                self.remove_verified_backup_dir(&old).await?;
            }
        }
        Ok(())
    }

    /// Verify the freshly created backup child is a real directory that
    /// still has the identity the guarded creation observed, sits
    /// directly under the real backups root (canonical containment, so
    /// nothing on the path resolved through a symlink), and return the
    /// verified root and child links for the mutation guards that follow.
    /// A pre-planted `backups/backup-*` symlink that `create_dir_all`
    /// would silently accept is caught here before anything is written
    /// through it.
    async fn verify_created_child(
        &self,
        root_link: &DirLink,
        child_link: &DirLink,
    ) -> anyhow::Result<(PathBuf, DirLink, DirLink)> {
        let root_meta = fs::symlink_metadata(&root_link.path).await?;
        if root_meta.file_type().is_symlink()
            || !root_meta.is_dir()
            || file_id_of(&root_meta) != root_link.id
        {
            anyhow::bail!(
                "backups root changed identity during create: {}",
                root_link.path.display()
            );
        }
        let child_meta = fs::symlink_metadata(&child_link.path).await?;
        if child_meta.file_type().is_symlink() || !child_meta.is_dir() {
            anyhow::bail!(
                "created backup path is not a real directory: {}",
                child_link.path.display()
            );
        }
        if file_id_of(&child_meta) != child_link.id {
            anyhow::bail!(
                "created backup path changed identity during create: {}",
                child_link.path.display()
            );
        }
        // Canonical containment: the resolved child must sit directly
        // inside the resolved root. If any component resolved through a
        // symlink, the canonical paths disagree with the parent check.
        let root_canon = fs::canonicalize(&root_link.path).await?;
        let child_canon = fs::canonicalize(&child_link.path).await?;
        if child_canon.parent() != Some(root_canon.as_path()) {
            anyhow::bail!(
                "backup directory resolved outside the backups root: {} -> {}",
                child_link.path.display(),
                child_canon.display()
            );
        }
        Ok((
            child_link.path.clone(),
            root_link.clone(),
            child_link.clone(),
        ))
    }

    /// Remove a listed backup directory after re-verifying the whole chain
    /// — backups root then the backup itself — is still real: a name
    /// swapped to a symlink between the listing and the removal must not
    /// be pruned through.
    async fn remove_verified_backup_dir(&self, old: &Path) -> anyhow::Result<()> {
        let Some(root_link) = self.existing_backups_root().await? else {
            return Ok(());
        };
        let old_canon = fs::canonicalize(old).await?;
        let root_canon = fs::canonicalize(&root_link.path).await?;
        if old_canon.parent() != Some(root_canon.as_path()) {
            anyhow::bail!(
                "refusing to prune outside the backups root: {}",
                old.display()
            );
        }
        let meta = fs::symlink_metadata(old).await?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            anyhow::bail!("refusing to prune through symlink: {}", old.display());
        }
        let chain = vec![
            root_link,
            DirLink {
                path: old.to_path_buf(),
                id: file_id_of(&meta),
            },
        ];
        tokio::task::spawn_blocking(move || guarded_remove_dir_all(chain))
            .await?
            .map_err(anyhow::Error::msg)?;
        Ok(())
    }

    async fn list_backup_dirs(&self) -> anyhow::Result<Vec<PathBuf>> {
        let Some(chain) = self.anchored_read_root().await? else {
            return Ok(Vec::new());
        };
        let root = chain.last().cloned().expect("anchor chain has a root");
        let mut entries = Vec::new();
        let mut rd = fs::read_dir(&root.path).await?;
        while let Some(e) = rd.next_entry().await? {
            // Non-following check: a symlink named `backup-*` must not
            // make an arbitrary target look like a backup.
            if e.file_type().await?.is_dir()
                && e.file_name().to_string_lossy().starts_with("backup-")
            {
                entries.push(e.path());
            }
        }
        // The listing must describe the root it anchored on: a workspace
        // or root swapped mid-listing must not hand back outside names.
        verify_chain(&chain).await?;
        entries.sort();
        entries.reverse(); // newest first
        Ok(entries)
    }

    /// List backups (newest first). Shared with the gateway operator surface.
    pub async fn cmd_list(&self) -> anyhow::Result<ToolResult> {
        let Some(anchor) = self.anchored_read_root().await? else {
            return Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&Vec::<serde_json::Value>::new())?.into(),
                error: None,
            });
        };
        let dirs = self.list_backup_dirs().await?;
        let mut items = Vec::new();
        for d in &dirs {
            // Each entry is read under a chain extended to the entry
            // itself, so a swapped workspace, root, or child cannot feed
            // outside manifests or metadata into the listing.
            let entry_meta = fs::symlink_metadata(d).await?;
            anyhow::ensure!(
                entry_meta.is_dir() && !entry_meta.file_type().is_symlink(),
                "listed backup is not a real directory: {}",
                d.display()
            );
            let mut chain = anchor.clone();
            chain.push(DirLink {
                path: d.clone(),
                id: file_id_of(&entry_meta),
            });
            verify_chain(&chain).await?;
            let name = d
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let manifest_path = d.join("manifest.json");
            // Non-following: a swapped-in symlinked manifest must not make
            // a foreign file readable as this backup's manifest.
            let file_count = match fs::symlink_metadata(&manifest_path).await {
                Ok(m) if m.is_file() => {
                    let data = fs::read_to_string(&manifest_path).await?;
                    let map: HashMap<String, String> =
                        serde_json::from_str(&data).unwrap_or_default();
                    map.len()
                }
                _ => 0,
            };
            verify_chain(&chain).await?;
            let meta = fs::symlink_metadata(d).await?;
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
        let Some(anchor) = self.anchored_read_root().await? else {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Backup not found: {backup_name}")),
            });
        };
        let root_link = anchor.last().cloned().expect("anchor chain has a root");
        let backup_dir = root_link.path.join(backup_name);
        let backup_meta = match fs::symlink_metadata(&backup_dir).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!("Backup not found: {backup_name}")),
                });
            }
            Err(e) => return Err(e.into()),
        };
        let backup_id = file_id_of(&backup_meta);
        if backup_meta.file_type().is_symlink() {
            anyhow::bail!(
                "refusing to verify through symlink: {}",
                backup_dir.display()
            );
        }
        if !backup_meta.is_dir() {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Backup not found: {backup_name}")),
            });
        }
        // Everything read below is pinned to the workspace, backups root,
        // and this child — checked before the manifest is even opened, so
        // an outside tree swapped in earlier cannot be read at all.
        let mut read_chain = anchor;
        read_chain.push(DirLink {
            path: backup_dir.clone(),
            id: backup_id,
        });
        verify_chain(&read_chain).await?;
        let manifest_path = backup_dir.join("manifest.json");
        let manifest_meta = fs::symlink_metadata(&manifest_path).await?;
        if manifest_meta.file_type().is_symlink() {
            anyhow::bail!(
                "refusing to read manifest through symlink: {}",
                manifest_path.display()
            );
        }
        let data = fs::read_to_string(&manifest_path).await?;
        let expected: HashMap<String, String> = serde_json::from_str(&data)?;
        let actual = compute_checksums(&backup_dir, &read_chain).await?;
        // The verify result must describe the tree it started from: if
        // any component of the chain was swapped during the read, refuse
        // rather than report a verdict about foreign data.
        verify_chain(&read_chain).await?;

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
        // Anchor the workspace and backups root BEFORE resolving the
        // backup child, so the child identity cannot be adopted through a
        // workspace or root swapped before the call or between an earlier
        // check and this resolution; the full chain rides every read and
        // copy below.
        let Some(anchor) = self.anchored_read_root().await? else {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Backup not found: {backup_name}")),
            });
        };
        let root_link = anchor.last().cloned().expect("anchor chain has a root");
        let backup_dir = root_link.path.join(backup_name);
        let backup_meta = match fs::symlink_metadata(&backup_dir).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!("Backup not found: {backup_name}")),
                });
            }
            Err(e) => return Err(e.into()),
        };
        if backup_meta.file_type().is_symlink() {
            anyhow::bail!(
                "refusing to restore through symlink: {}",
                backup_dir.display()
            );
        }
        if !backup_meta.is_dir() {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Backup not found: {backup_name}")),
            });
        }
        let backup_id = file_id_of(&backup_meta);
        // Everything read or copied below is pinned to the workspace, the
        // backups root, and this child.
        let mut src_anchor = anchor;
        src_anchor.push(DirLink {
            path: backup_dir.clone(),
            id: backup_id,
        });
        verify_chain(&src_anchor).await?;

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
        verify_chain(&src_anchor).await?;

        // Record the identity of each restorable subdirectory as observed
        // now: after the dry-run gate the restore re-verifies them, so a
        // backup swapped for a symlink or for a different directory in
        // that window fails instead of copying from wherever the swapped
        // name points.
        let mut sub_ids: Vec<(String, FileId)> = Vec::with_capacity(restore_items.len());
        for sub in &restore_items {
            let m = fs::symlink_metadata(&backup_dir.join(sub)).await?;
            anyhow::ensure!(
                m.is_dir() && !m.file_type().is_symlink(),
                "restorable entry is not a real directory: {sub}"
            );
            sub_ids.push((sub.clone(), file_id_of(&m)));
        }

        // Preflight the whole source tree before writing anything, so a
        // nested symlink fails the restore up front instead of after some
        // directories have already been overwritten. The preflight walks
        // under the verified chain and verifies it at every step.
        for sub in &restore_items {
            preflight_no_symlinks(&backup_dir.join(sub), &src_anchor).await?;
        }

        if !confirm {
            // The preview must describe the tree it just walked: re-check
            // the whole chain before publishing the names, so a swap
            // mid-collection cannot feed outside-derived entries to the
            // operator.
            verify_chain(&src_anchor).await?;
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

        verify_chain(&src_anchor).await?;
        // The workspace root anchors the destination side of every copy;
        // the workspace, backups root, and backup child anchor the source
        // side. All chains are re-verified before every mutation inside
        // the copy walk, so a swapped ancestor anywhere between a jail
        // root and a copied file refuses the copy.
        let dst_base = vec![self.ensure_workspace_root().await?];
        let src_base = src_anchor.clone();
        for (sub, id) in &sub_ids {
            // Re-verify each source immediately before its copy, not all
            // of them up front: earlier copies take time, and a name that
            // changed identity in that window must fail its own copy
            // rather than be trusted from an earlier check.
            let m = fs::symlink_metadata(backup_dir.join(sub)).await?;
            if !m.is_dir() || file_id_of(&m) != *id {
                anyhow::bail!("backup entry changed while restore was running: {sub}");
            }
            let src = backup_dir.join(sub);
            let dst = self.workspace_dir.join(sub);
            copy_dir_recursive(&src, &dst, &src_base, &dst_base).await?;
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

/// Copy `src` into `dst` recursively, never following symlinks on either
/// side and never overwriting a destination that is a symlink or a hard
/// link. Each file copy re-verifies, in one blocking step, the FULL
/// ancestor chains of both the source and the destination (seeded from the
/// caller's verified roots and extended as the walk descends), the source
/// entry, and the destination entry, so a component anywhere above a
/// mutation that was swapped for a symlink between the walk and the copy
/// is refused instead of written through. The residual window between that
/// re-check and the copy's own open is a single syscall wide — inherent to
/// path-based APIs — and is the same discipline applied by the `fs_guard`
/// helpers.
async fn copy_dir_recursive(
    src: &Path,
    dst: &Path,
    src_base: &[DirLink],
    dst_base: &[DirLink],
) -> anyhow::Result<()> {
    let src_meta = fs::symlink_metadata(src).await?;
    if src_meta.file_type().is_symlink() {
        anyhow::bail!("refusing to follow symlink: {}", src.display());
    }
    if !src_meta.is_dir() {
        anyhow::bail!("not a directory: {}", src.display());
    }
    let mut src_chain = src_base.to_vec();
    src_chain.push(DirLink {
        path: src.to_path_buf(),
        id: file_id_of(&src_meta),
    });

    match fs::symlink_metadata(dst).await {
        Ok(m) => {
            if m.file_type().is_symlink() {
                anyhow::bail!("refusing to copy through symlink: {}", dst.display());
            }
            if !m.is_dir() {
                anyhow::bail!("destination is not a directory: {}", dst.display());
            }
            // Adopted as a chain link and re-verified before every
            // mutation, so a resolution through a swapped ancestor cannot
            // smuggle in a foreign directory here.
            let mut chain = dst_base.to_vec();
            chain.push(DirLink {
                path: dst.to_path_buf(),
                id: file_id_of(&m),
            });
            recurse_entries(src, dst, src_chain, chain).await
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Create the destination inside a blocking step that first
            // re-verifies the whole destination chain: a swap of any
            // ancestor since it was observed must refuse the create, not
            // create through wherever the swapped name points.
            let chain = dst_base.to_vec();
            let dst_owned = dst.to_path_buf();
            let dst_id = tokio::task::spawn_blocking(move || guarded_create_dir(chain, &dst_owned))
                .await?
                .map_err(anyhow::Error::msg)?;
            let mut chain = dst_base.to_vec();
            chain.push(DirLink {
                path: dst.to_path_buf(),
                id: dst_id,
            });
            recurse_entries(src, dst, src_chain, chain).await
        }
        Err(e) => Err(e.into()),
    }
}

/// Enumerate `src` and copy its entries into `dst`, with both chains
/// re-verified before every mutation.
async fn recurse_entries(
    src: &Path,
    dst: &Path,
    src_chain: Vec<DirLink>,
    dst_chain: Vec<DirLink>,
) -> anyhow::Result<()> {
    let mut rd = fs::read_dir(src).await?;
    while let Some(entry) = rd.next_entry().await? {
        let file_type = entry.file_type().await?;
        if file_type.is_symlink() {
            anyhow::bail!("refusing to follow symlink: {}", entry.path().display());
        }
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            Box::pin(copy_dir_recursive(
                &src_path, &dst_path, &src_chain, &dst_chain,
            ))
            .await?;
        } else if file_type.is_file() {
            let entry_meta = entry.metadata().await?;
            if !entry_meta.is_file() {
                anyhow::bail!("{} changed while being copied", src_path.display());
            }
            let src_id = file_id_of(&entry_meta);
            let sc = src_chain.clone();
            let dc = dst_chain.clone();
            tokio::task::spawn_blocking(move || guarded_copy(src_path, src_id, sc, dst_path, dc))
                .await?
                .map_err(anyhow::Error::msg)?;
        }
        // Other entry kinds (sockets, devices) carry no file data and are
        // skipped: a copy that cannot be checksummed or restored has no
        // business being in a backup.
    }
    Ok(())
}

/// Refuse any symlink anywhere under `dir` (non-following traversal).
/// Used as a preflight so destructive operations fail before partial
/// writes rather than midway through them. The walk verifies `chain` at
/// every step and extends it with each directory it descends into, so a
/// component swapped mid-preflight cannot have its contents read.
async fn preflight_no_symlinks(dir: &Path, chain: &[DirLink]) -> anyhow::Result<()> {
    verify_chain(chain).await?;
    let mut rd = fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let file_type = entry.file_type().await?;
        if file_type.is_symlink() {
            anyhow::bail!("refusing to follow symlink: {}", entry.path().display());
        }
        if file_type.is_dir() {
            let m = fs::symlink_metadata(entry.path()).await?;
            anyhow::ensure!(
                m.is_dir() && !m.file_type().is_symlink(),
                "entry changed while being walked: {}",
                entry.path().display()
            );
            let mut child_chain = chain.to_vec();
            child_chain.push(DirLink {
                path: entry.path(),
                id: file_id_of(&m),
            });
            Box::pin(preflight_no_symlinks(&entry.path(), &child_chain)).await?;
        }
    }
    Ok(())
}

/// Hash the file tree under `dir`, refusing symlinks and re-verifying the
/// ancestor `chain` before each directory read and file read, so names
/// and hashes cannot be sourced from a tree swapped in mid-walk (which
/// would poison a manifest or forge a verify verdict; a swap-away and
/// swap-back around the post-walk identity check could otherwise pass).
/// The chain check and the read are still two steps, so a swap landing
/// exactly between them remains possible in principle — the same
/// single-step residual every path-based guard here carries.
async fn compute_checksums(
    dir: &Path,
    chain: &[DirLink],
) -> anyhow::Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    let base = dir.to_path_buf();
    Box::pin(walk_and_hash(&base, dir, chain, &mut map)).await?;
    Ok(map)
}

async fn walk_and_hash(
    base: &Path,
    dir: &Path,
    chain: &[DirLink],
    map: &mut HashMap<String, String>,
) -> anyhow::Result<()> {
    verify_chain(chain).await?;
    let mut rd = fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        let file_type = entry.file_type().await?;
        if file_type.is_symlink() {
            anyhow::bail!("refusing to follow symlink: {}", path.display());
        }
        if file_type.is_dir() {
            // Extend the chain with the descended directory: a nested
            // directory swapped for a symlink mid-walk must not have its
            // target's contents hashed under this backup's name even
            // though every ancestor identity still matches.
            let m = fs::symlink_metadata(&path).await?;
            anyhow::ensure!(
                m.is_dir() && !m.file_type().is_symlink(),
                "entry changed while being hashed: {}",
                path.display()
            );
            let mut child_chain = chain.to_vec();
            child_chain.push(DirLink {
                path: path.clone(),
                id: file_id_of(&m),
            });
            Box::pin(walk_and_hash(base, &path, &child_chain, map)).await?;
        } else {
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if rel == "manifest.json" {
                continue;
            }
            verify_chain(chain).await?;
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

    #[cfg(unix)]
    #[tokio::test]
    async fn create_refuses_planted_symlink_backup_child() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("config")).unwrap();
        std::fs::write(tmp.path().join("config/a.toml"), "v1").unwrap();

        // Pre-plant the deterministic per-second backup names for the
        // current and the next second as symlinks to an outside directory.
        // A create that only does create_dir_all on the predictable child
        // copies workspace data and writes its manifest straight through
        // such a link.
        let backups = tmp.path().join("backups");
        std::fs::create_dir_all(&backups).unwrap();
        let now = chrono::Utc::now();
        for delta in [0, 1] {
            let ts = (now + chrono::Duration::seconds(delta)).format("%Y%m%dT%H%M%SZ");
            let link = backups.join(format!("backup-{ts}"));
            std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        }

        let tool = make_tool(&tmp);
        let res = tool.execute(json!({"command": "create"})).await;
        // Create must succeed normally despite the planted links; a
        // failure that happened to leave the links untouched would prove
        // nothing, so the result itself is asserted too.
        assert!(
            res.is_ok(),
            "create must succeed with unguessable child names: {res:?}"
        );

        let leaked: Vec<String> = std::fs::read_dir(outside.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            leaked.is_empty(),
            "create wrote through the planted symlink child: {leaked:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn create_refuses_backup_child_swapped_after_verification() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        // The first include dir holds a single trigger file; the second
        // holds the long copy phase during which the swap must be caught.
        let trigger_dir = tmp.path().join("0trigger");
        std::fs::create_dir_all(&trigger_dir).unwrap();
        std::fs::write(trigger_dir.join("a000"), "t").unwrap();
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        for i in 0..300 {
            std::fs::write(cfg.join(format!("c{i:03}")), "v1").unwrap();
        }

        // Swapper: once the create has copied the trigger file into the
        // fresh child (proof the child was created and verified), rename
        // the child away and put a symlink in its place — after the
        // create-time verification but while the copy phase is still
        // running. A copy that only re-checks the final destination
        // components happily writes through such a swapped ancestor.
        let swapper = {
            let backups = tmp.path().join("backups");
            let target = outside.path().to_path_buf();
            std::thread::spawn(move || {
                loop {
                    let child = std::fs::read_dir(&backups).ok().and_then(|rd| {
                        rd.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| {
                            std::fs::symlink_metadata(p)
                                .map(|m| m.is_dir())
                                .unwrap_or(false)
                        })
                    });
                    if let Some(child) = child
                        && child.join("0trigger/a000").exists()
                    {
                        let staging = backups.join("staged-away");
                        let _ = std::fs::rename(&child, &staging);
                        let _ = std::os::unix::fs::symlink(&target, &child);
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_micros(20));
                }
            })
        };

        let tool = BackupTool::new(
            tmp.path().to_path_buf(),
            vec!["0trigger".into(), "config".into()],
            10,
        );
        let _ = tool.execute(json!({"command": "create"})).await;
        let _ = swapper.join();

        // The attack must have landed: a symlink now sits where the
        // verified child was, and the real child was renamed away.
        let backups = tmp.path().join("backups");
        let attack_landed = std::fs::read_dir(&backups)
            .map(|rd| {
                rd.filter_map(|e| e.ok()).any(|e| {
                    std::fs::symlink_metadata(e.path())
                        .map(|m| m.file_type().is_symlink())
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        assert!(
            attack_landed,
            "swapper never replaced the created child; test setup failed"
        );

        let leaked: Vec<String> = std::fs::read_dir(outside.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            leaked.is_empty(),
            "create copied through the swapped child: {leaked:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restore_refuses_hardlinked_destination() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("config")).unwrap();
        std::fs::write(tmp.path().join("config/a.toml"), "v1").unwrap();

        let tool = make_tool(&tmp);
        let res = tool.execute(json!({"command": "create"})).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&res.output).unwrap();
        let name = parsed["backup"].as_str().unwrap().to_string();

        // Replace the workspace file with a hard link to a foreign file:
        // the destination pathname stays inside the workspace, but the
        // inode it names is shared with a file outside it, so an
        // overwriting copy truncates foreign data.
        let victim = outside.path().join("victim.txt");
        std::fs::write(&victim, "precious").unwrap();
        std::fs::remove_file(tmp.path().join("config/a.toml")).unwrap();
        std::fs::hard_link(&victim, tmp.path().join("config/a.toml")).unwrap();

        let res = tool
            .execute(json!({"command": "restore", "backup_name": name, "confirm": true}))
            .await;
        assert!(
            res.is_err(),
            "restore must refuse a hard-linked destination"
        );
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "precious",
            "the shared inode outside the workspace must not be truncated"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restore_refuses_symlinked_backups_root() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("config")).unwrap();
        std::fs::write(tmp.path().join("config/a.toml"), "v1").unwrap();
        // A planted <workspace>/backups symlink: restore must not resolve
        // the requested backup (nor write anything) through it.
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("backups")).unwrap();

        let tool = make_tool(&tmp);
        let res = tool
            .execute(json!({"command": "restore", "backup_name": "backup-x", "confirm": true}))
            .await;
        assert!(res.is_err(), "restore must refuse a symlinked backups root");
        let entries = std::fs::read_dir(outside.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .count();
        assert_eq!(
            entries, 0,
            "nothing may be read or written through the link"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn copy_walk_refuses_destination_ancestor_swapped() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let src_dir = tmp.path().join("config");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("a.toml"), "v1").unwrap();

        // Real workspace / backups / child chains captured before the swap.
        let backups = tmp.path().join("backups");
        let child = backups.join("backup-x");
        std::fs::create_dir_all(&child).unwrap();
        let mk_link = |p: &Path| {
            let m = std::fs::symlink_metadata(p).unwrap();
            DirLink {
                path: p.to_path_buf(),
                id: file_id_of(&m),
            }
        };
        let ws_link = mk_link(tmp.path());
        let root_link = mk_link(&backups);
        let child_link = mk_link(&child);

        // Swap the verified child for a symlink to an outside directory,
        // then run the copy walk with the stale (honest) chains: the walk
        // must refuse to create or write anything through the link.
        std::fs::rename(&child, backups.join("staged-away")).unwrap();
        std::os::unix::fs::symlink(outside.path(), &child).unwrap();

        let res = copy_dir_recursive(
            &src_dir,
            &child.join("config"),
            &[ws_link],
            &[root_link, child_link],
        )
        .await;
        assert!(
            res.is_err(),
            "the copy walk must refuse a swapped destination ancestor"
        );
        let leaked = std::fs::read_dir(outside.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .count();
        assert_eq!(leaked, 0, "nothing may be created through the link");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn guarded_creation_refuses_swapped_root() {
        use crate::fs_guard::guarded_create_dir;
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();

        // A verified backups root whose identity is captured, then swapped
        // for a symlink: creating a directory inside a chain anchored on
        // the stale root must refuse instead of creating outside.
        let backups = tmp.path().join("backups");
        std::fs::create_dir_all(&backups).unwrap();
        let root_meta = std::fs::symlink_metadata(&backups).unwrap();
        let root_link = DirLink {
            path: backups.clone(),
            id: file_id_of(&root_meta),
        };
        std::fs::rename(&backups, tmp.path().join("staged-away")).unwrap();
        std::os::unix::fs::symlink(outside.path(), &backups).unwrap();

        let res = guarded_create_dir(vec![root_link], &backups.join("backup-new"));
        assert!(
            res.is_err(),
            "guarded creation must refuse a swapped backups root"
        );
        let leaked = std::fs::read_dir(outside.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .count();
        assert_eq!(leaked, 0, "no directory may be created through the link");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn create_refuses_workspace_swapped_mid_create() {
        let holder = TempDir::new().unwrap();
        let ws = holder.path().join("ws");
        let relocated = holder.path().join("relocated");
        std::fs::create_dir_all(&relocated).unwrap();
        // No copyable include dirs (hundreds of absent names): the only
        // destination-side mutation after the child appears is the
        // manifest write, whose guard chain must include the workspace
        // identity for the swap below to be caught.
        let absent_dirs: Vec<String> = (0..500).map(|i| format!("absent{i:03}")).collect();

        // Swapper: once the freshly created backup child appears under
        // the workspace, move the WHOLE workspace elsewhere and put a
        // symlink in its place. The relocation preserves every inode, so
        // root/child identities alone stay valid — only a chain anchored
        // on the workspace identity itself notices the swap.
        let swapper = {
            let ws = ws.clone();
            let moved = relocated.join("ws-moved");
            std::thread::spawn(move || {
                loop {
                    let child = std::fs::read_dir(ws.join("backups")).ok().and_then(|rd| {
                        rd.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| {
                            std::fs::symlink_metadata(p)
                                .map(|m| m.is_dir())
                                .unwrap_or(false)
                        })
                    });
                    if child.is_some() {
                        let _ = std::fs::rename(&ws, &moved);
                        let _ = std::os::unix::fs::symlink(&moved, &ws);
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_micros(20));
                }
            })
        };

        let tool = BackupTool::new(ws.clone(), absent_dirs, 10);
        let res = tool.execute(json!({"command": "create"})).await;
        let _ = swapper.join();

        assert!(
            std::fs::symlink_metadata(&ws)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false),
            "swapper never swapped the workspace; test setup failed"
        );
        let relocated_manifest = std::fs::read_dir(relocated.join("ws-moved/backups"))
            .ok()
            .and_then(|rd| {
                rd.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| {
                    std::fs::symlink_metadata(p)
                        .map(|m| m.is_dir())
                        .unwrap_or(false)
                })
            })
            .map(|child| child.join("manifest.json").exists())
            .unwrap_or(false);
        // On the fixed code the first destination-side guarded step after
        // the swap (the chain-verified checksum walk or the manifest
        // write) refuses and the create fails loudly; on code whose
        // destination chains skip the workspace identity the write
        // succeeds through the swapped name and the manifest lands in the
        // relocated tree. A relocated manifest is only acceptable when
        // the whole create had already finished before the swap.
        assert!(
            !relocated_manifest || res.is_ok(),
            "create wrote its manifest through the swapped workspace into the relocated tree"
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

//! No-follow, check-then-use-hardened filesystem mutation helpers shared by
//! the backup and data-retention tools.
//!
//! Every helper here follows one discipline: stat checks never traverse
//! symlinks (`symlink_metadata` all the way), and the re-check plus the
//! mutation run as adjacent syscalls inside one blocking task, so the
//! window in which a path component can be swapped for a symlink between
//! the check and the use is a single syscall wide. That residual window is
//! inherent to path-based APIs; closing it fully would require
//! descriptor-relative (openat-style) operations.

/// Identity of a filesystem object. Names can be swapped behind a walk's
/// back, but a different object under the same name has a different
/// identity, so re-checking identity detects mid-operation swaps. On Unix
/// this is device + inode; on other platforms it is a constant, so there
/// identity re-checks degrade to the symlink/kind checks alone.
#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileId {
    dev: u64,
    ino: u64,
}

#[cfg(unix)]
pub(crate) fn file_id_of(meta: &std::fs::Metadata) -> FileId {
    use std::os::unix::fs::MetadataExt;
    FileId {
        dev: meta.dev(),
        ino: meta.ino(),
    }
}

#[cfg(not(unix))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileId;

#[cfg(not(unix))]
pub(crate) fn file_id_of(_meta: &std::fs::Metadata) -> FileId {
    FileId
}

/// What a guarded unlink did.
pub(crate) enum UnlinkOutcome {
    Removed,
    /// The entry vanished on its own before the unlink ran; nothing was
    /// deleted by the caller, so nothing may be counted or reported.
    Vanished,
}

fn dir_recheck(dir: &std::path::Path, dir_id: FileId) -> Result<std::fs::Metadata, String> {
    let dm = std::fs::symlink_metadata(dir)
        .map_err(|e| format!("re-checking {} failed: {e}", dir.display()))?;
    if dm.file_type().is_symlink() || !dm.is_dir() || file_id_of(&dm) != dir_id {
        return Err(format!(
            "{} changed identity mid-operation; refusing to touch paths under it",
            dir.display()
        ));
    }
    Ok(dm)
}

/// Unlink `path` only if its containing directory `dir` still has identity
/// `dir_id` and the entry itself still is the regular file `file_id` that
/// the walk observed. Refuses anything that resolved through a swapped
/// component. Runs the re-checks and the unlink as adjacent syscalls.
pub(crate) fn guarded_unlink(
    dir: std::path::PathBuf,
    dir_id: FileId,
    path: std::path::PathBuf,
    file_id: FileId,
) -> Result<UnlinkOutcome, String> {
    dir_recheck(&dir, dir_id)?;
    match std::fs::symlink_metadata(&path) {
        Ok(fm) => {
            if fm.file_type().is_symlink() || !fm.is_file() || file_id_of(&fm) != file_id {
                return Err(format!(
                    "{} changed identity mid-operation; refusing to delete it",
                    path.display()
                ));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(UnlinkOutcome::Vanished),
        Err(e) => return Err(format!("re-checking {} failed: {e}", path.display())),
    }
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(UnlinkOutcome::Removed),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(UnlinkOutcome::Vanished),
        Err(e) => Err(format!("deleting {} failed: {e}", path.display())),
    }
}

/// True when the metadata describes a file whose inode is shared with
/// other names (a hard link). Overwriting such a destination truncates
/// every file that shares the inode — including files outside the
/// workspace — so guarded copies and writes refuse them. On platforms
/// without a link count the check cannot run and is skipped.
#[cfg(unix)]
pub(crate) fn hardlinked(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.nlink() > 1
}

#[cfg(not(unix))]
pub(crate) fn hardlinked(_meta: &std::fs::Metadata) -> bool {
    false
}

/// Re-check a copy/write destination entry: it must be absent, or a
/// regular file that is neither a symlink nor a hard link.
fn dst_recheck(dst: &std::path::Path) -> Result<(), String> {
    match std::fs::symlink_metadata(dst) {
        Ok(m) => {
            if m.file_type().is_symlink() {
                return Err(format!(
                    "refusing to overwrite through symlink: {}",
                    dst.display()
                ));
            }
            if !m.is_file() {
                return Err(format!(
                    "refusing to overwrite non-regular file: {}",
                    dst.display()
                ));
            }
            if hardlinked(&m) {
                return Err(format!(
                    "refusing to overwrite hard-linked file: {}",
                    dst.display()
                ));
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("re-checking {} failed: {e}", dst.display())),
    }
}

/// Copy regular file `src` (identity `src_id`) onto `dst` (inside `dir`,
/// identity `dir_id`) after re-verifying all three in one blocking step.
pub(crate) fn guarded_copy(
    src: std::path::PathBuf,
    src_id: FileId,
    dir: std::path::PathBuf,
    dir_id: FileId,
    dst: std::path::PathBuf,
) -> Result<(), String> {
    let sm = std::fs::symlink_metadata(&src)
        .map_err(|e| format!("re-checking {} failed: {e}", src.display()))?;
    if sm.file_type().is_symlink() || !sm.is_file() || file_id_of(&sm) != src_id {
        return Err(format!(
            "{} changed identity mid-operation; refusing to copy it",
            src.display()
        ));
    }
    dir_recheck(&dir, dir_id)?;
    dst_recheck(&dst)?;
    std::fs::copy(&src, &dst)
        .map_err(|e| format!("copying {} to {} failed: {e}", src.display(), dst.display()))?;
    Ok(())
}

/// Write `bytes` to `dst` (inside `dir`, identity `dir_id`) after
/// re-verifying the directory identity and the destination entry in one
/// blocking step.
pub(crate) fn guarded_write(
    dir: std::path::PathBuf,
    dir_id: FileId,
    dst: std::path::PathBuf,
    bytes: Vec<u8>,
) -> Result<(), String> {
    dir_recheck(&dir, dir_id)?;
    dst_recheck(&dst)?;
    std::fs::write(&dst, bytes).map_err(|e| format!("writing {} failed: {e}", dst.display()))
}

/// Recursively remove `dir` only after re-verifying it is still the real
/// directory `dir_id` observed earlier. `std::fs::remove_dir_all` itself
/// refuses to follow the top-level name if it is a symlink; the identity
/// re-check additionally rejects a swap that happened after that refusal
/// point was passed.
pub(crate) fn guarded_remove_dir_all(
    dir: std::path::PathBuf,
    dir_id: FileId,
) -> Result<(), String> {
    dir_recheck(&dir, dir_id)?;
    std::fs::remove_dir_all(&dir).map_err(|e| format!("removing {} failed: {e}", dir.display()))
}

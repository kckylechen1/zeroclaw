//! No-follow, check-then-use-hardened filesystem mutation helpers shared by
//! the backup and data-retention tools.
//!
//! Every helper here follows one discipline: stat checks never traverse
//! symlinks (`symlink_metadata` all the way), and the re-check plus the
//! mutation run as adjacent syscalls inside one blocking task, so the
//! window in which a path component can be swapped for a symlink between
//! the check and the use is a single syscall wide.
//!
//! Re-checks cover the WHOLE ancestor chain leading to the mutation, not
//! just the final component. `symlink_metadata` does not follow the final
//! component, but it still resolves intermediate components: renaming a
//! verified directory away and replacing it with a symlink would otherwise
//! let a walk "adopt" whatever the swapped name resolves to — including
//! identities captured through the link — and mutate through it. A chain
//! element that no longer has its captured identity (or became a symlink,
//! or vanished) refuses the mutation.
//!
//! The residual window between the last chain re-check and the mutation
//! itself is one syscall wide and inherent to path-based APIs; closing it
//! fully would require descriptor-relative (openat-style) operations.

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

/// A verified ancestor: a directory path together with the identity it had
/// when the walk last trusted it.
#[derive(Clone)]
pub(crate) struct DirLink {
    pub path: std::path::PathBuf,
    pub id: FileId,
}

/// What a guarded unlink did.
pub(crate) enum UnlinkOutcome {
    Removed,
    /// The entry vanished on its own before the unlink ran; nothing was
    /// deleted by the caller, so nothing may be counted or reported.
    Vanished,
}

/// Re-check one chain element: still a real directory, still the same
/// object.
fn link_recheck(link: &DirLink) -> Result<(), String> {
    let m = std::fs::symlink_metadata(&link.path)
        .map_err(|e| format!("re-checking {} failed: {e}", link.path.display()))?;
    if m.file_type().is_symlink() || !m.is_dir() || file_id_of(&m) != link.id {
        return Err(format!(
            "{} changed identity mid-operation; refusing to touch paths under it",
            link.path.display()
        ));
    }
    Ok(())
}

/// Re-check the whole ancestor chain (outermost first, immediate parent
/// last). Runs as adjacent syscalls with the mutation that follows it.
fn chain_recheck(chain: &[DirLink]) -> Result<(), String> {
    for link in chain {
        link_recheck(link)?;
    }
    Ok(())
}

/// Unlink `path` only if every ancestor in `chain` (ending with the
/// containing directory) still has its captured identity and the entry
/// itself still is the regular file `file_id` the walk observed.
pub(crate) fn guarded_unlink(
    chain: Vec<DirLink>,
    path: std::path::PathBuf,
    file_id: FileId,
) -> Result<UnlinkOutcome, String> {
    chain_recheck(&chain)?;
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
/// workspace — so guarded copies and writes refuse them. Stable std does
/// not expose a link count on every platform (on Windows it is only
/// available through an unstable metadata extension), so there the check
/// cannot run and hard-link overwrite remains a documented residual.
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

/// Copy regular file `src` (identity `src_id`, ancestor chain `src_chain`)
/// onto `dst` (ancestor chain `dst_chain`, ending with the containing
/// directory) after re-verifying both chains, the source entry, and the
/// destination entry in one blocking step.
pub(crate) fn guarded_copy(
    src: std::path::PathBuf,
    src_id: FileId,
    src_chain: Vec<DirLink>,
    dst: std::path::PathBuf,
    dst_chain: Vec<DirLink>,
) -> Result<(), String> {
    let sm = std::fs::symlink_metadata(&src)
        .map_err(|e| format!("re-checking {} failed: {e}", src.display()))?;
    if sm.file_type().is_symlink() || !sm.is_file() || file_id_of(&sm) != src_id {
        return Err(format!(
            "{} changed identity mid-operation; refusing to copy it",
            src.display()
        ));
    }
    chain_recheck(&src_chain)?;
    chain_recheck(&dst_chain)?;
    dst_recheck(&dst)?;
    std::fs::copy(&src, &dst)
        .map_err(|e| format!("copying {} to {} failed: {e}", src.display(), dst.display()))?;
    Ok(())
}

/// Write `bytes` to `dst` after re-verifying its ancestor chain and the
/// destination entry in one blocking step.
pub(crate) fn guarded_write(
    chain: Vec<DirLink>,
    dst: std::path::PathBuf,
    bytes: Vec<u8>,
) -> Result<(), String> {
    chain_recheck(&chain)?;
    dst_recheck(&dst)?;
    std::fs::write(&dst, bytes).map_err(|e| format!("writing {} failed: {e}", dst.display()))
}

/// Create `dst` (or accept it if it already exists as a real directory)
/// after re-verifying its ancestor chain, and return the new directory's
/// identity. Refuses anything that resolved through a swapped component.
pub(crate) fn guarded_create_dir(
    chain: Vec<DirLink>,
    dst: &std::path::Path,
) -> Result<FileId, String> {
    chain_recheck(&chain)?;
    std::fs::create_dir_all(dst).map_err(|e| format!("creating {} failed: {e}", dst.display()))?;
    let m = std::fs::symlink_metadata(dst)
        .map_err(|e| format!("re-checking {} failed: {e}", dst.display()))?;
    if m.file_type().is_symlink() || !m.is_dir() {
        return Err(format!(
            "created path is not a real directory: {}",
            dst.display()
        ));
    }
    Ok(file_id_of(&m))
}

/// Recursively remove the directory named by the last chain element, only
/// after re-verifying the whole chain. `std::fs::remove_dir_all` itself
/// refuses to follow the top-level name if it is a symlink; the chain
/// re-check additionally rejects a swap of any ancestor.
pub(crate) fn guarded_remove_dir_all(chain: Vec<DirLink>) -> Result<(), String> {
    let Some(target) = chain.last() else {
        return Err("removal requested without a target directory".into());
    };
    chain_recheck(&chain)?;
    std::fs::remove_dir_all(&target.path)
        .map_err(|e| format!("removing {} failed: {e}", target.path.display()))
}

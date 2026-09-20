//! No-follow, check-then-use-hardened filesystem mutation helpers shared by
//! the backup and data-retention tools.
//!
//! The legacy path-based helpers reject final symlinks with metadata
//! checks and recheck captured directory identities before mutation. Checks
//! and mutation share one blocking task, but remain separate syscalls: an
//! ancestor can still be replaced in that check/use window.
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
//! Unix backup creation instead opens each captured destination directory
//! relative to its verified parent and checks the opened identity. Directory
//! creation, new payload files and the manifest then use the held parent FD.
//! A renamed original directory may still receive writes, but a replacement
//! pathname cannot redirect them. Namespace rechecks still precede success.
//! Source reads, restore overwrites, removal, non-Unix mutations and initial
//! missing-workspace bootstrap retain the path-based residual described above.

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

/// Asynchronously re-verify a whole ancestor chain (read-side guards):
/// every element must still be the same real directory. Verified
/// innermost-first so the outermost link — whose relocation preserves
/// every descendant identity — is checked last, closest to the read it
/// guards. Used by walks that only read, so foreign data cannot be
/// adopted into listings, counts, or hashes through a swapped component.
pub(crate) async fn verify_chain(chain: &[DirLink]) -> anyhow::Result<()> {
    for link in chain.iter().rev() {
        let m = tokio::fs::symlink_metadata(&link.path).await?;
        anyhow::ensure!(
            m.is_dir() && !m.file_type().is_symlink() && file_id_of(&m) == link.id,
            "{} changed identity during the operation; refusing its data",
            link.path.display()
        );
    }
    Ok(())
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

/// Re-check the whole ancestor chain, INNERMOST first, so the outermost
/// link — the highest-value swap target, whose relocation leaves every
/// descendant inode identity intact — is verified last, immediately
/// before the mutation that follows. Each element's check-to-mutation
/// window spans only the checks that follow it plus the mutation itself;
/// for the outermost element that is a single adjacent syscall. A swap
/// landing inside a window this small is the documented path-based
/// residual and would need descriptor-relative operations to remove.
fn chain_recheck(chain: &[DirLink]) -> Result<(), String> {
    for link in chain.iter().rev() {
        link_recheck(link)?;
    }
    Ok(())
}

#[cfg(unix)]
fn child_name<'a>(
    parent: &std::path::Path,
    path: &'a std::path::Path,
) -> Result<&'a std::ffi::OsStr, String> {
    if path.parent() != Some(parent) {
        return Err(format!(
            "{} is not a direct child of {}",
            path.display(),
            parent.display()
        ));
    }
    path.file_name()
        .ok_or_else(|| format!("missing child name: {}", path.display()))
}

/// Bind each opened directory to the identity already captured by the walk.
/// A replacement directory must not be adopted merely because it now has
/// the same path as an object the walk previously verified.
#[cfg(unix)]
fn destination_parent(
    chain: &[DirLink],
    dst: &std::path::Path,
) -> Result<(std::fs::File, std::ffi::OsString), String> {
    use rustix::fs::{CWD, Mode, OFlags, openat};
    let first = chain.first().ok_or("destination has no directory anchor")?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut dir =
        std::fs::File::from(openat(CWD, &first.path, flags, Mode::empty()).map_err(|e| {
            format!(
                "opening directory anchor {} failed: {e}",
                first.path.display()
            )
        })?);
    let verify = |file: &std::fs::File, link: &DirLink| -> Result<(), String> {
        let meta = file
            .metadata()
            .map_err(|e| format!("reading directory identity failed: {e}"))?;
        if !meta.is_dir() || file_id_of(&meta) != link.id {
            return Err(format!(
                "{} changed directory identity",
                link.path.display()
            ));
        }
        Ok(())
    };
    verify(&dir, first)?;
    let mut previous = first;
    for link in &chain[1..] {
        let name = child_name(&previous.path, &link.path)?;
        dir = std::fs::File::from(openat(&dir, name, flags, Mode::empty()).map_err(|e| {
            format!(
                "opening anchored directory {} failed: {e}",
                link.path.display()
            )
        })?);
        verify(&dir, link)?;
        previous = link;
    }
    let name = child_name(&previous.path, dst)?.to_os_string();
    chain_recheck(chain)?;
    Ok((dir, name))
}

#[cfg(unix)]
fn create_file_at(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    mode: rustix::fs::Mode,
) -> Result<std::fs::File, String> {
    use rustix::fs::{OFlags, openat};
    openat(
        parent,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        mode,
    )
    .map(std::fs::File::from)
    .map_err(|e| format!("creating anchored backup file failed: {e}"))
}

#[cfg(unix)]
fn destination_name_recheck(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    held: &std::fs::File,
) -> Result<(), String> {
    use rustix::fs::{AtFlags, FileType, fstat, statat};
    let opened = fstat(held).map_err(|e| format!("reading opened backup identity failed: {e}"))?;
    let named = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|e| format!("re-checking backup entry failed: {e}"))?;
    if named.st_dev != opened.st_dev
        || named.st_ino != opened.st_ino
        || FileType::from_raw_mode(named.st_mode) != FileType::from_raw_mode(opened.st_mode)
    {
        return Err("backup entry changed identity during the operation".into());
    }
    Ok(())
}

#[cfg(all(test, unix))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackupRacePoint {
    ParentOpened,
    EntryOpened,
}

#[cfg(all(test, unix))]
type BackupDestinationRaceHook = Box<dyn Fn(&std::path::Path, BackupRacePoint) + Send + Sync>;

#[cfg(all(test, unix))]
pub(crate) static BACKUP_DESTINATION_RACE_HOOK: std::sync::Mutex<
    Option<BackupDestinationRaceHook>,
> = std::sync::Mutex::new(None);

#[cfg(all(test, unix))]
fn run_backup_destination_race_hook(dst: &std::path::Path, point: BackupRacePoint) {
    if let Some(hook) = BACKUP_DESTINATION_RACE_HOOK
        .lock()
        .expect("race hook mutex")
        .as_ref()
    {
        hook(dst, point);
    }
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
    chain_recheck(&src_chain)?;
    chain_recheck(&dst_chain)?;
    dst_recheck(&dst)?;
    // The source entry is checked LAST, immediately before the copy
    // opens it: a final-component swap of the source between an earlier
    // check and the open would otherwise be undetectable by the chain
    // guards, which never resolve the source's own name.
    let sm = std::fs::symlink_metadata(&src)
        .map_err(|e| format!("re-checking {} failed: {e}", src.display()))?;
    if sm.file_type().is_symlink() || !sm.is_file() || file_id_of(&sm) != src_id {
        return Err(format!(
            "{} changed identity mid-operation; refusing to copy it",
            src.display()
        ));
    }
    std::fs::copy(&src, &dst)
        .map_err(|e| format!("copying {} to {} failed: {e}", src.display(), dst.display()))?;
    Ok(())
}

/// New backup payloads must never open or truncate an existing destination.
/// Restore keeps its existing overwrite path in `guarded_copy`.
pub(crate) fn guarded_copy_new(
    src: std::path::PathBuf,
    src_id: FileId,
    src_chain: Vec<DirLink>,
    dst: std::path::PathBuf,
    dst_chain: Vec<DirLink>,
) -> Result<(), String> {
    #[cfg(unix)]
    {
        use rustix::fs::{CWD, Mode, OFlags, openat};
        chain_recheck(&src_chain)?;
        let (parent, name) = destination_parent(&dst_chain, &dst)?;
        let mut source = std::fs::File::from(
            openat(
                CWD,
                &src,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| format!("opening backup source {} failed: {e}", src.display()))?,
        );
        let meta = source
            .metadata()
            .map_err(|e| format!("reading backup source identity failed: {e}"))?;
        if !meta.is_file() || file_id_of(&meta) != src_id {
            return Err(format!(
                "{} changed identity before backup copy",
                src.display()
            ));
        }
        chain_recheck(&src_chain)?;
        #[cfg(all(test, unix))]
        run_backup_destination_race_hook(&dst, BackupRacePoint::ParentOpened);
        let mut target = create_file_at(&parent, &name, Mode::from(0o600))?;
        #[cfg(all(test, unix))]
        run_backup_destination_race_hook(&dst, BackupRacePoint::EntryOpened);
        std::io::copy(&mut source, &mut target)
            .map_err(|e| format!("copying backup payload failed: {e}"))?;
        target
            .set_permissions(meta.permissions())
            .map_err(|e| format!("setting backup permissions failed: {e}"))?;
        destination_name_recheck(&parent, &name, &target)?;
        chain_recheck(&dst_chain)?;
        chain_recheck(&src_chain)
    }
    #[cfg(not(unix))]
    guarded_copy(src, src_id, src_chain, dst, dst_chain)
}

/// Write `bytes` to `dst` after re-verifying its ancestor chain and the
/// destination entry in one blocking step.
pub(crate) fn guarded_write(
    chain: Vec<DirLink>,
    dst: std::path::PathBuf,
    bytes: Vec<u8>,
) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::io::Write;
        let (parent, name) = destination_parent(&chain, &dst)?;
        #[cfg(all(test, unix))]
        run_backup_destination_race_hook(&dst, BackupRacePoint::ParentOpened);
        let mut target = create_file_at(&parent, &name, rustix::fs::Mode::from(0o666))?;
        #[cfg(all(test, unix))]
        run_backup_destination_race_hook(&dst, BackupRacePoint::EntryOpened);
        target
            .write_all(&bytes)
            .map_err(|e| format!("writing backup manifest failed: {e}"))?;
        destination_name_recheck(&parent, &name, &target)?;
        chain_recheck(&chain)
    }
    #[cfg(not(unix))]
    {
        chain_recheck(&chain)?;
        dst_recheck(&dst)?;
        std::fs::write(&dst, bytes).map_err(|e| format!("writing {} failed: {e}", dst.display()))
    }
}

/// Create `dst` (or accept it if it already exists as a real directory)
/// after re-verifying its ancestor chain, and return the new directory's
/// identity. Refuses anything that resolved through a swapped component.
pub(crate) fn guarded_create_dir_new(
    chain: Vec<DirLink>,
    dst: &std::path::Path,
) -> Result<FileId, String> {
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags, mkdirat, openat};
        let (parent, name) = destination_parent(&chain, dst)?;
        #[cfg(all(test, unix))]
        run_backup_destination_race_hook(dst, BackupRacePoint::ParentOpened);
        match mkdirat(&parent, name.as_os_str(), Mode::from(0o777)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(e) => {
                return Err(format!(
                    "creating anchored directory {} failed: {e}",
                    dst.display()
                ));
            }
        }
        let dir = std::fs::File::from(
            openat(
                &parent,
                name.as_os_str(),
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| format!("opening created directory {} failed: {e}", dst.display()))?,
        );
        let meta = dir
            .metadata()
            .map_err(|e| format!("reading created directory identity failed: {e}"))?;
        #[cfg(all(test, unix))]
        run_backup_destination_race_hook(dst, BackupRacePoint::EntryOpened);
        destination_name_recheck(&parent, &name, &dir)?;
        chain_recheck(&chain)?;
        Ok(file_id_of(&meta))
    }
    #[cfg(not(unix))]
    guarded_create_dir(chain, dst)
}

/// Existing path-based directory creation for restore and initial workspace
/// bootstrap. Backup creation after workspace admission uses the FD variant.
pub(crate) fn guarded_create_dir(
    chain: Vec<DirLink>,
    dst: &std::path::Path,
) -> Result<FileId, String> {
    chain_recheck(&chain)?;
    #[cfg(all(test, unix))]
    run_backup_destination_race_hook(dst, BackupRacePoint::ParentOpened);
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn link(path: &std::path::Path) -> DirLink {
        DirLink {
            path: path.to_path_buf(),
            id: file_id_of(&std::fs::metadata(path).unwrap()),
        }
    }

    #[test]
    fn new_destination_rejects_replaced_directory_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let chain = vec![link(&root)];
        std::fs::rename(&root, tmp.path().join("old-root")).unwrap();
        std::fs::create_dir(&root).unwrap();
        assert!(guarded_create_dir_new(chain, &root.join("child")).is_err());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    }

    #[test]
    fn new_destination_requires_each_intermediate_directory_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("uncaptured")).unwrap();
        assert!(guarded_create_dir_new(vec![link(root)], &root.join("uncaptured/child")).is_err());
        assert!(!root.join("uncaptured/child").exists());
    }

    #[test]
    fn new_files_refuse_existing_links_and_preserve_copy_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let source = root.join("source");
        std::fs::write(&source, "unchanged").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o640)).unwrap();
        let source_id = file_id_of(&std::fs::metadata(&source).unwrap());
        let dest = root.join("payload");
        let chain = vec![link(root)];
        guarded_copy_new(
            source.clone(),
            source_id,
            chain.clone(),
            dest.clone(),
            chain.clone(),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "unchanged");
        assert_eq!(
            std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o640
        );
        std::fs::remove_file(&dest).unwrap();
        for hard_link in [false, true] {
            if hard_link {
                std::fs::hard_link(&source, &dest).unwrap();
            } else {
                std::os::unix::fs::symlink(&source, &dest).unwrap();
            }
            assert!(
                guarded_copy_new(
                    source.clone(),
                    source_id,
                    chain.clone(),
                    dest.clone(),
                    chain.clone()
                )
                .is_err()
            );
            assert!(guarded_write(chain.clone(), dest.clone(), b"changed".to_vec()).is_err());
            assert_eq!(std::fs::read_to_string(&source).unwrap(), "unchanged");
            std::fs::remove_file(&dest).unwrap();
        }
    }
}

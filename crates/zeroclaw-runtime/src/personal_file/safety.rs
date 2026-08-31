//! Descriptor-bound containment core for the `personal_file` domain.
//!
//! Unix only. The safety proof is structural and anchored in kernel
//! primitives, not in lexical checks:
//!
//! - **Every operation walks from the admitted root's open directory
//!   descriptor**, `openat`-style, one component at a time with
//!   `O_NOFOLLOW | O_DIRECTORY`. A component that is (or becomes) a
//!   symlink fails the open instead of resolving through it. Because
//!   each step holds a real descriptor, later resolution is pinned to
//!   inodes: renaming an ancestor mid-walk cannot redirect the walk.
//!   This closes the check-then-use race class that path-based guards
//!   (including the tools-crate `fs_guard` helpers, which re-verify
//!   path identities instead) can only narrow.
//! - **The admitted root is captured once**: opened no-follow, identity
//!   (device + inode) recorded at admission and re-verified against the
//!   held descriptor before every operation. The canonical path string
//!   is kept for display only and is never opened through again.
//! - **Hard-link containment**: any file targeted for mutation must have
//!   `nlink == 1`; a shared inode answers [`PersonalFileRefusal::Hardlinked`].
//! - **Git exclusion runs before mutation**: the root's canonical
//!   ancestry is scanned at admission (a write root inside any repository
//!   or worktree is not admissible), and every ancestor directory of a
//!   mutation target is probed for a `.git` directory or `.git` worktree
//!   file at operation time. `.git` as a path component is rejected at
//!   the type boundary.
//! - **Unsupported platforms fail closed**: this module is compiled only
//!   on Unix (`cfg(unix)`); admission on any other platform answers
//!   [`PersonalFileError::UnsupportedSafely`] from the service and never
//!   degrades to string containment.
//!
//! The residual exposure is the kernel-defined atomicity of each single
//! syscall (`openat`, `mkdirat`, `renameat`, `renameat2`/`renamex_np`);
//! there is no window in which this code reasons about a path it does
//! not hold a descriptor for.
//!
//! ## Test-only race hooks
//!
//! `RACE_HOOK` exists only under `cfg(test)`. It lets the discrimination
//! tests run attacker steps (ancestor/leaf swaps) exactly between the
//! verification and the mutation of a production ordering. It is not
//! compiled into production builds.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rustix::fd::OwnedFd;
use rustix::fs::{
    AtFlags, CWD, FileType, Mode, OFlags, RenameFlags, Stat, fstat, fsync, mkdirat, openat,
    renameat, renameat_with, statat, unlinkat,
};

use crate::personal_file::PersonalRelativePath;
use crate::personal_file::domain::{
    ObjectId, PersonalFileError, PersonalFileRefusal, RootInner, TRASH_NAMESPACE,
};

pub(crate) type SafetyResult<T> = Result<T, PersonalFileError>;

impl ObjectId {
    pub(crate) fn of(stat: &Stat) -> Self {
        // Raw stat field widths differ per Unix platform (e.g. `i32`
        // dev on macOS, `u64` on Linux): the cast widens where needed
        // and is a same-type no-op elsewhere, hence the allowance.
        #[allow(clippy::unnecessary_cast)]
        let (device, inode) = (stat.st_dev as u64, stat.st_ino as u64);
        Self { device, inode }
    }
}

fn errno_to_io(error: rustix::io::Errno) -> PersonalFileError {
    PersonalFileError::Io(std::io::Error::from(error))
}

/// Test-only race-injection point (see module docs; the admitted-root
/// anchor type lives in `domain::RootInner` so the typed surface
/// compiles on platforms where this module is absent). Never compiled
/// into production builds.
#[cfg(test)]
pub(crate) static RACE_HOOK: std::sync::Mutex<Option<Box<dyn Fn() + Send + Sync>>> =
    std::sync::Mutex::new(None);

/// Run the test-only race hook if one is installed. In production builds
/// this is an empty function body.
#[cfg(test)]
pub(crate) fn run_race_hook() {
    let guard = RACE_HOOK.lock().expect("race hook mutex");
    if let Some(function) = guard.as_ref() {
        function();
    }
}

#[cfg(not(test))]
pub(crate) fn run_race_hook() {}

/// Open flags used for every directory traversal step: read-only, must
/// already be a directory, never follow a final symlink, close-on-exec.
fn dir_oflags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

fn file_type_of(stat: &Stat) -> FileType {
    FileType::from_raw_mode(stat.st_mode)
}

/// Verify the held root descriptor still has the identity captured at
/// admission (root swap / descriptor recycling guard).
pub(crate) fn verify_root_identity(root: &RootInner) -> SafetyResult<()> {
    let stat = fstat(&root.dir).map_err(errno_to_io)?;
    if file_type_of(&stat) != FileType::Directory || ObjectId::of(&stat) != root.identity {
        return Err(PersonalFileRefusal::RootIdentityChanged.into());
    }
    Ok(())
}

fn dup_fd(fd: &OwnedFd) -> SafetyResult<OwnedFd> {
    rustix::io::dup(fd).map_err(errno_to_io)
}

/// Walk the parent chain of `path` component-by-component from the root
/// descriptor and return the leaf's parent directory descriptor.
///
/// - every open is `O_NOFOLLOW | O_DIRECTORY`: a swapped-in symlink
///   component answers [`PersonalFileRefusal::Symlink`], never resolves;
/// - `creating` allows missing ancestors to be created with `mkdirat`
///   (mode 0700) and then opened no-follow;
/// - `scan_git` probes every walked directory for a `.git` directory or
///   `.git` worktree file before it is used for a mutation.
pub(crate) fn walk_parents(
    root: &RootInner,
    path: &PersonalRelativePath,
    creating: bool,
    scan_git: bool,
) -> SafetyResult<OwnedFd> {
    verify_root_identity(root)?;
    let components = path.components();
    let parent_count = components.len() - 1;
    let mut current = dup_fd(&root.dir)?;

    for component in components.iter().take(parent_count) {
        let dir = match openat(&current, component.as_str(), dir_oflags(), Mode::empty()) {
            Ok(dir) => dir,
            Err(rustix::io::Errno::NOENT) if creating => {
                mkdirat(&current, component.as_str(), Mode::from(0o700)).map_err(errno_to_io)?;
                openat(&current, component.as_str(), dir_oflags(), Mode::empty())
                    .map_err(|error| map_walk_error(error, path))?
            }
            Err(error) => return Err(map_walk_error(error, path)),
        };
        // The freshly held directory is one of the target's ancestors:
        // probe it for a `.git` entry before it is used any further.
        if scan_git {
            probe_git_entry(&dir, &format!("{path} (at ancestor {component})"))?;
        }
        current = dir;
    }
    Ok(current)
}

fn map_walk_error(error: rustix::io::Errno, path: &PersonalRelativePath) -> PersonalFileError {
    match error {
        rustix::io::Errno::NOENT => PersonalFileError::NotFound(path.to_string()),
        rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR => PersonalFileRefusal::Symlink {
            path: path.to_string(),
        }
        .into(),
        _ => errno_to_io(error),
    }
}

/// Probe one directory (by descriptor) for a `.git` directory or `.git`
/// worktree file. A hit refuses the operation.
fn probe_git_entry(dir: &OwnedFd, display: &str) -> SafetyResult<()> {
    match statat(dir, ".git", AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => Err(PersonalFileRefusal::GitRepository {
            at: display.to_string(),
        }
        .into()),
        Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(error) => Err(errno_to_io(error)),
    }
}

/// Probe the root directory itself (the first ancestor of every target).
pub(crate) fn probe_git_at_root(root: &RootInner) -> SafetyResult<()> {
    probe_git_entry(&root.dir, &format!("{} (at root)", root.canonical_display))
}

/// `statat` no-follow. `None` means the entry does not exist.
pub(crate) fn stat_leaf(parent: &OwnedFd, name: &str) -> SafetyResult<Option<Stat>> {
    match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(stat)),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(error) => Err(errno_to_io(error)),
    }
}

/// Require the entry to be a regular file. A symlink answers
/// [`PersonalFileRefusal::Symlink`]; any other non-regular object
/// answers [`PersonalFileRefusal::NotRegularFile`]. `Ok(None)` means
/// not-found.
pub(crate) fn require_regular_file(
    parent: &OwnedFd,
    name: &str,
    display: &str,
) -> SafetyResult<Option<Stat>> {
    match stat_leaf(parent, name)? {
        None => Ok(None),
        Some(stat) => {
            let kind = FileType::from_raw_mode(stat.st_mode);
            if kind == FileType::Symlink {
                return Err(PersonalFileRefusal::Symlink {
                    path: display.to_string(),
                }
                .into());
            }
            if kind != FileType::RegularFile {
                return Err(PersonalFileRefusal::NotRegularFile {
                    path: display.to_string(),
                }
                .into());
            }
            Ok(Some(stat))
        }
    }
}

/// Refuse shared inodes for any mutating file operation: overwriting or
/// removing a hard-linked name would corrupt a foreign identity.
pub(crate) fn require_unshared_inode(stat: &Stat, display: &str) -> SafetyResult<()> {
    // st_nlink width differs per platform; see ObjectId::of.
    #[allow(clippy::unnecessary_cast)]
    let links = stat.st_nlink as u64;
    if links != 1 {
        return Err(PersonalFileRefusal::Hardlinked {
            path: display.to_string(),
        }
        .into());
    }
    Ok(())
}

/// Open a leaf no-follow under `parent`. A symlink final component fails
/// with `ELOOP` before any content is touched.
pub(crate) fn open_leaf(
    parent: &OwnedFd,
    name: &str,
    write: bool,
    create_exclusive: bool,
) -> Result<OwnedFd, rustix::io::Errno> {
    let mut flags = OFlags::CLOEXEC | OFlags::NOFOLLOW;
    if write {
        flags |= OFlags::WRONLY;
    } else {
        flags |= OFlags::RDONLY;
    }
    if create_exclusive {
        flags |= OFlags::CREATE | OFlags::EXCL;
    }
    openat(parent, name, flags, Mode::from(0o600))
}

/// Open a leaf as a directory, no-follow (`O_DIRECTORY | O_NOFOLLOW`).
pub(crate) fn open_leaf_dir(parent: &OwnedFd, name: &str) -> Result<OwnedFd, rustix::io::Errno> {
    openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

/// Read a bounded regular file completely from an already-open
/// descriptor. The size bound is enforced both from the descriptor's
/// metadata and from the actual read.
pub(crate) fn read_bounded(file: OwnedFd, limit: u64) -> SafetyResult<Vec<u8>> {
    let stat = fstat(&file).map_err(errno_to_io)?;
    let declared = stat.st_size as u64;
    if declared > limit {
        return Err(PersonalFileError::TooLarge {
            limit,
            actual: declared,
        });
    }
    let mut file = std::fs::File::from(file);
    let mut buffer = Vec::with_capacity(declared as usize);
    file.read_to_end(&mut buffer)
        .map_err(PersonalFileError::Io)?;
    #[allow(clippy::unnecessary_cast)]
    let read = buffer.len() as u64;
    if read > limit {
        return Err(PersonalFileError::TooLarge {
            limit,
            actual: read,
        });
    }
    Ok(buffer)
}

/// Write bytes to a freshly created exclusive (no-clobber, no-follow)
/// file under `parent`, fsync it, and close it. On any failure the
/// staged name is removed.
pub(crate) fn write_exclusive_file(parent: &OwnedFd, name: &str, bytes: &[u8]) -> SafetyResult<()> {
    let file = match open_leaf(parent, name, true, true) {
        Ok(file) => file,
        Err(error) => return Err(errno_to_io(error)),
    };
    let outcome = (|| -> SafetyResult<()> {
        let mut file = std::fs::File::from(file);
        file.write_all(bytes).map_err(PersonalFileError::Io)?;
        file.flush().map_err(PersonalFileError::Io)?;
        file.sync_all().map_err(PersonalFileError::Io)
    })();
    match outcome {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = unlinkat(parent, name, AtFlags::empty());
            Err(error)
        }
    }
}

/// fsync a directory descriptor (durability of a name change).
pub(crate) fn fsync_dir(dir: &OwnedFd) -> SafetyResult<()> {
    fsync(dir).map_err(errno_to_io)
}

/// Best-effort fsync after a completed rename-class mutation.
pub(crate) fn fsync_dir_best_effort(dir: &OwnedFd) {
    let _ = fsync(dir);
}

/// Open (creating with mode 0700 if needed) the reserved root-local
/// trash directory and verify it is a real directory inside the root.
pub(crate) fn open_trash(root: &RootInner) -> SafetyResult<OwnedFd> {
    match openat(&root.dir, TRASH_NAMESPACE, dir_oflags(), Mode::empty()) {
        Ok(dir) => Ok(dir),
        Err(rustix::io::Errno::NOENT) => {
            mkdirat(&root.dir, TRASH_NAMESPACE, Mode::from(0o700)).map_err(|error| {
                PersonalFileRefusal::TrashUnavailable {
                    reason: format!("creating the reserved trash dir failed: {error}"),
                }
            })?;
            openat(&root.dir, TRASH_NAMESPACE, dir_oflags(), Mode::empty()).map_err(|error| {
                PersonalFileRefusal::TrashUnavailable {
                    reason: format!("reopening the reserved trash dir failed: {error}"),
                }
                .into()
            })
        }
        Err(rustix::io::Errno::LOOP) => Err(PersonalFileRefusal::TrashUnavailable {
            reason: "the reserved trash name is occupied by a non-directory".to_string(),
        }
        .into()),
        Err(error) => Err(errno_to_io(error)),
    }
}

/// Create a fresh per-operation slot directory inside the trash and
/// return `(slot name, slot descriptor)`. Slot names are freshly minted
/// per operation and re-minted on the (astronomically unlikely) `EEXIST`,
/// so the following rename can never clobber: collision handling is
/// deterministic by construction.
pub(crate) fn fresh_trash_slot(trash: &OwnedFd) -> SafetyResult<(String, OwnedFd)> {
    for _ in 0..8 {
        let slot = uuid::Uuid::new_v4().to_string();
        match mkdirat(trash, slot.as_str(), Mode::from(0o700)) {
            Ok(()) => {
                let dir =
                    openat(trash, slot.as_str(), dir_oflags(), Mode::empty()).map_err(|error| {
                        PersonalFileRefusal::TrashUnavailable {
                            reason: format!("opening the fresh trash slot failed: {error}"),
                        }
                    })?;
                return Ok((slot, dir));
            }
            Err(rustix::io::Errno::EXIST) => continue,
            Err(error) => {
                return Err(PersonalFileRefusal::TrashUnavailable {
                    reason: format!("creating the trash slot failed: {error}"),
                }
                .into());
            }
        }
    }
    Err(PersonalFileRefusal::TrashUnavailable {
        reason: "could not mint a fresh trash slot".to_string(),
    }
    .into())
}

/// Rename `leaf` (under `parent`) into the freshly minted empty `slot`.
/// No-clobber by construction.
pub(crate) fn move_into_trash(
    parent: &OwnedFd,
    leaf: &str,
    slot_dir: &OwnedFd,
) -> SafetyResult<()> {
    renameat_with(parent, leaf, slot_dir, leaf, RenameFlags::NOREPLACE).map_err(|error| {
        PersonalFileRefusal::TrashUnavailable {
            reason: format!("moving into the trash failed: {error}"),
        }
        .into()
    })
}

/// Rename with no-clobber semantics inside one filesystem (Linux
/// `RENAME_NOREPLACE`, macOS `RENAME_EXCL` underneath).
pub(crate) fn rename_no_clobber(
    src_parent: &OwnedFd,
    src_leaf: &str,
    dst_parent: &OwnedFd,
    dst_leaf: &str,
) -> Result<(), rustix::io::Errno> {
    renameat_with(
        src_parent,
        src_leaf,
        dst_parent,
        dst_leaf,
        RenameFlags::NOREPLACE,
    )
}

/// Plain same-directory publication rename (used by replace, which
/// verified the expected identity immediately before).
pub(crate) fn rename_publish(
    parent: &OwnedFd,
    staged: &str,
    leaf: &str,
) -> Result<(), rustix::io::Errno> {
    renameat(parent, staged, parent, leaf)
}

/// Remove a staged sibling after a failed publication (best effort; the
/// staged name is freshly minted so this unlink cannot hit foreign data).
pub(crate) fn remove_staged(parent: &OwnedFd, staged: &str) {
    let _ = unlinkat(parent, staged, AtFlags::empty());
}

/// Admission-time validation and capture of a root path:
///
/// 1. the caller path must be absolute and lexically normal (no `..`,
///    no `.` components);
/// 2. the root itself must not be a symlink: the entry is checked
///    no-follow, and the subsequent open is `O_NOFOLLOW | O_DIRECTORY`,
///    so a symlinked final component refuses admission instead of
///    silently admitting the redirect target. Symlinked OS-level
///    *ancestors* above the root (e.g. `/var` -> `/private/var` on
///    macOS) are resolved by canonicalization and recorded in the
///    canonical display — the admitted object is the resolved directory,
///    and the held descriptor plus per-operation identity re-verification
///    keep it pinned even if any name above it is later swapped;
/// 3. the resolved root must be a real directory.
///
/// On any platform without descriptor primitives this answers
/// [`PersonalFileError::UnsupportedSafely`] (fail closed).
pub(crate) fn admit_root(dir: &Path) -> Result<(String, OwnedFd, ObjectId), PersonalFileError> {
    {
        use std::path::Component;

        let caller = dir
            .to_str()
            .ok_or::<PersonalFileError>(
                PersonalFileRefusal::RelativeRoot {
                    path: dir.display().to_string(),
                }
                .into(),
            )?
            .to_string();
        if !dir.is_absolute() {
            return Err(PersonalFileRefusal::RelativeRoot { path: caller }.into());
        }
        for component in dir.components() {
            if matches!(component, Component::CurDir | Component::ParentDir) {
                return Err(PersonalFileRefusal::RelativeRoot { path: caller }.into());
            }
        }
        // The root itself must not be a symlink (no-follow stat; the
        // no-follow open below re-proves it atomically).
        let root_meta = std::fs::symlink_metadata(dir).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => PersonalFileError::NotFound(caller.clone()),
            _ => PersonalFileError::Io(error),
        })?;
        if root_meta.file_type().is_symlink() {
            return Err(PersonalFileRefusal::SymlinkedRoot { path: caller }.into());
        }
        // Resolve OS ancestors for audit display and the Git scan. The
        // descriptor below is bound to this resolved directory.
        let canonical = std::fs::canonicalize(dir).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => PersonalFileError::NotFound(caller.clone()),
            _ => PersonalFileError::Io(error),
        })?;
        let opened =
            openat(CWD, canonical.as_os_str(), dir_oflags(), Mode::empty()).map_err(|error| {
                match error {
                    rustix::io::Errno::LOOP => PersonalFileRefusal::SymlinkedRoot {
                        path: caller.clone(),
                    }
                    .into(),
                    rustix::io::Errno::NOENT => PersonalFileError::NotFound(caller.clone()),
                    rustix::io::Errno::NOTDIR => PersonalFileRefusal::NotRegularFile {
                        path: caller.clone(),
                    }
                    .into(),
                    other => errno_to_io(other),
                }
            })?;
        let stat = fstat(&opened).map_err(errno_to_io)?;
        if file_type_of(&stat) != FileType::Directory {
            return Err(PersonalFileRefusal::NotRegularFile { path: caller }.into());
        }
        Ok((canonical.display().to_string(), opened, ObjectId::of(&stat)))
    }
}

/// Admission-time Git scan for mutation roots: a root at or under a
/// repository or worktree (any ancestor holding a `.git` directory or a
/// `.git` worktree file) is refused before the root can ever be used.
/// Read-only source roots are exempt (reading sources is their purpose);
/// `.git` itself remains unreachable as a path component at the type
/// boundary for every operation.
pub(crate) fn git_poisoned_above(canonical: &Path) -> Option<PathBuf> {
    let mut current = canonical.to_path_buf();
    loop {
        let candidate = current.join(".git");
        if std::fs::symlink_metadata(&candidate)
            .map(|metadata| metadata.is_dir() || metadata.is_file())
            .unwrap_or(false)
        {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Display string for a root-relative path (audit/refusal messages).
pub(crate) fn display_path(root: &RootInner, path: &PersonalRelativePath) -> String {
    format!("{}//{path}", root.canonical_display)
}

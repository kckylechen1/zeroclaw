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
//!   inodes: a symlink swap of any ancestor or leaf cannot redirect the
//!   walk. This closes the check-then-use race class that path-based
//!   guards (including the tools-crate `fs_guard` helpers, which
//!   re-verify path identities instead) can only narrow.
//! - **Ancestry is verified, not assumed.** A directory descriptor pins
//!   an inode, not its path: renaming an interior directory would let
//!   writes follow the inode to its new (attacker-chosen) location.
//!   Every opened child therefore re-proves its `..` entry against the
//!   parent descriptor's identity while walking, and the root proves
//!   its own `..` against the parent identity captured at admission on
//!   every operation. A relocated directory answers a typed refusal
//!   instead of inheriting the root's authority.
//! - **The admitted root is captured once**: the canonical path is
//!   walked component-by-component with no-follow opens at admission
//!   (canonicalization itself resolves the caller path by design — the
//!   ground-truth caveat is documented at `admit_root`), identity
//!   (device + inode) and parent identity are recorded, and both are
//!   re-verified against the held descriptor before every operation.
//!   The canonical path string is kept for display only and is never
//!   opened through again.
//! - **Staged writes are verified on their held descriptor.** Every
//!   intermediate write (new file, replacement content, trash recovery
//!   copy) is created under a freshly minted name (not derivable from
//!   the target name alone; recovery names sit inside a freshly minted
//!   slot), written through a held descriptor, fsynced, and checked for
//!   `nlink == 1` on that held descriptor before publication. A link
//!   planted between the check and the publication is a documented
//!   timing window (below), not a silent success.
//! - **Hard-link containment**: any file targeted for mutation must
//!   have `nlink == 1`; a shared inode answers
//!   [`PersonalFileRefusal::Hardlinked`].
//! - **Git exclusion runs before mutation**: the root's canonical
//!   ancestry is scanned at admission (a write root inside any
//!   repository or worktree is not admissible), and the root plus every
//!   ancestor directory of a mutation target is probed for a `.git`
//!   directory or `.git` worktree file after it is held (so the probe
//!   applies to the exact object used, not to a name). `.git` as a path
//!   component is rejected at the type boundary.
//! - **Unsupported platforms fail closed**: this module is compiled only
//!   on Unix (`cfg(unix)`); admission on any other platform answers
//!   [`PersonalFileError::UnsupportedSafely`] from the service and never
//!   degrades to string containment.
//!
//! ## Adversary model and documented residual
//!
//! The adversary this core defends against can plant symlinks and swap
//! path names on directories it can write — including (for the classic
//! TOCTOU family) ancestors of an admitted root. It cannot write inside
//! the kernel, and every capability it needs to win a remaining race is
//! also, directly, a broader capability (a process able to rename
//! objects inside the admitted root already has write access to the
//! user's personal files by the same token).
//!
//! Against that adversary these invariants hold structurally: every
//! operation resolves paths only through no-follow component opens of
//! held descriptors (admission resolves the caller path to its
//! canonical form first — see `admit_root` for the ground-truth
//! caveat); foreign inodes (`nlink != 1`) and `.git` metadata are
//! refused as mutation targets; refused and failed operations roll
//! back what they created (trash slots are removed, best effort, only
//! while the name still names the object bound at creation); staged
//! content is fsynced and checked unshared on its held descriptor
//! before publication.
//!
//! The residual is timing, not traversal: between any verification and
//! its adjacent mutation syscall a sufficiently lucky in-root rename
//! can substitute an object. Move and delete re-verify immediately
//! before their `renameat` to make that window one syscall wide; the
//! staged-write path spans write + fsync before its publication rename;
//! and `.git`/ancestor probes are not repeated after the walk. Closing
//! these windows entirely is not possible with pathname-based
//! primitives — it would require renaming to be impossible outside this
//! process — and every window is already narrower than the adversary's
//! own direct write authority over the same directories.
//!
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

fn identity_of(fd: &OwnedFd) -> SafetyResult<ObjectId> {
    let stat = fstat(fd).map_err(errno_to_io)?;
    if file_type_of(&stat) != FileType::Directory {
        return Err(PersonalFileRefusal::RootIdentityChanged.into());
    }
    Ok(ObjectId::of(&stat))
}

/// Verify `child`'s `..` entry still resolves to the object identified
/// by `parent_id`: the ancestry of a held descriptor is re-proved so a
/// renamed interior directory cannot silently relocate a subtree (and
/// its writes) to an attacker-chosen location.
fn verify_parent_link(child: &OwnedFd, parent_id: ObjectId) -> SafetyResult<()> {
    let parent = openat(child, "..", dir_oflags(), Mode::empty()).map_err(map_link_error)?;
    let stat = fstat(&parent).map_err(errno_to_io)?;
    if file_type_of(&stat) != FileType::Directory || ObjectId::of(&stat) != parent_id {
        return Err(PersonalFileRefusal::ConcurrentModification {
            path: "held directory ancestry".to_string(),
        }
        .into());
    }
    Ok(())
}

fn map_link_error(error: rustix::io::Errno) -> PersonalFileError {
    match error {
        rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR => {
            PersonalFileRefusal::ConcurrentModification {
                path: "held directory ancestry".to_string(),
            }
            .into()
        }
        _ => errno_to_io(error),
    }
}

/// Verify the held root descriptor still has the identity captured at
/// admission AND is still linked under the parent it was admitted
/// under (root swap, relocation, and descriptor-recycling guard).
pub(crate) fn verify_root_identity(root: &RootInner) -> SafetyResult<()> {
    let stat = fstat(&root.dir).map_err(errno_to_io)?;
    if file_type_of(&stat) != FileType::Directory || ObjectId::of(&stat) != root.identity {
        return Err(PersonalFileRefusal::RootIdentityChanged.into());
    }
    if verify_parent_link(&root.dir, root.parent_identity).is_err() {
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
/// - every held directory re-proves its `..` against the previous
///   descriptor's identity (see [`verify_parent_link`]);
/// - `creating` allows missing ancestors to be created with `mkdirat`
///   (mode 0700) and then opened no-follow;
/// - `scan_git` probes the root and every held ancestor directory for a
///   `.git` directory or `.git` worktree file — after the directory is
///   held, so the probe applies to the exact object used.
pub(crate) fn walk_parents(
    root: &RootInner,
    path: &PersonalRelativePath,
    creating: bool,
    scan_git: bool,
) -> SafetyResult<OwnedFd> {
    verify_root_identity(root)?;
    if scan_git {
        probe_git_at_root(root)?;
    }
    let components = path.components();
    let parent_count = components.len() - 1;
    let mut current = dup_fd(&root.dir)?;
    let mut current_id = root.identity;

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
        // The freshly held directory must still be a child of the
        // directory it was opened from, and is then probed (mutations).
        verify_parent_link(&dir, current_id)?;
        if scan_git {
            probe_git_entry(&dir, &format!("{path} (at ancestor {component})"))?;
        }
        current_id = identity_of(&dir)?;
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

/// Open a leaf no-follow and non-blocking under `parent`. A symlink
/// final component fails with `ELOOP`; a FIFO cannot block the open
/// (`O_NONBLOCK`) and is rejected by the type check that follows the
/// open in every caller.
pub(crate) fn open_leaf(
    parent: &OwnedFd,
    name: &str,
    write: bool,
    create_exclusive: bool,
) -> Result<OwnedFd, rustix::io::Errno> {
    let mut flags = OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
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
    #[allow(clippy::unnecessary_cast)]
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

/// Write bytes to a freshly created exclusive (no-clobber, no-follow,
/// non-blocking) file under `parent`, fsync it, verify `nlink == 1` on
/// the held descriptor, and return the still-open descriptor. The name
/// should be freshly minted (unpredictable) so the object is not
/// observable under a guessable name before the caller publishes it.
/// The caller drops the descriptor after its publication step.
pub(crate) fn write_staged_file(
    parent: &OwnedFd,
    name: &str,
    bytes: &[u8],
) -> SafetyResult<OwnedFd> {
    let file = match open_leaf(parent, name, true, true) {
        Ok(file) => file,
        Err(error) => return Err(errno_to_io(error)),
    };
    let write = (|| -> SafetyResult<()> {
        {
            let mut writer = std::fs::File::from(dup_fd(&file)?);
            writer.write_all(bytes).map_err(PersonalFileError::Io)?;
            writer.flush().map_err(PersonalFileError::Io)?;
            writer.sync_all().map_err(PersonalFileError::Io)?;
        }
        Ok(())
    })();
    let outcome = write.and_then(|()| {
        // No foreign alias may exist for what we are about to publish.
        let stat = fstat(&file).map_err(errno_to_io)?;
        require_unshared_inode(&stat, name)
    });
    match outcome {
        Ok(()) => Ok(file),
        Err(error) => {
            drop(file);
            let _ = unlinkat(parent, name, AtFlags::empty());
            Err(error)
        }
    }
}

/// Best-effort fsync of a directory descriptor after a completed
/// rename-class mutation. Publication is already complete when this
/// runs; a flushing failure must not turn a published, recoverable
/// outcome into an error that hides it.
pub(crate) fn fsync_dir_best_effort(dir: &OwnedFd) {
    let _ = fsync(dir);
}

/// Open (creating with mode 0700 if needed) the reserved root-local
/// trash directory and verify it is a real directory linked directly
/// under the root.
pub(crate) fn open_trash(root: &RootInner) -> SafetyResult<OwnedFd> {
    match openat(&root.dir, TRASH_NAMESPACE, dir_oflags(), Mode::empty()) {
        Ok(dir) => {
            verify_parent_link(&dir, root.identity).map_err(|_| {
                PersonalFileRefusal::TrashUnavailable {
                    reason: "the reserved trash dir is not linked under the root".to_string(),
                }
            })?;
            Ok(dir)
        }
        Err(rustix::io::Errno::NOENT) => {
            mkdirat(&root.dir, TRASH_NAMESPACE, Mode::from(0o700)).map_err(|error| {
                PersonalFileRefusal::TrashUnavailable {
                    reason: format!("creating the reserved trash dir failed: {error}"),
                }
            })?;
            let dir = openat(&root.dir, TRASH_NAMESPACE, dir_oflags(), Mode::empty()).map_err(
                |error| PersonalFileRefusal::TrashUnavailable {
                    reason: format!("reopening the reserved trash dir failed: {error}"),
                },
            )?;
            verify_parent_link(&dir, root.identity).map_err(|_| {
                PersonalFileRefusal::TrashUnavailable {
                    reason: "the reserved trash dir is not linked under the root".to_string(),
                }
            })?;
            Ok(dir)
        }
        Err(rustix::io::Errno::LOOP) => Err(PersonalFileRefusal::TrashUnavailable {
            reason: "the reserved trash name is occupied by a non-directory".to_string(),
        }
        .into()),
        Err(error) => Err(errno_to_io(error)),
    }
}

/// A trash slot bound at creation: name, held descriptor, and the
/// identity captured while the slot was proved linked under the trash.
pub(crate) struct TrashSlot {
    /// The freshly minted slot directory name.
    pub name: String,
    /// The slot's held descriptor (linkage-verified at bind time).
    pub dir: OwnedFd,
    /// The slot's identity captured at bind time; cleanup removes the
    /// name only while it still names this object.
    pub identity: ObjectId,
}

/// Create a fresh per-operation slot directory inside the trash and
/// bind it: the descriptor is opened, its `..` linkage under the trash
/// is verified, and its identity is captured — all on one held
/// descriptor — before the caller uses it. Slot names are freshly
/// minted per operation and re-minted on `EEXIST` (deterministic
/// collision handling by construction); the trash directory's dirent is
/// fsynced so recovery copies are crash-reachable. A substitution
/// between `mkdirat` and the bind sequence fails the bind and is
/// reported WITHOUT removing the name (nothing was bound, so nothing
/// is removed by name).
pub(crate) fn fresh_trash_slot(trash: &OwnedFd) -> SafetyResult<TrashSlot> {
    let trash_id = identity_of(trash)?;
    for _ in 0..8 {
        let slot = uuid::Uuid::new_v4().to_string();
        match mkdirat(trash, slot.as_str(), Mode::from(0o700)) {
            Ok(()) => match bind_trash_slot(trash, &slot, trash_id) {
                Ok(bound) => {
                    fsync_dir_best_effort(trash);
                    return Ok(bound);
                }
                Err(error) => return Err(error),
            },
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

/// Open the named slot, verify its `..` linkage under the trash, and
/// capture its identity — the bind. Every failure path either cleans
/// up by name only while the name still names the object this call
/// opened and identified, or leaves the freshly created directory in
/// place (an orphan inside the reserved trash namespace, harmless and
/// operator-cleanable); a substituted name is never removed.
fn bind_trash_slot(trash: &OwnedFd, slot: &str, trash_id: ObjectId) -> SafetyResult<TrashSlot> {
    let dir = openat(trash, slot, dir_oflags(), Mode::empty()).map_err(|error| {
        PersonalFileRefusal::TrashUnavailable {
            reason: format!("opening the fresh trash slot failed: {error}"),
        }
    })?;
    let identity = identity_of(&dir)?;
    if verify_parent_link(&dir, trash_id).is_err() {
        identity_checked_rmdir(trash, slot, &identity);
        return Err(PersonalFileRefusal::TrashUnavailable {
            reason: "the fresh trash slot is not linked under the trash".to_string(),
        }
        .into());
    }
    // The bind must prove the object is the freshly created, empty
    // directory (nlink == 2: parent + "."; no entries). A substituted
    // directory that already carries planted content fails here, and a
    // failed bind performs no name-based cleanup, so foreign content is
    // never adopted or removed through this module.
    let stat = fstat(&dir).map_err(errno_to_io)?;
    #[allow(clippy::unnecessary_cast)]
    let links = stat.st_nlink as u64;
    if links != 2 || !directory_is_empty(&dir)? {
        return Err(PersonalFileRefusal::TrashUnavailable {
            reason: "the fresh trash slot is not the freshly created empty directory".to_string(),
        }
        .into());
    }
    Ok(TrashSlot {
        name: slot.to_string(),
        dir,
        identity,
    })
}

/// True when the directory held by `dir` contains no entries besides
/// `.` and `..`.
fn directory_is_empty(dir: &OwnedFd) -> SafetyResult<bool> {
    let mut listing = rustix::fs::Dir::read_from(dir).map_err(errno_to_io)?;
    while let Some(entry) = listing.read() {
        let entry = entry.map_err(errno_to_io)?;
        let name = entry.file_name();
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Remove the named trash directory only while it still names an
/// object with the given identity; a no-op otherwise. The check and
/// the removal are two syscalls: a rename landing between them can at
/// worst leave this module's own orphan in place or remove an empty
/// same-named directory planted by an in-root-writable adversary (who
/// per the adversary model already holds broader direct authority);
/// it cannot remove non-empty foreign data.
fn identity_checked_rmdir(trash: &OwnedFd, slot: &str, probe: &ObjectId) -> bool {
    match statat(trash, slot, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) if ObjectId::of(&stat) == *probe => {
            unlinkat(trash, slot, AtFlags::REMOVEDIR).is_ok()
        }
        _ => false,
    }
}

/// Remove a trash slot that a failed operation left behind (best
/// effort): unlink the given file names inside the bound slot, then
/// remove the slot directory — but only while the name still resolves
/// to the object bound at creation. All names are freshly minted by
/// this module, so the cleanup cannot hit foreign data.
pub(crate) fn discard_trash_slot(trash: &OwnedFd, slot: &TrashSlot, file_names: &[String]) {
    for name in file_names {
        let _ = unlinkat(&slot.dir, name.as_str(), AtFlags::empty());
    }
    let _ = identity_checked_rmdir(trash, &slot.name, &slot.identity);
}

/// Rename `leaf` (under `parent`) into the freshly minted empty `slot`.
/// No-clobber by construction; the slot is re-proved to be linked under
/// the trash immediately before the rename so a relocated slot refuses
/// instead of receiving the object elsewhere.
pub(crate) fn move_into_trash(
    parent: &OwnedFd,
    leaf: &str,
    trash: &OwnedFd,
    slot: &TrashSlot,
) -> SafetyResult<()> {
    let trash_id = identity_of(trash)?;
    verify_parent_link(&slot.dir, trash_id).map_err(|_| PersonalFileRefusal::TrashUnavailable {
        reason: "the trash slot moved".to_string(),
    })?;
    renameat_with(parent, leaf, &slot.dir, leaf, RenameFlags::NOREPLACE).map_err(|error| {
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

/// Plain same-directory publication rename (used by replace and staged
/// create, which verified the leaf state immediately before).
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
/// 2. the canonical form is resolved, then **walked component-by-
///    component from the process root with `O_NOFOLLOW | O_DIRECTORY`
///    opens** — a symlink planted anywhere on the path before its
///    component is opened refuses admission instead of silently
///    admitting the redirect target. Symlinked OS-level *ancestors*
///    (e.g. `/var` -> `/private/var` on macOS) are part of the
///    canonical form and are walked the same way: the admitted object
///    is the resolved directory, pinned by descriptor;
/// 3. the resolved root must be a real directory linked under the
///    parent directory recorded at admission; both identities are
///    re-verified before every later operation.
pub(crate) fn admit_root(
    dir: &Path,
) -> Result<(String, OwnedFd, ObjectId, ObjectId), PersonalFileError> {
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
    // no-follow walk below re-proves every component atomically).
    let root_meta = std::fs::symlink_metadata(dir).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => PersonalFileError::NotFound(caller.clone()),
        _ => PersonalFileError::Io(error),
    })?;
    if root_meta.file_type().is_symlink() {
        return Err(PersonalFileRefusal::SymlinkedRoot { path: caller }.into());
    }
    // Canonical form for audit display, the Git scan, and the walk.
    let canonical = std::fs::canonicalize(dir).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => PersonalFileError::NotFound(caller.clone()),
        _ => PersonalFileError::Io(error),
    })?;

    // Component walk from the filesystem root: every component is
    // opened no-follow + directory-only and re-proves its `..` linkage
    // against the previously held descriptor.
    let mut current = openat(CWD, "/", dir_oflags(), Mode::empty()).map_err(errno_to_io)?;
    let mut current_id = identity_of(&current)?;
    // The filesystem root's `..` is itself; for the admitted root this
    // becomes the identity of the directory it was opened from.
    let mut root_parent_id = current_id;
    for component in canonical.components() {
        let name = match component {
            Component::RootDir => continue,
            Component::Normal(name) => name,
            _ => return Err(PersonalFileRefusal::RelativeRoot { path: caller }.into()),
        };
        let next =
            openat(&current, name, dir_oflags(), Mode::empty()).map_err(|error| match error {
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
            })?;
        verify_parent_link(&next, current_id)?;
        root_parent_id = current_id;
        current_id = identity_of(&next)?;
        current = next;
    }
    let stat = fstat(&current).map_err(errno_to_io)?;
    if file_type_of(&stat) != FileType::Directory {
        return Err(PersonalFileRefusal::NotRegularFile { path: caller }.into());
    }
    Ok((
        canonical.display().to_string(),
        current,
        current_id,
        root_parent_id,
    ))
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

/// A freshly minted staging name derived from `leaf`, guaranteed to
/// stay within NAME_MAX (255): long leaves fall back to a generic
/// prefix that keeps the uuid.
pub(crate) fn staged_name_for(leaf: &str) -> String {
    let derived = format!(".{leaf}.{}", uuid::Uuid::new_v4());
    if derived.len() <= 255 {
        derived
    } else {
        format!(".staged-{}", uuid::Uuid::new_v4())
    }
}

/// A recovery name for the trash slot: keeps the original leaf for
/// operator readability, falling back to a generic prefix at NAME_MAX.
pub(crate) fn recovery_name_for(leaf: &str) -> String {
    let derived = format!("{leaf}.pre");
    if derived.len() <= 255 {
        derived
    } else {
        format!("pre-{}", uuid::Uuid::new_v4())
    }
}

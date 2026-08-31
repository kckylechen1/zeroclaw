//! The `personal_file` safe service: typed operations over admitted
//! roots, additive and default-closed.
//!
//! Nothing in this module registers a model-visible tool, joins a
//! composition, or reads configuration. The service is constructed
//! explicitly with already-admitted root descriptors by a trusted
//! composition root (or a test fixture); #272 owns any later wiring.
//!
//! On platforms without the descriptor primitives every admission and
//! every operation answers [`PersonalFileError::UnsupportedSafely`] —
//! containment never degrades to string checks.

use std::path::Path;

use crate::personal_file::domain::{
    ExpectedContentIdentity, PersonalFileError, PersonalFileRefusal, PersonalFileResult,
    PersonalRelativePath, RootKind,
};
#[cfg(unix)]
use crate::personal_file::domain::{
    ListedEntry, MAX_TEXT_BYTES, ObjectId, RootInner, TRASH_NAMESPACE,
};

/// Fail-closed message for platforms without descriptor primitives.
#[cfg(not(unix))]
const UNSUPPORTED_PLATFORM: &str =
    "descriptor-bound filesystem containment is not available on this platform";

#[cfg(unix)]
use rustix::fd::OwnedFd;
#[cfg(unix)]
use rustix::fs::{FileType, Stat, fstat};

/// The safe personal-file service.
///
/// Holds the admitted root references minted through
/// [`PersonalFileService::admit_read_write`] /
/// [`PersonalFileService::admit_read_only`]. Every operation verifies
/// that the supplied root reference belongs to this instance and carries
/// the right kind before any descriptor is touched.
///
/// There is no ambient authority: no constructor reads `HOME`, cwd, the
/// agent workspace, or any environment. If nobody admits a root, the
/// service cannot reach anything.
#[derive(Clone, Debug, Default)]
pub struct PersonalFileService {
    write_roots: Vec<PersonalRootRef>,
    read_roots: Vec<PersonalRootRef>,
}

/// Source of a move operation.
pub struct MoveSource<'a> {
    /// Admitted root of the source.
    pub root: &'a PersonalRootRef,
    /// Root-relative source path.
    pub path: &'a PersonalRelativePath,
}

/// Destination of a move operation (must not exist).
pub struct MoveDestination<'a> {
    /// Admitted root of the destination.
    pub root: &'a PersonalRootRef,
    /// Root-relative destination path.
    pub path: &'a PersonalRelativePath,
}

impl PersonalFileService {
    /// Admit a read-write personal root. Trusted-caller only: the path is
    /// a descriptor-bound root supplied by the composition, never parsed
    /// from model input. Write roots inside any Git repository/worktree
    /// are refused at admission (owner decision D5).
    pub fn admit_read_write(dir: &Path) -> Result<PersonalRootRef, PersonalFileError> {
        Self::admit(dir, RootKind::ReadWrite)
    }

    /// Admit a read-only source root. Read roots are exempt from the
    /// Git-boundary admission scan (reading sources is their purpose);
    /// `.git` stays unreachable as a path component and reads can never
    /// mutate.
    pub fn admit_read_only(dir: &Path) -> Result<PersonalRootRef, PersonalFileError> {
        Self::admit(dir, RootKind::ReadOnly)
    }

    fn admit(dir: &Path, kind: RootKind) -> Result<PersonalRootRef, PersonalFileError> {
        #[cfg(not(unix))]
        {
            let _ = (dir, kind);
            return Err(PersonalFileError::UnsupportedSafely(UNSUPPORTED_PLATFORM));
        }

        #[cfg(unix)]
        {
            let (canonical_display, dir_fd, identity, parent_identity) = safety::admit_root(dir)?;
            if kind.allows_mutation()
                && let Some(at) = safety::git_poisoned_above(Path::new(&canonical_display))
            {
                return Err(PersonalFileRefusal::GitRepository {
                    at: at.display().to_string(),
                }
                .into());
            }
            Ok(PersonalRootRef {
                inner: std::sync::Arc::new(RootInner {
                    kind,
                    dir: dir_fd,
                    identity,
                    parent_identity,
                    canonical_display,
                }),
            })
        }
    }

    /// Assemble a service over admitted roots. Each write root must be
    /// `ReadWrite` and each read root `ReadOnly`; mixing kinds is a
    /// typed construction failure, not a runtime surprise.
    pub fn new(
        write_roots: Vec<PersonalRootRef>,
        read_roots: Vec<PersonalRootRef>,
    ) -> Result<Self, PersonalFileError> {
        for root in &write_roots {
            if !root.kind().allows_mutation() {
                return Err(PersonalFileRefusal::RootKindMismatch {
                    root: root.canonical_display().to_string(),
                    needed: "read_write",
                }
                .into());
            }
        }
        for root in &read_roots {
            if root.kind().allows_mutation() {
                return Err(PersonalFileRefusal::RootKindMismatch {
                    root: root.canonical_display().to_string(),
                    needed: "read_only",
                }
                .into());
            }
        }
        Ok(Self {
            write_roots,
            read_roots,
        })
    }

    /// All admitted roots (read-write first). Bounded introspection for
    /// the eventual tool contract.
    pub fn roots(&self) -> impl Iterator<Item = &PersonalRootRef> {
        self.write_roots.iter().chain(self.read_roots.iter())
    }

    fn admitted_for_read(&self, root: &PersonalRootRef) -> Result<(), PersonalFileError> {
        if self
            .write_roots
            .iter()
            .chain(self.read_roots.iter())
            .any(|admitted| admitted.is_same_root(root))
        {
            Ok(())
        } else {
            Err(PersonalFileRefusal::UnadmittedRoot.into())
        }
    }

    fn admitted_for_mutation(&self, root: &PersonalRootRef) -> Result<(), PersonalFileError> {
        if !root.kind().allows_mutation() {
            return Err(PersonalFileRefusal::ReadOnlyRoot.into());
        }
        self.admitted_for_read(root)
    }

    // ─────────────────────────────────────────────────────────────────
    // Async surface (blocking syscalls stay off the reactor)
    // ─────────────────────────────────────────────────────────────────

    /// Read a bounded UTF-8 text file from an admitted root.
    pub async fn read_text(
        &self,
        root: &PersonalRootRef,
        path: &PersonalRelativePath,
    ) -> Result<PersonalFileResult, PersonalFileError> {
        let this = self.clone();
        let root = root.clone();
        let path = path.clone();
        tokio::task::spawn_blocking(move || this.read_text_sync(&root, &path))
            .await
            .map_err(join_error)?
    }

    /// Create a text file; the leaf must not exist (no-clobber).
    pub async fn create_text_no_clobber(
        &self,
        root: &PersonalRootRef,
        path: &PersonalRelativePath,
        content: &str,
    ) -> Result<PersonalFileResult, PersonalFileError> {
        let this = self.clone();
        let root = root.clone();
        let path = path.clone();
        let content = content.to_string();
        tokio::task::spawn_blocking(move || this.create_text_sync(&root, &path, &content))
            .await
            .map_err(join_error)?
    }

    /// Replace a text file only if its current content identity matches
    /// `expected`; publication is an atomic staged rename and the prior
    /// content lands in the root-local trash.
    pub async fn replace_text_if_expected(
        &self,
        root: &PersonalRootRef,
        path: &PersonalRelativePath,
        expected: &ExpectedContentIdentity,
        new_content: &str,
    ) -> Result<PersonalFileResult, PersonalFileError> {
        let this = self.clone();
        let root = root.clone();
        let path = path.clone();
        let expected = expected.clone();
        let new_content = new_content.to_string();
        tokio::task::spawn_blocking(move || {
            this.replace_text_sync(&root, &path, &expected, &new_content)
        })
        .await
        .map_err(join_error)?
    }

    /// Move/rename within one admitted root; the destination must not
    /// exist. Cross-root moves answer [`PersonalFileError::UnsupportedSafely`].
    pub async fn move_no_clobber(
        &self,
        source: MoveSource<'_>,
        destination: MoveDestination<'_>,
    ) -> Result<PersonalFileResult, PersonalFileError> {
        let this = self.clone();
        let src_root = source.root.clone();
        let src_path = source.path.clone();
        let dst_root = destination.root.clone();
        let dst_path = destination.path.clone();
        tokio::task::spawn_blocking(move || {
            this.move_sync(&src_root, &src_path, &dst_root, &dst_path)
        })
        .await
        .map_err(join_error)?
    }

    /// Delete by moving into the reserved root-local trash. There is no
    /// unlink/hard-delete path in this core.
    pub async fn delete_to_trash(
        &self,
        root: &PersonalRootRef,
        path: &PersonalRelativePath,
    ) -> Result<PersonalFileResult, PersonalFileError> {
        let this = self.clone();
        let root = root.clone();
        let path = path.clone();
        tokio::task::spawn_blocking(move || this.delete_to_trash_sync(&root, &path))
            .await
            .map_err(join_error)?
    }

    /// Bounded entry metadata.
    pub async fn stat_entry(
        &self,
        root: &PersonalRootRef,
        path: &PersonalRelativePath,
    ) -> Result<PersonalFileResult, PersonalFileError> {
        let this = self.clone();
        let root = root.clone();
        let path = path.clone();
        tokio::task::spawn_blocking(move || this.stat_sync(&root, &path))
            .await
            .map_err(join_error)?
    }

    /// Bounded listing of a directory inside an admitted root (or of the
    /// root itself when `dir` is `None`). The trash namespace is never
    /// included and symlinks are never listed.
    pub async fn list(
        &self,
        root: &PersonalRootRef,
        dir: Option<&PersonalRelativePath>,
        limit: usize,
    ) -> Result<PersonalFileResult, PersonalFileError> {
        let this = self.clone();
        let root = root.clone();
        let dir = dir.cloned();
        tokio::task::spawn_blocking(move || this.list_sync(&root, dir.as_ref(), limit))
            .await
            .map_err(join_error)?
    }

    // ─────────────────────────────────────────────────────────────────
    // Sync cores (run under spawn_blocking)
    // ─────────────────────────────────────────────────────────────────

    fn read_text_sync(
        &self,
        root: &PersonalRootRef,
        path: &PersonalRelativePath,
    ) -> Result<PersonalFileResult, PersonalFileError> {
        self.admitted_for_read(root)?;
        #[cfg(not(unix))]
        {
            let _ = path;
            return Err(PersonalFileError::UnsupportedSafely(UNSUPPORTED_PLATFORM));
        }
        #[cfg(unix)]
        {
            safety::verify_root_identity(&root.inner)?;
            let parent = safety::walk_parents(&root.inner, path, false, false)?;
            let display = safety::display_path(&root.inner, path);
            let (file, _) = open_regular_verified(&parent, path.leaf(), &display)?;
            let bytes = safety::read_bounded(file, MAX_TEXT_BYTES)?;
            let identity = ExpectedContentIdentity::of_content(&bytes);
            match String::from_utf8(bytes) {
                Ok(text) => Ok(PersonalFileResult::ReadText { text, identity }),
                Err(_) => Err(PersonalFileError::NotText(display)),
            }
        }
    }

    fn create_text_sync(
        &self,
        root: &PersonalRootRef,
        path: &PersonalRelativePath,
        content: &str,
    ) -> Result<PersonalFileResult, PersonalFileError> {
        self.admitted_for_mutation(root)?;
        #[cfg(not(unix))]
        {
            let _ = (path, content);
            return Err(PersonalFileError::UnsupportedSafely(UNSUPPORTED_PLATFORM));
        }
        #[cfg(unix)]
        {
            safety::verify_root_identity(&root.inner)?;
            safety::probe_git_at_root(&root.inner)?;
            let parent = safety::walk_parents(&root.inner, path, true, true)?;
            let display = safety::display_path(&root.inner, path);
            safety::run_race_hook();
            // Stage under a freshly minted (unobservable) name, verify
            // the staged inode is unshared on its held descriptor, then
            // publish atomically with no-clobber semantics. The final
            // leaf name never carries partially written content and is
            // not visible to watch-and-link before publication.
            let staged_name = safety::staged_name_for(path.leaf());
            let publish = (|| {
                let staged = safety::write_staged_file(&parent, &staged_name, content.as_bytes())?;
                drop(staged);
                match safety::rename_no_clobber(&parent, &staged_name, &parent, path.leaf()) {
                    Ok(()) => Ok(()),
                    Err(rustix::io::Errno::EXIST) => {
                        safety::remove_staged(&parent, &staged_name);
                        Err(PersonalFileError::AlreadyExists(display.clone()))
                    }
                    Err(error) => {
                        safety::remove_staged(&parent, &staged_name);
                        Err(rustix_errno_to_io(error))
                    }
                }
            })();
            match publish {
                Ok(()) => {
                    // Publication is complete; see the replace note on
                    // best-effort directory-dirent durability.
                    safety::fsync_dir_best_effort(&parent);
                    Ok(PersonalFileResult::Created {
                        identity: ExpectedContentIdentity::of_content(content.as_bytes()),
                    })
                }
                Err(PersonalFileError::Io(error))
                    if error.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) =>
                {
                    Err(PersonalFileRefusal::Symlink { path: display }.into())
                }
                Err(error) => Err(error),
            }
        }
    }

    fn replace_text_sync(
        &self,
        root: &PersonalRootRef,
        path: &PersonalRelativePath,
        expected: &ExpectedContentIdentity,
        new_content: &str,
    ) -> Result<PersonalFileResult, PersonalFileError> {
        self.admitted_for_mutation(root)?;
        #[cfg(not(unix))]
        {
            let _ = (path, expected, new_content);
            return Err(PersonalFileError::UnsupportedSafely(UNSUPPORTED_PLATFORM));
        }
        #[cfg(unix)]
        {
            safety::verify_root_identity(&root.inner)?;
            safety::probe_git_at_root(&root.inner)?;
            let parent = safety::walk_parents(&root.inner, path, false, true)?;
            let display = safety::display_path(&root.inner, path);
            let (file, stat) = open_regular_verified(&parent, path.leaf(), &display)?;
            safety::require_unshared_inode(&stat, &display)?;
            let prior_bytes = safety::read_bounded(file, MAX_TEXT_BYTES)?;
            let actual = ExpectedContentIdentity::of_content(&prior_bytes);
            if actual.as_hex() != expected.as_hex() {
                // Typed conflict; zero mutation.
                return Err(PersonalFileError::Conflict {
                    expected: expected.as_hex().to_string(),
                    actual: actual.as_hex().to_string(),
                });
            }
            let new_identity = ExpectedContentIdentity::of_content(new_content.as_bytes());

            // Prior content stays recoverable in the root-local trash
            // before publication (D6). A freshly minted slot is
            // collision-free and the recovery copy is written under an
            // unpredictable name with its own unshared-inode check.
            let trash = safety::open_trash(&root.inner)?;
            let slot = safety::fresh_trash_slot(&trash)?;
            let recovery_name = safety::recovery_name_for(path.leaf());
            if let Err(error) = safety::write_staged_file(&slot.dir, &recovery_name, &prior_bytes) {
                safety::discard_trash_slot(&trash, &slot, &[recovery_name]);
                return Err(error);
            }
            safety::fsync_dir_best_effort(&slot.dir);
            let prior_in_trash = format!("{TRASH_NAMESPACE}/{}/{recovery_name}", slot.name);

            // Test-only race window: an attacker step may now swap the leaf.
            safety::run_race_hook();
            // Re-verify immediately before publication: still a regular
            // file, still the exact object whose identity matched, still
            // unshared.
            let recheck =
                open_regular_verified(&parent, path.leaf(), &display).and_then(|(_, restat)| {
                    safety::require_unshared_inode(&restat, &display)?;
                    if ObjectId::of(&restat) != ObjectId::of(&stat) {
                        return Err(PersonalFileRefusal::ConcurrentModification {
                            path: display.clone(),
                        }
                        .into());
                    }
                    Ok(())
                });
            if let Err(error) = recheck {
                safety::discard_trash_slot(&trash, &slot, &[recovery_name]);
                return Err(error);
            }

            // Stage the new content under a freshly minted name, then
            // publish atomically. The staged write already fsynced and
            // proved its inode unshared on the held descriptor.
            let staged_name = safety::staged_name_for(path.leaf());
            let staged =
                match safety::write_staged_file(&parent, &staged_name, new_content.as_bytes()) {
                    Ok(staged) => staged,
                    Err(error) => {
                        safety::discard_trash_slot(&trash, &slot, &[recovery_name]);
                        return Err(error);
                    }
                };
            drop(staged);
            match safety::rename_publish(&parent, &staged_name, path.leaf()) {
                Ok(()) => {
                    // The publication is complete; the directory-dirent
                    // fsync is best-effort so a flushing failure cannot
                    // turn a published, recoverable replacement into an
                    // error that hides it.
                    safety::fsync_dir_best_effort(&parent);
                    Ok(PersonalFileResult::Replaced {
                        identity: new_identity,
                        prior_in_trash,
                    })
                }
                Err(rustix::io::Errno::NOENT) => {
                    safety::remove_staged(&parent, &staged_name);
                    safety::discard_trash_slot(&trash, &slot, &[recovery_name]);
                    Err(PersonalFileRefusal::ConcurrentModification { path: display }.into())
                }
                Err(error) => {
                    safety::remove_staged(&parent, &staged_name);
                    safety::discard_trash_slot(&trash, &slot, &[recovery_name]);
                    Err(rustix_errno_to_io(error))
                }
            }
        }
    }

    fn move_sync(
        &self,
        src_root: &PersonalRootRef,
        src_path: &PersonalRelativePath,
        dst_root: &PersonalRootRef,
        dst_path: &PersonalRelativePath,
    ) -> Result<PersonalFileResult, PersonalFileError> {
        // v1 law: move/rename is same-root only (owner decision D6).
        // Cross-root is unrepresentable as one operation and refused
        // here before any filesystem work.
        if !src_root.is_same_root(dst_root) {
            return Err(PersonalFileError::UnsupportedSafely(
                "cross-root move is not supported; v1 moves stay within one admitted root",
            ));
        }
        self.admitted_for_mutation(src_root)?;
        #[cfg(not(unix))]
        {
            let _ = (src_path, dst_path);
            return Err(PersonalFileError::UnsupportedSafely(UNSUPPORTED_PLATFORM));
        }
        #[cfg(unix)]
        {
            safety::verify_root_identity(&src_root.inner)?;
            safety::probe_git_at_root(&src_root.inner)?;
            // Validate the source strictly BEFORE any destination work:
            // a refused or missing source must not leave freshly created
            // destination directories behind.
            let src_parent = safety::walk_parents(&src_root.inner, src_path, false, true)?;
            let src_display = safety::display_path(&src_root.inner, src_path);
            let src_stat = match safety::stat_leaf(&src_parent, src_path.leaf())? {
                Some(stat) => stat,
                None => return Err(PersonalFileError::NotFound(src_display)),
            };
            let src_kind = FileType::from_raw_mode(src_stat.st_mode);
            match src_kind {
                FileType::RegularFile => safety::require_unshared_inode(&src_stat, &src_display)?,
                FileType::Directory => {
                    probe_target_dir_not_git(&src_parent, src_path.leaf(), &src_display)?
                }
                _ => {
                    return Err(PersonalFileRefusal::NotRegularFile { path: src_display }.into());
                }
            }
            let dst_parent = safety::walk_parents(&src_root.inner, dst_path, true, true)?;
            safety::run_race_hook();
            // Re-verify the source immediately before the rename: still
            // the exact object that was classified, still unshared.
            match safety::stat_leaf(&src_parent, src_path.leaf())? {
                Some(restat) if ObjectId::of(&restat) == ObjectId::of(&src_stat) => {
                    if FileType::from_raw_mode(restat.st_mode) == FileType::RegularFile {
                        safety::require_unshared_inode(&restat, &src_display)?;
                    }
                }
                _ => {
                    return Err(
                        PersonalFileRefusal::ConcurrentModification { path: src_display }.into(),
                    );
                }
            }
            match safety::rename_no_clobber(
                &src_parent,
                src_path.leaf(),
                &dst_parent,
                dst_path.leaf(),
            ) {
                Ok(()) => {
                    safety::fsync_dir_best_effort(&src_parent);
                    safety::fsync_dir_best_effort(&dst_parent);
                    Ok(PersonalFileResult::Moved)
                }
                Err(rustix::io::Errno::EXIST) => Err(PersonalFileError::AlreadyExists(
                    safety::display_path(&src_root.inner, dst_path),
                )),
                Err(rustix::io::Errno::NOENT) => Err(PersonalFileError::NotFound(src_display)),
                Err(rustix::io::Errno::XDEV) => Err(PersonalFileError::UnsupportedSafely(
                    "cross-device move refused; moves stay inside one admitted root filesystem",
                )),
                Err(other) => Err(rustix_errno_to_io(other)),
            }
        }
    }

    fn delete_to_trash_sync(
        &self,
        root: &PersonalRootRef,
        path: &PersonalRelativePath,
    ) -> Result<PersonalFileResult, PersonalFileError> {
        self.admitted_for_mutation(root)?;
        #[cfg(not(unix))]
        {
            let _ = path;
            return Err(PersonalFileError::UnsupportedSafely(UNSUPPORTED_PLATFORM));
        }
        #[cfg(unix)]
        {
            safety::verify_root_identity(&root.inner)?;
            safety::probe_git_at_root(&root.inner)?;
            let parent = safety::walk_parents(&root.inner, path, false, true)?;
            let display = safety::display_path(&root.inner, path);
            let stat = match safety::stat_leaf(&parent, path.leaf())? {
                Some(stat) => stat,
                None => return Err(PersonalFileError::NotFound(display)),
            };
            let kind = FileType::from_raw_mode(stat.st_mode);
            match kind {
                FileType::RegularFile => safety::require_unshared_inode(&stat, &display)?,
                FileType::Directory => probe_target_dir_not_git(&parent, path.leaf(), &display)?,
                _ => return Err(PersonalFileRefusal::NotRegularFile { path: display }.into()),
            }
            let trash = safety::open_trash(&root.inner)?;
            let slot = safety::fresh_trash_slot(&trash)?;
            // Everything after slot creation rolls the slot back on any
            // failure: a refused delete publishes nothing.
            let outcome = (|| -> SafetyResult<PersonalFileResult> {
                safety::run_race_hook();
                // Re-verify immediately before the rename: still the
                // exact object that was classified, still unshared
                // (files).
                match safety::stat_leaf(&parent, path.leaf())? {
                    Some(restat) if ObjectId::of(&restat) == ObjectId::of(&stat) => {
                        if FileType::from_raw_mode(restat.st_mode) == FileType::RegularFile {
                            safety::require_unshared_inode(&restat, &display)?;
                        }
                    }
                    _ => {
                        return Err(PersonalFileRefusal::ConcurrentModification {
                            path: display.clone(),
                        }
                        .into());
                    }
                }
                safety::move_into_trash(&parent, path.leaf(), &trash, &slot)?;
                safety::fsync_dir_best_effort(&parent);
                safety::fsync_dir_best_effort(&slot.dir);
                Ok(PersonalFileResult::Trashed {
                    trash_location: format!("{TRASH_NAMESPACE}/{}/{}", slot.name, path.leaf()),
                })
            })();
            match outcome {
                Ok(result) => Ok(result),
                Err(error) => {
                    safety::discard_trash_slot(&trash, &slot, &[]);
                    Err(error)
                }
            }
        }
    }

    fn stat_sync(
        &self,
        root: &PersonalRootRef,
        path: &PersonalRelativePath,
    ) -> Result<PersonalFileResult, PersonalFileError> {
        self.admitted_for_read(root)?;
        #[cfg(not(unix))]
        {
            let _ = path;
            return Err(PersonalFileError::UnsupportedSafely(UNSUPPORTED_PLATFORM));
        }
        #[cfg(unix)]
        {
            safety::verify_root_identity(&root.inner)?;
            let parent = safety::walk_parents(&root.inner, path, false, false)?;
            let display = safety::display_path(&root.inner, path);
            let stat = match safety::stat_leaf(&parent, path.leaf())? {
                Some(stat) => stat,
                None => return Err(PersonalFileError::NotFound(display)),
            };
            match FileType::from_raw_mode(stat.st_mode) {
                FileType::RegularFile => Ok(PersonalFileResult::Stat {
                    is_dir: false,
                    size: stat.st_size as u64,
                }),
                FileType::Directory => Ok(PersonalFileResult::Stat {
                    is_dir: true,
                    size: 0,
                }),
                _ => Err(PersonalFileRefusal::NotRegularFile { path: display }.into()),
            }
        }
    }

    fn list_sync(
        &self,
        root: &PersonalRootRef,
        dir: Option<&PersonalRelativePath>,
        limit: usize,
    ) -> Result<PersonalFileResult, PersonalFileError> {
        self.admitted_for_read(root)?;
        #[cfg(not(unix))]
        {
            let _ = (dir, limit);
            return Err(PersonalFileError::UnsupportedSafely(UNSUPPORTED_PLATFORM));
        }
        #[cfg(unix)]
        {
            safety::verify_root_identity(&root.inner)?;
            let dir_fd: OwnedFd = match dir {
                None => dup(&root.inner.dir)?,
                Some(path) => {
                    let parent = safety::walk_parents(&root.inner, path, false, false)?;
                    let display = safety::display_path(&root.inner, path);
                    match safety::open_leaf_dir(&parent, path.leaf()) {
                        Ok(opened) => opened,
                        Err(rustix::io::Errno::NOENT) => {
                            return Err(PersonalFileError::NotFound(display));
                        }
                        Err(rustix::io::Errno::LOOP) => {
                            return Err(PersonalFileRefusal::Symlink { path: display }.into());
                        }
                        Err(rustix::io::Errno::NOTDIR) => {
                            return Err(
                                PersonalFileRefusal::NotRegularFile { path: display }.into()
                            );
                        }
                        Err(other) => return Err(rustix_errno_to_io(other)),
                    }
                }
            };
            // The listing is a pure read: entries resolve against this
            // held descriptor only, so a swapped name elsewhere cannot
            // be adopted into the result.
            let stat_dir = dup(&dir_fd)?;
            let mut entries: Vec<ListedEntry> = Vec::new();
            for entry in rustix::fs::Dir::read_from(&dir_fd).map_err(rustix_errno_to_io)? {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => return Err(rustix_errno_to_io(error)),
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                if name == "." || name == ".." || name == TRASH_NAMESPACE {
                    continue;
                }
                // Symlinks are refused objects in this domain, never
                // navigable entries: they are not listed at all. Each
                // entry's classification and size come from ONE no-follow
                // stat against the held descriptor, so type and size can
                // never describe two different objects.
                let stat = match safety::stat_leaf(&stat_dir, &name)? {
                    Some(stat) => stat,
                    None => continue,
                };
                match FileType::from_raw_mode(stat.st_mode) {
                    FileType::RegularFile => {
                        #[allow(clippy::unnecessary_cast)]
                        let size = stat.st_size as u64;
                        entries.push(ListedEntry {
                            name,
                            is_dir: false,
                            size,
                        });
                    }
                    FileType::Directory => entries.push(ListedEntry {
                        name,
                        is_dir: true,
                        size: 0,
                    }),
                    _ => continue,
                }
            }
            if entries.len() > limit {
                return Err(PersonalFileError::TooManyEntries(limit));
            }
            entries.sort_by(|left, right| left.name.cmp(&right.name));
            Ok(PersonalFileResult::Listed { entries })
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────

use crate::personal_file::domain::PersonalRootRef;
#[cfg(unix)]
use crate::personal_file::safety;
#[cfg(unix)]
use crate::personal_file::safety::SafetyResult;

fn join_error(error: tokio::task::JoinError) -> PersonalFileError {
    PersonalFileError::Io(std::io::Error::from(error))
}

#[cfg(unix)]
fn rustix_errno_to_io(error: rustix::io::Errno) -> PersonalFileError {
    PersonalFileError::Io(std::io::Error::from(error))
}

#[cfg(unix)]
fn dup(fd: &OwnedFd) -> Result<OwnedFd, PersonalFileError> {
    rustix::io::dup(fd).map_err(rustix_errno_to_io)
}

/// Open the leaf as a regular file, no-follow, with the descriptor
/// identity checked against a pre-open stat. Not-found flows through
/// [`PersonalFileError::NotFound`].
#[cfg(unix)]
fn open_regular_verified(
    parent: &OwnedFd,
    name: &str,
    display: &str,
) -> Result<(OwnedFd, Stat), PersonalFileError> {
    let Some(stat) = safety::require_regular_file(parent, name, display)? else {
        return Err(PersonalFileError::NotFound(display.to_string()));
    };
    let file = safety::open_leaf(parent, name, false, false).map_err(|error| match error {
        rustix::io::Errno::LOOP => PersonalFileRefusal::Symlink {
            path: display.to_string(),
        }
        .into(),
        other => rustix_errno_to_io(other),
    })?;
    let restat = fstat(&file).map_err(rustix_errno_to_io)?;
    if FileType::from_raw_mode(restat.st_mode) != FileType::RegularFile
        || ObjectId::of(&restat) != ObjectId::of(&stat)
    {
        return Err(PersonalFileRefusal::ConcurrentModification {
            path: display.to_string(),
        }
        .into());
    }
    Ok((file, restat))
}

/// For directory targets of move/delete: refuse a directory that is
/// itself a repository root (holds a `.git` entry).
#[cfg(unix)]
fn probe_target_dir_not_git(
    parent: &OwnedFd,
    leaf: &str,
    display: &str,
) -> Result<(), PersonalFileError> {
    let dir = safety::open_leaf_dir(parent, leaf).map_err(|error| match error {
        rustix::io::Errno::NOENT => PersonalFileError::NotFound(display.to_string()),
        rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR => PersonalFileRefusal::Symlink {
            path: display.to_string(),
        }
        .into(),
        other => rustix_errno_to_io(other),
    })?;
    match safety::stat_leaf(&dir, ".git")? {
        Some(_) => Err(PersonalFileRefusal::GitRepository {
            at: display.to_string(),
        }
        .into()),
        None => Ok(()),
    }
}

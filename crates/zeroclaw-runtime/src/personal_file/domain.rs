//! Typed vocabulary for the `personal_file` safe core.
//!
//! Every capability boundary of the module is expressed here as a type:
//! an absolute caller-selected root is unrepresentable (roots enter only
//! through [`crate::personal_file::PersonalFileService::admit_read_write`]
//! / `admit_read_only`), `..` escapes and reserved namespaces are
//! unrepresentable as a [`PersonalRelativePath`], binary content is
//! unrepresentable as a write input (`&str`), and no operation name below
//! maps to a raw path, a shell, or a process.

use std::sync::Arc;

/// How an admitted root may be used. Read-write and read-only roots are
/// distinct capabilities (owner decision D2): a read-only root can never
/// be mutated through this module, and the distinction is carried by the
/// root reference itself, not by per-call flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootKind {
    /// Admitted for reads and mutations.
    ReadWrite,
    /// Admitted for reads only; every mutation answers
    /// [`PersonalFileRefusal::ReadOnlyRoot`].
    ReadOnly,
}

impl RootKind {
    /// True when mutations are allowed through this root kind.
    pub fn allows_mutation(self) -> bool {
        matches!(self, RootKind::ReadWrite)
    }
}

/// Identity captured for a filesystem object (device + inode). Only
/// meaningful on platforms that expose it; on others the containment
/// primitives themselves are unavailable and admission fails closed
/// before any identity could be captured.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectId {
    /// Device id.
    pub device: u64,
    /// Inode number.
    pub inode: u64,
}

/// The admitted-root anchor shared by the safety core and the service.
///
/// On Unix it holds the root's open directory descriptor (the single
/// operation anchor) and the identity captured at admission. On other
/// platforms it can never be constructed — admission fails closed with
/// `unsupported_safely` — and the struct exists only so the typed
/// surface compiles.
pub(crate) struct RootInner {
    pub kind: RootKind,
    /// The root directory descriptor — the operation anchor.
    #[cfg(unix)]
    pub dir: rustix::fd::OwnedFd,
    /// Identity at admission, re-verified before every operation.
    #[cfg(unix)]
    pub identity: ObjectId,
    /// Identity of the parent directory the root was admitted under;
    /// re-verified (via the root's `..` entry) before every operation
    /// so a relocated root refuses instead of carrying its authority
    /// to a new location.
    #[cfg(unix)]
    pub parent_identity: ObjectId,
    /// Canonical path at admission; display/audit only.
    pub canonical_display: String,
}

/// A reference to an already-admitted personal root. Construction is
/// private to the admission path; no ambient `HOME`/cwd/workspace
/// authority exists and no model-supplied absolute path can mint one.
///
/// The reference is self-contained: it holds the root's open directory
/// descriptor (the operation anchor), the device/inode identity captured
/// at admission (re-verified before every operation), and the canonical
/// path for audit display only — no filesystem access ever resolves
/// through that path after admission.
#[derive(Clone)]
pub struct PersonalRootRef {
    pub(crate) inner: Arc<RootInner>,
}

impl PersonalRootRef {
    /// How this root may be used.
    pub fn kind(&self) -> RootKind {
        self.inner.kind
    }

    /// Canonical path captured at admission. Display/audit only — the
    /// safety core never opens through this string.
    pub fn canonical_display(&self) -> &str {
        &self.inner.canonical_display
    }

    /// Stable identity of the admitted root (device + inode on Unix).
    pub(crate) fn is_same_root(&self, other: &PersonalRootRef) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl std::fmt::Debug for PersonalRootRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersonalRootRef")
            .field("kind", &self.inner.kind)
            .field("canonical_display", &self.inner.canonical_display)
            .finish_non_exhaustive()
    }
}

/// Reserved root-local namespace that holds deleted/replaced content.
/// Invisible to ordinary listing, unreachable as a user path component,
/// and excluded from every user-addressable operation.
pub const TRASH_NAMESPACE: &str = ".zeroclaw-trash";

/// A path relative to an admitted root, safe by construction.
///
/// Parsing rejects: absolute paths, empty/`.`/`..` components, NUL bytes,
/// and the reserved [`TRASH_NAMESPACE`] and `.git` component names. A
/// parsed value therefore cannot name anything outside its root, the
/// trash namespace, or Git metadata — those refusals are enforced at the
/// type boundary before any descriptor is touched.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PersonalRelativePath {
    components: Vec<String>,
}

impl PersonalRelativePath {
    /// Parse and validate a root-relative path. Refusals are typed, never
    /// silent normalization.
    pub fn parse(raw: &str) -> Result<Self, PersonalFileError> {
        let components = Self::validate(raw)?;
        Ok(Self { components })
    }

    fn validate(raw: &str) -> Result<Vec<String>, PersonalFileError> {
        if raw.is_empty() {
            return Err(PersonalFileRefusal::EmptyPath.into());
        }
        if raw.starts_with('/') {
            return Err(PersonalFileRefusal::AbsolutePath {
                path: raw.to_string(),
            }
            .into());
        }
        if raw.contains('\0') {
            return Err(PersonalFileRefusal::NulByte.into());
        }
        let mut components = Vec::new();
        for component in raw.split('/') {
            match component {
                "" | "." => {
                    return Err(PersonalFileRefusal::InvalidComponent {
                        component: component.to_string(),
                    }
                    .into());
                }
                ".." => {
                    return Err(PersonalFileRefusal::ParentComponent {
                        path: raw.to_string(),
                    }
                    .into());
                }
                TRASH_NAMESPACE => {
                    return Err(PersonalFileRefusal::ReservedNamespace {
                        component: component.to_string(),
                    }
                    .into());
                }
                ".git" => {
                    return Err(PersonalFileRefusal::GitMetadataPath {
                        component: component.to_string(),
                    }
                    .into());
                }
                _ => components.push(component.to_string()),
            }
        }
        if components.is_empty() {
            return Err(PersonalFileRefusal::EmptyPath.into());
        }
        Ok(components)
    }

    /// The validated components (no separators, no escapes).
    pub fn components(&self) -> &[String] {
        &self.components
    }

    /// The final component (file or directory name).
    pub fn leaf(&self) -> &str {
        self.components
            .last()
            .expect("validated paths have at least one component")
    }

    /// Number of components.
    pub fn depth(&self) -> usize {
        self.components.len()
    }
}

impl std::fmt::Display for PersonalRelativePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.components.join("/"))
    }
}

/// Upper bound for one text read/create/replace payload (1 MiB). The
/// policy is explicit and constant; exceeding it is a typed
/// [`PersonalFileError::TooLarge`], never a silent truncation.
pub const MAX_TEXT_BYTES: u64 = 1024 * 1024;

/// Upper bound for one bounded listing.
pub const MAX_LIST_ENTRIES: usize = 10_000;

/// Sha-256 digest, lowercase hex. The content identity used by
/// `replace_text_if_expected`: a mismatch answers a typed conflict with
/// zero mutation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ExpectedContentIdentity {
    hex: String,
}

impl ExpectedContentIdentity {
    /// Wrap an expected digest (lowercase hex, 64 chars).
    pub fn from_hex(hex: impl Into<String>) -> Result<Self, PersonalFileError> {
        let hex = hex.into();
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(PersonalFileRefusal::MalformedContentIdentity.into());
        }
        Ok(Self {
            hex: hex.to_ascii_lowercase(),
        })
    }

    /// Compute the identity of the given content.
    pub fn of_content(content: &[u8]) -> Self {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(content);
        Self {
            hex: hex::encode(digest),
        }
    }

    /// The lowercase hex digest.
    pub fn as_hex(&self) -> &str {
        &self.hex
    }
}

impl std::fmt::Display for ExpectedContentIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.hex)
    }
}

/// The operations the safe core exposes. This enum is the audit/
/// classification vocabulary; it carries no raw path or command payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersonalFileOperation {
    ReadText,
    CreateTextNoClobber,
    ReplaceTextIfExpected,
    MoveSameRootNoClobber,
    DeleteToTrash,
    Stat,
    List,
}

impl PersonalFileOperation {
    /// Stable operation name (audit logs / typed results).
    pub fn as_str(self) -> &'static str {
        match self {
            PersonalFileOperation::ReadText => "read_text",
            PersonalFileOperation::CreateTextNoClobber => "create_text_no_clobber",
            PersonalFileOperation::ReplaceTextIfExpected => "replace_text_if_expected",
            PersonalFileOperation::MoveSameRootNoClobber => "move_same_root_no_clobber",
            PersonalFileOperation::DeleteToTrash => "delete_to_trash",
            PersonalFileOperation::Stat => "stat",
            PersonalFileOperation::List => "list",
        }
    }
}

/// One entry of a bounded listing. Symlinks are never reported (they are
/// refused objects in this domain, not entries to navigate).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedEntry {
    /// Entry name (final component only).
    pub name: String,
    /// True for directories, false for regular files.
    pub is_dir: bool,
    /// Regular-file size in bytes (0 for directories).
    pub size: u64,
}

/// Typed outcome of a personal-file operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PersonalFileResult {
    /// Text read (already validated UTF-8, within the byte bound).
    ReadText {
        /// File content.
        text: String,
        /// Content identity of the raw bytes (input to replace-if-expected).
        identity: ExpectedContentIdentity,
    },
    /// Created; no prior file existed (no-clobber).
    Created {
        /// Identity of the written content.
        identity: ExpectedContentIdentity,
    },
    /// Replaced; the expected identity matched and publication was atomic.
    /// The prior content is recoverable in the root-local trash.
    Replaced {
        /// Identity of the new content.
        identity: ExpectedContentIdentity,
        /// Trash-relative location of the prior content (display/audit).
        prior_in_trash: String,
    },
    /// Moved/renamed within one admitted root; destination did not exist.
    Moved,
    /// Deleted to the reserved root-local trash (recoverable, hidden from
    /// ordinary listing). No unlink/hard delete exists in this core.
    Trashed {
        /// Trash-relative location (display/audit).
        trash_location: String,
    },
    /// Bounded entry metadata.
    Stat {
        /// Whether the path is a directory.
        is_dir: bool,
        /// Regular-file size in bytes (0 for directories).
        size: u64,
    },
    /// Bounded listing; the trash namespace is never included.
    Listed {
        /// Entries sorted by name, within the requested bound.
        entries: Vec<ListedEntry>,
    },
}

impl PersonalFileResult {
    /// The operation class this result belongs to.
    pub fn operation(&self) -> PersonalFileOperation {
        match self {
            PersonalFileResult::ReadText { .. } => PersonalFileOperation::ReadText,
            PersonalFileResult::Created { .. } => PersonalFileOperation::CreateTextNoClobber,
            PersonalFileResult::Replaced { .. } => PersonalFileOperation::ReplaceTextIfExpected,
            PersonalFileResult::Moved => PersonalFileOperation::MoveSameRootNoClobber,
            PersonalFileResult::Trashed { .. } => PersonalFileOperation::DeleteToTrash,
            PersonalFileResult::Stat { .. } => PersonalFileOperation::Stat,
            PersonalFileResult::Listed { .. } => PersonalFileOperation::List,
        }
    }
}

/// Why a path/root/operation was refused. Refusals are structural: the
/// caller cannot argue, retry, or widen them through this API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PersonalFileRefusal {
    /// An absolute path was supplied where only a root-relative path is
    /// representable.
    AbsolutePath {
        /// The offending input.
        path: String,
    },
    /// `..` or equivalent escape attempt.
    ParentComponent {
        /// The offending input.
        path: String,
    },
    /// Empty or `.`-only path.
    EmptyPath,
    /// A component that is not a usable name.
    InvalidComponent {
        /// The offending component.
        component: String,
    },
    /// NUL byte in a path.
    NulByte,
    /// The reserved trash namespace was targeted.
    ReservedNamespace {
        /// The offending component.
        component: String,
    },
    /// Git metadata was targeted directly.
    GitMetadataPath {
        /// The offending component.
        component: String,
    },
    /// The root descriptor itself was relative or not normal.
    RelativeRoot {
        /// The offending input.
        path: String,
    },
    /// The root itself is a symlink; no-follow admission refused it.
    /// (Symlinked ancestors *above* the root are resolved by admission's
    /// canonicalization by design — see `safety::admit_root` for the
    /// ground-truth caveat; the canonical path is then walked no-follow,
    /// so a symlink planted on it refuses there.)
    SymlinkedRoot {
        /// The offending path.
        path: String,
    },
    /// A path component resolved to a symlink (no-follow open).
    Symlink {
        /// The offending path (display).
        path: String,
    },
    /// A `.git` directory or `.git` worktree file exists at the root or
    /// an ancestor of the mutation target.
    GitRepository {
        /// Where the indicator was found (display).
        at: String,
    },
    /// The target file shares an inode with other names; mutating it
    /// through the personal root would corrupt foreign identities.
    Hardlinked {
        /// The offending path (display).
        path: String,
    },
    /// The admitted root's identity changed since admission.
    RootIdentityChanged,
    /// The object changed identity between verification and use (a lost
    /// race against the safety core; no mutation was applied).
    ConcurrentModification {
        /// The offending path (display).
        path: String,
    },
    /// The target is not a regular file (or plain directory where
    /// allowed).
    NotRegularFile {
        /// The offending path (display).
        path: String,
    },
    /// The root was admitted read-only; the operation mutates.
    ReadOnlyRoot,
    /// The root reference was not admitted to this service instance.
    UnadmittedRoot,
    /// A root was registered into a service slot requiring another kind.
    RootKindMismatch {
        /// The offending root (display).
        root: String,
        /// The kind the slot requires.
        needed: &'static str,
    },
    /// The trash namespace itself failed safety checks.
    TrashUnavailable {
        /// Reason (display).
        reason: String,
    },
    /// A malformed expected-content identity was supplied.
    MalformedContentIdentity,
}

impl std::error::Error for PersonalFileRefusal {}

impl std::fmt::Display for PersonalFileRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PersonalFileRefusal::AbsolutePath { path } => {
                write!(f, "absolute path is not representable here: {path}")
            }
            PersonalFileRefusal::ParentComponent { path } => {
                write!(f, "parent traversal refused: {path}")
            }
            PersonalFileRefusal::EmptyPath => f.write_str("empty path"),
            PersonalFileRefusal::InvalidComponent { component } => {
                write!(f, "invalid path component: {component:?}")
            }
            PersonalFileRefusal::NulByte => f.write_str("NUL byte in path"),
            PersonalFileRefusal::ReservedNamespace { component } => {
                write!(f, "reserved namespace is unreachable: {component:?}")
            }
            PersonalFileRefusal::GitMetadataPath { component } => {
                write!(f, "git metadata is unreachable: {component:?}")
            }
            PersonalFileRefusal::RelativeRoot { path } => {
                write!(f, "root must be an absolute, normal path: {path}")
            }
            PersonalFileRefusal::SymlinkedRoot { path } => {
                write!(f, "root or its ancestor is a symlink: {path}")
            }
            PersonalFileRefusal::Symlink { path } => {
                write!(f, "symlink refused (no-follow): {path}")
            }
            PersonalFileRefusal::GitRepository { at } => {
                write!(f, "git repository/worktree boundary reached at {at}")
            }
            PersonalFileRefusal::Hardlinked { path } => {
                write!(
                    f,
                    "hard-linked inode is not mutable through this root: {path}"
                )
            }
            PersonalFileRefusal::RootIdentityChanged => {
                f.write_str("root identity changed since admission")
            }
            PersonalFileRefusal::ConcurrentModification { path } => {
                write!(
                    f,
                    "{path} changed during the operation; no mutation applied"
                )
            }
            PersonalFileRefusal::NotRegularFile { path } => {
                write!(f, "not a regular file: {path}")
            }
            PersonalFileRefusal::ReadOnlyRoot => {
                f.write_str("root is admitted read-only; mutation refused")
            }
            PersonalFileRefusal::UnadmittedRoot => {
                f.write_str("root is not admitted to this service")
            }
            PersonalFileRefusal::RootKindMismatch { root, needed } => {
                write!(f, "root {root} does not satisfy the required kind {needed}")
            }
            PersonalFileRefusal::TrashUnavailable { reason } => {
                write!(f, "root-local trash unavailable: {reason}")
            }
            PersonalFileRefusal::MalformedContentIdentity => {
                f.write_str("expected content identity must be 64 hex chars")
            }
        }
    }
}

/// Typed error surface of the safe core. Read unavailability, not-found,
/// conflicts, bounds, and safety refusals are distinct variants: a failed
/// read can never masquerade as empty content.
#[derive(Debug, thiserror::Error)]
pub enum PersonalFileError {
    /// The platform primitive needed for structural containment is
    /// unavailable. The core answers unsupported and never falls back to
    /// string-only containment.
    #[error("unsupported_safely: {0}")]
    UnsupportedSafely(&'static str),
    /// A structural safety refusal.
    #[error("refused: {0}")]
    Refused(#[from] PersonalFileRefusal),
    /// The current content identity does not match the expected one;
    /// zero mutation was applied.
    #[error("content conflict: expected {expected}, found {actual}")]
    Conflict {
        /// The identity the caller expected.
        expected: String,
        /// The identity actually present.
        actual: String,
    },
    /// The target does not exist. Distinct from a read failure and from
    /// empty content.
    #[error("not found: {0}")]
    NotFound(String),
    /// Create/move no-clobber: the leaf already exists.
    #[error("already exists (no-clobber): {0}")]
    AlreadyExists(String),
    /// File exceeds the explicit text bound.
    #[error("too large: {actual} bytes exceeds the {limit}-byte bound")]
    TooLarge {
        /// The explicit bound.
        limit: u64,
        /// The actual size.
        actual: u64,
    },
    /// Listing exceeds the explicit bound.
    #[error("too many entries: more than {0}")]
    TooManyEntries(usize),
    /// Content is not valid UTF-8; v1 is text-only and answers typed
    /// unsupported rather than corrupting content.
    #[error("content is not text (utf-8): {0}")]
    NotText(String),
    /// A filesystem I/O failure. Never reported as empty or not-found.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_clean_relative_paths() {
        let path = PersonalRelativePath::parse("notes/2026/todo.txt").expect("parse");
        assert_eq!(path.components().len(), 3);
        assert_eq!(path.leaf(), "todo.txt");
    }

    #[test]
    fn parse_refuses_escapes_and_reserved_names_typed() {
        assert!(matches!(
            PersonalRelativePath::parse("../escape.txt"),
            Err(PersonalFileError::Refused(
                PersonalFileRefusal::ParentComponent { .. }
            ))
        ));
        assert!(matches!(
            PersonalRelativePath::parse("/abs/path.txt"),
            Err(PersonalFileError::Refused(
                PersonalFileRefusal::AbsolutePath { .. }
            ))
        ));
        assert!(matches!(
            PersonalRelativePath::parse(format!("a/{TRASH_NAMESPACE}/b").as_str()),
            Err(PersonalFileError::Refused(
                PersonalFileRefusal::ReservedNamespace { .. }
            ))
        ));
        assert!(matches!(
            PersonalRelativePath::parse(".git/config"),
            Err(PersonalFileError::Refused(
                PersonalFileRefusal::GitMetadataPath { .. }
            ))
        ));
        assert!(matches!(
            PersonalRelativePath::parse("a/./b"),
            Err(PersonalFileError::Refused(
                PersonalFileRefusal::InvalidComponent { .. }
            ))
        ));
    }

    #[test]
    fn identity_hex_is_validated_and_normalized() {
        let identity = ExpectedContentIdentity::of_content(b"hello");
        assert_eq!(identity.as_hex().len(), 64);
        assert!(ExpectedContentIdentity::from_hex(identity.as_hex()).is_ok());
        assert!(matches!(
            ExpectedContentIdentity::from_hex("nothex"),
            Err(PersonalFileError::Refused(
                PersonalFileRefusal::MalformedContentIdentity
            ))
        ));
    }
}

//! The model-unregistered `personal_file` safe core (LAST-A LA1,
//! #271).
//!
//! A typed filesystem kernel for personal documents (notes, config
//! drafts) that can operate **only** inside explicitly admitted root
//! descriptors supplied by a trusted composition root. This slice is
//! deliberately additive and default-closed:
//!
//! ```text
//! no model-visible tool registration
//! no composition membership
//! no config producer changes
//! no shell/file_write/file_edit change
//! ```
//!
//! The frozen authority law (owner decisions D1–D7 in #266):
//!
//! - **No ambient authority.** Roots enter only through explicit
//!   admission of an absolute, normal, non-symlink directory. No
//!   constructor reads `HOME`, cwd, the agent workspace, a harness
//!   workspace, or any repository. If nothing is admitted, the kernel
//!   cannot reach anything.
//! - **Descriptor-bound containment.** Every operation walks from the
//!   admitted root's held directory descriptor with `openat`-style
//!   `O_NOFOLLOW | O_DIRECTORY` component opens; a swapped-in symlink
//!   answers a typed refusal instead of resolving. The root's identity
//!   is re-verified before every operation. Lexical prefix checks are
//!   nowhere the security proof.
//! - **Git exclusion runs before mutation.** A write root at or under
//!   any repository/worktree (a `.git` directory or a `.git` worktree
//!   file on the root or any ancestor) is refused at admission; every
//!   mutation re-probes the root and the target's ancestor chain;
//!   `.git` is unreachable as a path component.
//! - **Hard-link containment.** Files with `nlink != 1` refuse
//!   mutation: a foreign inode identity cannot be modified through a
//!   personal root.
//! - **Typed semantics.** Create is no-clobber; replace requires the
//!   expected content identity and publishes atomically from a staged
//!   sibling; move is same-root and no-clobber; delete moves into a
//!   reserved root-local trash that ordinary listing never shows and
//!   that no path component can name. Hard purge does not exist here.
//! - **Text-only v1.** Non-UTF-8 content answers a typed unsupported
//!   result; binary writes are unrepresentable (`&str` inputs).
//! - **Fail closed on unsupported platforms.** Platforms without the
//!   descriptor primitives answer `unsupported_safely`; containment
//!   never degrades to string checks.
//!
//! ```text
//! PersonalFileService          the only entry point
//!   ├─ admit_read_write / admit_read_only     trusted-caller admission
//!   ├─ read_text / create_text_no_clobber
//!   ├─ replace_text_if_expected
//!   ├─ move_no_clobber (same-root)
//!   ├─ delete_to_trash
//!   └─ stat_entry / list (bounded)
//!
//! domain.rs    typed vocabulary (roots, paths, identities, results)
//! safety.rs    the descriptor-bound containment core (Unix)
//! service.rs   operation semantics over the safety core
//! ```
//!
//! #272 owns root/read wiring and migration closure; #273 owns the
//! atomic authority cutover. Until then nothing outside this module may
//! reference it.

mod domain;
#[cfg(unix)]
mod safety;
mod service;

#[cfg(test)]
mod tests;

pub use domain::{
    ExpectedContentIdentity, ListedEntry, MAX_LIST_ENTRIES, MAX_TEXT_BYTES, PersonalFileError,
    PersonalFileOperation, PersonalFileRefusal, PersonalFileResult, PersonalRelativePath,
    PersonalRootRef, RootKind, TRASH_NAMESPACE,
};
pub use service::{MoveDestination, MoveSource, PersonalFileService};

//! Shared delegation admission vocabulary.

/// TB-4 forbidden-content categories (mirrors the tachi host's
/// `ForbiddenCategory` one-for-one so client pre-flight and host
/// admission cannot disagree on vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ForbiddenCategory {
    /// Raw credential-shaped value (API keys, tokens, private keys).
    #[error("credential-shaped value")]
    Credential,
    /// Shell/SSH/tmux/container command text.
    #[error("cli/shell command text")]
    Command,
    /// Worktree/filesystem path used as execution authority.
    #[error("worktree-shaped path")]
    WorktreePath,
    /// Private-Dyad-labeled value.
    #[error("private-dyad-labeled value")]
    PrivateDyad,
    /// Caller-minted task/attempt id smuggled as content.
    #[error("caller-minted task/attempt id")]
    CallerMintedRef,
    /// Execution detail named as PROSE in a text-bearing value — a
    /// worktree, cwd, tmux/SSH, sandbox, or CLI-flag token anywhere in the
    /// text (vertical V2b discrimination list).
    /// Client-side strict superset of the mirrored host categories: the
    /// host law stays authoritative host-side; this layer exists so the
    /// watershed dimensions are rejected before transport even when they
    /// are not shaped like commands or paths.
    #[error("execution detail named in text")]
    ExecutionDetail,
}

//! Canonical encode-side text admission for Tachi delegation.
//! Tachi remains authoritative; this client guard rejects more, never less.

use zeroclaw_api::delegation_admission::ForbiddenCategory;
use zeroclaw_api::taskintent::{AttemptRef, TaskRef};

/// Substrings whose presence in any text-bearing value is
/// credential-shaped (TB-4 category 1). Byte-identical list to the tachi
/// host's `CREDENTIAL_MARKERS`.
const CREDENTIAL_MARKERS: &[&str] = &[
    "-----BEGIN OPENSSH PRIVATE KEY",
    "-----BEGIN RSA PRIVATE KEY",
    "-----BEGIN PRIVATE KEY",
    "-----BEGIN EC PRIVATE KEY",
    "sk-ant-",
    "sk-proj-",
    "ghp_",
    "github_pat_",
    "gho_",
    "xoxb-",
    "xoxp-",
    "AKIA",
    "api_key=",
    "apikey:",
    "password=",
    "bearer ",
];

/// Leading tokens that make a value a shell/SSH/tmux/container command
/// (TB-4 category 2; includes the harness CLI names the Parent must
/// never place in an intent). Byte-identical list to the tachi host's
/// `COMMAND_LEAD_TOKENS`.
const COMMAND_LEAD_TOKENS: &[&str] = &[
    "sh", "bash", "zsh", "dash", "ksh", "exec", "eval", "source", "sudo", "su", "ssh", "scp",
    "sftp", "mosh", "telnet", "tmux", "screen", "docker", "podman", "kubectl", "nerdctl", "git",
    "cargo", "npm", "pnpm", "yarn", "python", "python3", "node", "ruby", "codex", "claude",
    "gemini", "opencode", "aider", "grok", "rm", "mv", "cp", "chmod", "chown", "curl", "wget",
    "nc",
];

/// Markers that make a value a worktree/filesystem path (TB-4 category
/// 3). Byte-identical list to the tachi host's `WORKTREE_MARKERS`.
const WORKTREE_MARKERS: &[&str] = &[
    "/worktrees/",
    "worktree_path",
    ".git/",
    "/Users/",
    "/home/",
    "/tmp/",
    "/var/folders/",
    "\\.git\\",
];

/// Markers for Private-Dyad-labeled content (TB-4 category 4).
const PRIVATE_DYAD_MARKERS: &[&str] = &["private dyad", "private_dyad", "private-dyad"];

/// Execution-placement tokens banned ANYWHERE in a text-bearing value
/// (vertical V2b discrimination list: worktree, tmux/SSH, sandbox flags,
/// cwd — TB-4/TB-1). Word-boundary matched; `working directory` is
/// phrase-matched because it is two words.
pub const WATERSHED_PLACEMENT_TOKENS: &[&str] = &["worktree", "tmux", "ssh", "sandbox", "cwd"];
const WATERSHED_PLACEMENT_PHRASES: &[&str] = &["working directory"];

/// Admit raw delegated text before request claims or transport.
/// The same engine also guards typed TaskIntent composition.
pub fn scan_text(text: &str) -> Result<(), ForbiddenCategory> {
    let lower = text.to_ascii_lowercase();

    for marker in CREDENTIAL_MARKERS {
        if text.contains(marker) || lower.contains(&marker.to_ascii_lowercase()) {
            return Err(ForbiddenCategory::Credential);
        }
    }
    let first_token = lower.split_whitespace().next().unwrap_or("");
    if COMMAND_LEAD_TOKENS.contains(&first_token) {
        return Err(ForbiddenCategory::Command);
    }
    if lower.starts_with("./") || lower.starts_with('/') || lower.starts_with('~') {
        return Err(ForbiddenCategory::WorktreePath);
    }
    for marker in WORKTREE_MARKERS {
        if lower.contains(&marker.to_ascii_lowercase()) {
            return Err(ForbiddenCategory::WorktreePath);
        }
    }
    for marker in PRIVATE_DYAD_MARKERS {
        if lower.contains(marker) {
            return Err(ForbiddenCategory::PrivateDyad);
        }
    }
    if text.contains(TaskRef::WIRE_PREFIX) || text.contains(AttemptRef::WIRE_PREFIX) {
        return Err(ForbiddenCategory::CallerMintedRef);
    }

    // Client-side watershed layer (vertical V2b discrimination list).
    // This is a deliberate STRICT SUPERSET of the mirrored host law:
    // the host's five categories stay byte-identical above; this layer
    // exists because the watershed dimensions are semantic, not
    // shape-based — `name the model`, `use a worktree`, `pass --flag`
    // are forbidden as PROSE, wherever they appear. The client may
    // reject more than the host; it may never reject less.
    //
    // Vendor, model, and harness NAMES are deliberately not in this layer
    // (ADR-017 §3): task text that mentions a harness is ordinary content.
    // What a task may not do is choose its own execution placement, so the
    // placement vocabulary and flags below stay banned.
    for word in lower.split(|c: char| !c.is_ascii_alphanumeric()) {
        if WATERSHED_PLACEMENT_TOKENS.contains(&word) {
            return Err(ForbiddenCategory::ExecutionDetail);
        }
    }
    for phrase in WATERSHED_PLACEMENT_PHRASES {
        if lower.contains(phrase) {
            return Err(ForbiddenCategory::ExecutionDetail);
        }
    }
    // Mid-string relative paths (`worktree ../feature-v2b`, `see
    // ./docs/x`) — the prefix checks above only catch leading paths.
    if lower.contains("../") || lower.contains(" ./") || lower.contains(" ~/") {
        return Err(ForbiddenCategory::ExecutionDetail);
    }
    // CLI flags (`--fast`, `-rf`, including parenthesized/quoted forms
    // like `(--full-auto)`): any `--` run in the text is flag-shaped;
    // beyond that, whitespace tokens with leading punctuation stripped
    // that begin with `-` followed by a letter/digit are flag-shaped.
    if lower.contains("--") {
        return Err(ForbiddenCategory::ExecutionDetail);
    }
    for token in lower.split_whitespace() {
        let trimmed = token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-');
        let stripped = trimmed.trim_start_matches('-');
        if stripped.len() != trimmed.len()
            && stripped.chars().next().is_some_and(char::is_alphanumeric)
        {
            return Err(ForbiddenCategory::ExecutionDetail);
        }
    }
    Ok(())
}

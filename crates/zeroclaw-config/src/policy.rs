use anyhow::Context as _;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// Re-export from zeroclaw-config.
pub use crate::autonomy::AutonomyLevel;

/// Risk score for shell command execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandRiskLevel {
    Low,
    Medium,
    High,
}

/// Classifies whether a tool operation is read-only or side-effecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOperation {
    Read,
    Act,
}

/// Sliding-window action tracker for rate limiting.
#[derive(Debug)]
pub struct ActionTracker {
    /// Timestamps of recent actions (kept within the last hour).
    actions: Mutex<Vec<Instant>>,
}

const ACTION_WINDOW: Duration = Duration::from_secs(3600);

fn retain_actions_after(actions: &mut Vec<Instant>, cutoff: Option<Instant>) {
    if let Some(cutoff) = cutoff {
        actions.retain(|timestamp| *timestamp > cutoff);
    }
}

impl Default for ActionTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ActionTracker {
    pub fn new() -> Self {
        Self {
            actions: Mutex::new(Vec::new()),
        }
    }

    /// Record an action and return the current count within the window.
    pub fn record(&self) -> usize {
        let mut actions = self.actions.lock();
        let now = Instant::now();
        retain_actions_after(&mut actions, now.checked_sub(ACTION_WINDOW));
        actions.push(now);
        actions.len()
    }

    /// Count of actions in the current window without recording.
    pub fn count(&self) -> usize {
        let mut actions = self.actions.lock();
        retain_actions_after(&mut actions, Instant::now().checked_sub(ACTION_WINDOW));
        actions.len()
    }
}

impl Clone for ActionTracker {
    fn clone(&self) -> Self {
        let actions = self.actions.lock();
        Self {
            actions: Mutex::new(actions.clone()),
        }
    }
}

/// Per-sender sliding-window rate limiter. The bucket map is Arc-shared
/// so cloned policies (SubAgents) consume from the same budgets.
#[derive(Debug)]
pub struct PerSenderTracker {
    buckets: std::sync::Arc<parking_lot::Mutex<HashMap<String, ActionTracker>>>,
}

impl PerSenderTracker {
    /// Bucket key used when no per-sender context is available (cron, CLI).
    pub const GLOBAL_KEY: &'static str = "__global__";

    /// Create an empty tracker with no sender buckets.
    pub fn new() -> Self {
        Self {
            buckets: std::sync::Arc::new(parking_lot::Mutex::new(HashMap::new())),
        }
    }

    /// Resolve the current sender key from the task-local, falling back to GLOBAL_KEY.
    fn current_key() -> String {
        zeroclaw_api::TOOL_LOOP_THREAD_ID
            .try_with(|v| v.clone())
            .ok()
            .flatten()
            .unwrap_or_else(|| Self::GLOBAL_KEY.to_string())
    }

    /// Record one action for the current sender. Returns `true` if allowed
    /// (count after recording <= max), `false` if budget exhausted.
    pub fn record_for_current(&self, max: u32) -> bool {
        let key = Self::current_key();
        self.record_within(&key, max)
    }

    /// Record one action for `key`. Allows the action when count == max (≤ max);
    /// blocks and returns false when count > max.
    pub fn record_within(&self, key: &str, max: u32) -> bool {
        let mut buckets = self.buckets.lock();
        let tracker = buckets.entry(key.to_string()).or_default();
        let count = tracker.record();
        count <= max as usize
    }

    /// Check if the current sender is at or over the limit (without recording).
    pub fn is_limited_for_current(&self, max: u32) -> bool {
        let key = Self::current_key();
        self.is_exhausted(&key, max)
    }

    pub fn is_exhausted(&self, key: &str, max: u32) -> bool {
        if max == 0 {
            return true;
        }
        let mut buckets = self.buckets.lock();
        match buckets.get_mut(key) {
            Some(tracker) => tracker.count() >= max as usize,
            None => false,
        }
    }
}

impl Clone for PerSenderTracker {
    fn clone(&self) -> Self {
        Self {
            buckets: std::sync::Arc::clone(&self.buckets),
        }
    }
}

impl Default for PerSenderTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct SecurityPolicy {
    pub autonomy: AutonomyLevel,
    /// Name of the risk profile this policy was built from. Used to gate
    /// delegation: a Delegate may only target an agent sharing the caller's
    /// risk profile. Empty when constructed outside the profile path.
    pub risk_profile_name: String,
    pub workspace_dir: PathBuf,
    pub config_path: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    pub workspace_only: bool,
    pub allowed_commands: Vec<String>,
    pub forbidden_paths: Vec<String>,
    /// Directories the agent can read AND write under. Includes
    /// `RiskProfileConfig.allowed_roots` plus any cross-agent
    /// `AccessMode::ReadWrite` grants resolved from
    /// `agent.workspace.access` at policy construction time.
    pub allowed_roots: Vec<PathBuf>,
    /// Directories the agent can read but NOT write under. Populated
    /// from cross-agent `AccessMode::Read` grants at policy
    /// construction time. Empty when no read-only cross-agent access
    /// is configured.
    pub allowed_roots_read_only: Vec<PathBuf>,
    /// Directories the agent can write but NOT read under. Populated
    /// from cross-agent `AccessMode::Write` grants; read-side tools
    /// ignore this list.
    pub allowed_roots_write_only: Vec<PathBuf>,
    pub max_actions_per_hour: u32,
    pub max_cost_per_day_cents: u32,
    pub require_approval_for_medium_risk: bool,
    pub block_high_risk_commands: bool,
    pub shell_env_passthrough: Vec<String>,
    pub shell_timeout_secs: u64,
    /// Tool name allowlist. `None` is unrestricted (default for agents
    /// without an explicit `risk_profile.allowed_tools` setting).
    /// `Some(vec![])` denies every tool. `Some(list)` admits only the
    /// listed names. Enforced at the agent loop's tool-dispatch site.
    pub allowed_tools: Option<Vec<String>>,
    /// Tool name denylist. Subtracts from the allowed set (whether the
    /// allowed set comes from `allowed_tools` or from the unrestricted
    /// default). `None` and `Some(vec![])` both mean "exclude nothing".
    pub excluded_tools: Option<Vec<String>>,
    /// Whether a non-empty `allowed_tools` auto-admits runtime-discovered
    /// MCP tools. Mirrors `RiskProfileConfig.mcp_discovered_tool_policy`.
    pub mcp_discovered_tool_policy: crate::autonomy::McpDiscoveredToolPolicy,
    /// Tools that never require approval in this profile. Mirrors
    /// `RiskProfileConfig.auto_approve`.
    pub auto_approve: Vec<String>,
    /// Tools that always require approval in this profile. Mirrors
    /// `RiskProfileConfig.always_ask`.
    pub always_ask: Vec<String>,
    /// Whether the sandbox is enabled for this profile. `None`
    /// inherits the global default at the call site.
    pub sandbox_enabled: Option<bool>,
    /// Sandbox backend identifier (e.g. `"firejail"`, `"landlock"`).
    /// `None` inherits the global default.
    pub sandbox_backend: Option<String>,
    /// Extra arguments forwarded to firejail when `sandbox_backend`
    /// resolves to `"firejail"`.
    pub firejail_args: Vec<String>,
    pub tracker: PerSenderTracker,
}

impl SecurityPolicy {
    /// True when `name` is admissible under the current policy.
    /// `allowed_tools = None` is unrestricted; `Some(list)` is the
    /// allowlist. `excluded_tools` always subtracts.
    pub fn is_tool_allowed(&self, name: &str) -> bool {
        let allowed = self.allowed_tools.as_ref().is_none_or(|list| {
            list.iter()
                .any(|t| crate::node_allowlist::tool_name_matches(t, name))
        });
        allowed && !self.is_tool_excluded(name)
    }

    pub fn is_tool_excluded(&self, name: &str) -> bool {
        self.excluded_tools.as_ref().is_some_and(|list| {
            list.iter()
                .any(|t| crate::node_allowlist::tool_name_matches(t, name))
        })
    }
}

/// Default allowed commands for Unix platforms.
#[cfg(not(target_os = "windows"))]
pub(crate) fn default_allowed_commands() -> Vec<String> {
    #[allow(unused_mut)]
    let mut cmds = vec![
        "git".into(),
        "npm".into(),
        "cargo".into(),
        "ls".into(),
        "cat".into(),
        "grep".into(),
        "find".into(),
        "echo".into(),
        "pwd".into(),
        "wc".into(),
        "head".into(),
        "tail".into(),
        "date".into(),
        "df".into(),
        "du".into(),
        "uname".into(),
        "uptime".into(),
        "hostname".into(),
        "python".into(),
        "python3".into(),
        "pip".into(),
        "node".into(),
    ];
    // `free` is Linux-only; it does not exist on macOS or other BSDs.
    #[cfg(target_os = "linux")]
    cmds.push("free".into());
    cmds
}

/// Default allowed commands for Windows platforms.
/// Includes both native Windows commands and their Unix equivalents
/// (available via Git for Windows, WSL, etc.).
#[cfg(target_os = "windows")]
pub(crate) fn default_allowed_commands() -> Vec<String> {
    vec![
        // Cross-platform tools
        "git".into(),
        "npm".into(),
        "cargo".into(),
        "echo".into(),
        // Windows-native equivalents
        "dir".into(),
        "type".into(),
        "findstr".into(),
        "where".into(),
        "more".into(),
        "date".into(),
        // Unix commands (available via Git for Windows / MSYS2)
        "ls".into(),
        "cat".into(),
        "grep".into(),
        "find".into(),
        "pwd".into(),
        "wc".into(),
        "head".into(),
        "tail".into(),
        "df".into(),
        "du".into(),
        "uname".into(),
        "uptime".into(),
        "hostname".into(),
        "python".into(),
        "python3".into(),
        "pip".into(),
        "node".into(),
    ]
}

/// Default forbidden paths for Unix platforms.
#[cfg(not(target_os = "windows"))]
pub(crate) fn default_forbidden_paths() -> Vec<String> {
    vec![
        "/etc".into(),
        "/root".into(),
        "/home".into(),
        "/usr".into(),
        "/bin".into(),
        "/sbin".into(),
        "/lib".into(),
        "/opt".into(),
        "/boot".into(),
        "/dev".into(),
        "/proc".into(),
        "/sys".into(),
        "/var".into(),
        "/tmp".into(),
        "~/.ssh".into(),
        "~/.gnupg".into(),
        "~/.aws".into(),
        "~/.config".into(),
    ]
}

/// Default forbidden paths for Windows platforms.
#[cfg(target_os = "windows")]
pub(crate) fn default_forbidden_paths() -> Vec<String> {
    vec![
        "C:\\Windows".into(),
        "C:\\Windows\\System32".into(),
        "C:\\Program Files".into(),
        "C:\\Program Files (x86)".into(),
        "C:\\ProgramData".into(),
        "~/.ssh".into(),
        "~/.gnupg".into(),
        "~/.aws".into(),
        "~/.config".into(),
    ]
}

fn roots_contain(roots: &[PathBuf], expanded: &Path) -> bool {
    roots.iter().any(|root| {
        let canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
        expanded.starts_with(&canonical) || expanded.starts_with(root)
    })
}

fn path_contains(parent: &Path, child: &Path) -> bool {
    let canonical_parent = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf());
    let canonical_child = child.canonicalize().unwrap_or_else(|_| child.to_path_buf());
    canonical_child.starts_with(&canonical_parent) || child.starts_with(parent)
}

/// Specific kind of escalation violation returned by
/// [`SecurityPolicy::ensure_no_escalation_beyond`]. Each variant names
/// the field that violated subset semantics so the SubAgent spawn path
/// can produce a precise error to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EscalationViolation {
    /// Child raises `autonomy` above the parent (e.g. parent
    /// `Supervised`, child `Full`). The autonomy level gates the
    /// entire `can_act` and approval flow, so silent escalation here
    /// would bypass every other guard.
    AutonomyAboveParent {
        child: AutonomyLevel,
        parent: AutonomyLevel,
    },
    /// `child.allowed_roots` contains a path the parent cannot rw.
    ReadWriteRootNotInParent { path: PathBuf },
    /// `child.allowed_roots_read_only` contains a path the parent
    /// cannot read at all (not in parent rw or read-only lists).
    ReadOnlyRootNotInParent { path: PathBuf },
    /// `child.allowed_roots_write_only` contains a path the parent
    /// cannot write at all (not in parent rw or write-only lists).
    WriteOnlyRootNotInParent { path: PathBuf },
    /// `child.allowed_commands` contains a shell command the parent
    /// has no allowance for.
    CommandNotInParent { command: String },
    /// Parent enforces workspace_only but the child override tries to
    /// turn it off.
    WorkspaceOnlyDisabledByChild,
    /// Child drops a forbidden_paths entry the parent enforces. Subset
    /// semantics on forbidden lists run the opposite direction from
    /// allowlists: parent ⊆ child, so the child can ADD entries but
    /// never DROP them.
    ForbiddenPathDroppedByChild { path: String },
    /// Child raises `shell_env_passthrough` to leak env vars the
    /// parent declined to forward.
    ShellEnvPassthroughExpanded { variable: String },
    /// Child override raises `max_actions_per_hour` above the
    /// parent's ceiling.
    MaxActionsExceeded { child: u32, parent: u32 },
    /// Child override raises `max_cost_per_day_cents` above the
    /// parent's ceiling.
    MaxCostExceeded { child: u32, parent: u32 },
    /// Child override raises `shell_timeout_secs` above the parent's
    /// ceiling. The shell budget is a runaway-process guard; raising
    /// it on the child side defeats the parent's intent.
    ShellTimeoutExceeded { child: u64, parent: u64 },
    /// Child flips `block_high_risk_commands` from `true` (parent) to
    /// `false`, opening the high-risk command surface the parent
    /// closed.
    BlockHighRiskCommandsDisabledByChild,
    /// Child flips `require_approval_for_medium_risk` from `true`
    /// (parent) to `false`, bypassing the human-in-the-loop step the
    /// parent required.
    RequireApprovalDisabledByChild,
}

impl std::fmt::Display for EscalationViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AutonomyAboveParent { child, parent } => {
                write!(f, "subagent autonomy={child:?} exceeds parent's {parent:?}")
            }
            Self::ReadWriteRootNotInParent { path } => write!(
                f,
                "subagent allowed_roots entry {path:?} is not contained within any of the parent's allowed_roots entries"
            ),
            Self::ReadOnlyRootNotInParent { path } => write!(
                f,
                "subagent allowed_roots_read_only entry {path:?} is not contained within the parent's allowed_roots or allowed_roots_read_only"
            ),
            Self::WriteOnlyRootNotInParent { path } => write!(
                f,
                "subagent allowed_roots_write_only entry {path:?} is not contained within the parent's allowed_roots or allowed_roots_write_only"
            ),
            Self::CommandNotInParent { command } => write!(
                f,
                "subagent allowed_commands entry {command:?} is not present on the parent's allowed_commands"
            ),
            Self::WorkspaceOnlyDisabledByChild => write!(
                f,
                "subagent attempts to disable workspace_only but the parent enforces it"
            ),
            Self::ForbiddenPathDroppedByChild { path } => write!(
                f,
                "subagent drops forbidden_paths entry {path:?} that the parent enforces"
            ),
            Self::ShellEnvPassthroughExpanded { variable } => write!(
                f,
                "subagent shell_env_passthrough entry {variable:?} is not present on the parent's list"
            ),
            Self::MaxActionsExceeded { child, parent } => write!(
                f,
                "subagent max_actions_per_hour={child} exceeds parent's {parent}"
            ),
            Self::MaxCostExceeded { child, parent } => write!(
                f,
                "subagent max_cost_per_day_cents={child} exceeds parent's {parent}"
            ),
            Self::ShellTimeoutExceeded { child, parent } => write!(
                f,
                "subagent shell_timeout_secs={child} exceeds parent's {parent}"
            ),
            Self::BlockHighRiskCommandsDisabledByChild => write!(
                f,
                "subagent attempts to set block_high_risk_commands=false but the parent enforces it"
            ),
            Self::RequireApprovalDisabledByChild => write!(
                f,
                "subagent attempts to set require_approval_for_medium_risk=false but the parent enforces it"
            ),
        }
    }
}

impl std::error::Error for EscalationViolation {}

impl Default for SecurityPolicy {
    fn default() -> Self {
        Self {
            autonomy: AutonomyLevel::Supervised,
            risk_profile_name: String::new(),
            workspace_dir: PathBuf::from("."),
            config_path: None,
            data_dir: None,
            workspace_only: true,
            allowed_commands: default_allowed_commands(),
            forbidden_paths: default_forbidden_paths(),
            allowed_roots: Vec::new(),
            allowed_roots_read_only: Vec::new(),
            allowed_roots_write_only: Vec::new(),
            max_actions_per_hour: 20,
            max_cost_per_day_cents: 500,
            require_approval_for_medium_risk: true,
            block_high_risk_commands: true,
            shell_env_passthrough: vec![],
            shell_timeout_secs: 60,
            allowed_tools: None,
            excluded_tools: None,
            mcp_discovered_tool_policy: crate::autonomy::McpDiscoveredToolPolicy::default(),
            auto_approve: vec![],
            always_ask: vec![],
            sandbox_enabled: None,
            sandbox_backend: None,
            firejail_args: vec![],
            tracker: PerSenderTracker::new(),
        }
    }
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }
}

fn expand_user_path(path: &str) -> PathBuf {
    if path == "~"
        && let Some(home) = home_dir()
    {
        return home;
    }

    if let Some(stripped) = path.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return home.join(stripped);
    }

    PathBuf::from(path)
}

/// Resolve `path` to its real target, following symlinks component by component.
///
/// Unlike [`Path::canonicalize`] this does NOT require the target to exist, so a
/// path whose leaf is about to be created (`touch link/new.txt`) still resolves,
/// while a symlinked component is followed to its target even when that target
/// does not exist yet (a *dangling* symlink `link -> /outside/new` — the write
/// would still land outside, so it must be resolved and blocked, exactly as
/// `file_write` blocks writing through a symlink leaf). Lexical `.`/`..` are
/// applied without touching the filesystem. Symlink chains are bounded to guard
/// against cycles: exhausting the hop budget returns `None` ("could not
/// resolve"), and callers fail closed by BLOCKING the path - a crafted cycle
/// never falls back to the literal input. An unreadable symlink (`read_link`
/// fails after `symlink_metadata` confirms a link) likewise returns `None` so
/// callers fail closed rather than falling through to literal-component strip.
fn resolve_symlinked_path(path: &Path) -> Option<PathBuf> {
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut current = path.to_path_buf();
    // Bounds symlink hops so a cycle cannot spin forever. Only symlink hops
    // consume the budget; stripping non-existent trailing components is bounded
    // by path depth. `None` on exhaustion means "could not resolve"; the caller
    // FAILS CLOSED (blocks), so a crafted deep/cyclic symlink chain cannot
    // exhaust the budget and fall back to an allowed in-workspace path.
    let mut budget: u32 = 64;
    loop {
        // Canonicalizing the deepest EXISTING prefix normalizes it the same way
        // `is_resolved_path_allowed` normalizes the workspace root (e.g. `/tmp`
        // vs its real location), so ordinary in-workspace paths stay in-workspace.
        if let Ok(resolved) = current.canonicalize() {
            let mut result = resolved;
            for component in suffix.iter().rev() {
                result.push(component);
            }
            return Some(result);
        }
        // A *dangling* symlink cannot be canonicalized (its target does not
        // exist), yet a write through it still lands at the target. Follow it
        // explicitly so the resolved path reflects where the write would go.
        // Only symlink hops consume the budget (a symlink cycle is the only way
        // to loop forever); stripping non-existent trailing components is bounded
        // by the finite path depth, so a deeply-nested create is NOT false-blocked.
        if current
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
        {
            // Confirmed symlink but unreadable: fail closed (same posture as
            // hop-budget exhaustion). Do not fall through to literal strip —
            // that would skip following the link and treat the path as if the
            // symlink were an ordinary missing component.
            let Ok(target) = std::fs::read_link(&current) else {
                return None;
            };
            if budget == 0 {
                return None;
            }
            budget -= 1;
            current = if target.is_absolute() {
                target
            } else {
                current
                    .parent()
                    .map(|parent| parent.join(&target))
                    .unwrap_or(target)
            };
            continue;
        }
        // Otherwise strip the trailing (non-existent) component and retry on the
        // parent. An absolute input terminates at the filesystem root, which
        // always canonicalizes; running out of components should be unreachable
        // for the absolute inputs the caller passes, so treat it as unresolvable
        // and fail closed rather than trusting the literal path.
        match (current.file_name(), current.parent()) {
            (Some(name), Some(parent)) if !parent.as_os_str().is_empty() => {
                suffix.push(name.to_os_string());
                current = parent.to_path_buf();
            }
            _ => return None,
        }
    }
}

fn is_null_device(path: &Path) -> bool {
    #[cfg(not(target_os = "windows"))]
    {
        path == Path::new("/dev/null")
    }
    #[cfg(target_os = "windows")]
    {
        let s = path.to_string_lossy();
        let lower = s.to_ascii_lowercase();
        lower == "nul" || lower == r"\\.\nul"
    }
}

fn rootless_path(path: &Path) -> Option<PathBuf> {
    let mut relative = PathBuf::new();

    for component in path.components() {
        match component {
            std::path::Component::Prefix(_)
            | std::path::Component::RootDir
            | std::path::Component::CurDir => {}
            std::path::Component::ParentDir => return None,
            std::path::Component::Normal(part) => relative.push(part),
        }
    }

    if relative.as_os_str().is_empty() {
        None
    } else {
        Some(relative)
    }
}

struct NormalizedRootlessPath {
    drive: Option<u8>,
    text: String,
}

fn normalized_rootless_path_text(path: &Path) -> Option<NormalizedRootlessPath> {
    let mut text = path.to_string_lossy().replace('\\', "/");

    if let Some(rest) = text.strip_prefix("//?/UNC/") {
        text = rest.to_string();
    } else if let Some(rest) = text.strip_prefix("//?/") {
        text = rest.to_string();
    }

    let mut drive = None;
    let bytes = text.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        drive = Some(bytes[0].to_ascii_lowercase());
        text = text[2..].to_string();
    }

    let parts: Vec<&str> = text
        .trim_start_matches('/')
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();

    if parts.is_empty() || parts.contains(&"..") {
        None
    } else {
        Some(NormalizedRootlessPath {
            drive,
            text: parts.join("/"),
        })
    }
}

fn workspace_prefixed_relative_suffix(path: &Path, workspace_dir: &Path) -> Option<PathBuf> {
    let path_text = normalized_rootless_path_text(path)?;
    let workspace_text = normalized_rootless_path_text(workspace_dir)?;

    if path_text.drive.is_some() && path_text.drive != workspace_text.drive {
        return None;
    }

    if path_text.text == workspace_text.text {
        return Some(PathBuf::new());
    }

    let prefix = format!("{}/", workspace_text.text);
    path_text
        .text
        .strip_prefix(&prefix)
        .map(|suffix| PathBuf::from(suffix.replace('/', std::path::MAIN_SEPARATOR_STR)))
}

/// Skip leading environment variable assignments (e.g. `FOO=bar cmd args`).
/// Returns the remainder starting at the first non-assignment word.
fn skip_env_assignments(s: &str) -> &str {
    let mut rest = s;
    loop {
        let Some(word) = rest.split_whitespace().next() else {
            return rest;
        };
        // Environment assignment: contains '=' and starts with a letter or underscore
        if word.contains('=')
            && word
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        {
            // Advance past this word
            rest = rest[word.len()..].trim_start();
        } else {
            return rest;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuoteState {
    None,
    Single,
    Double,
}

fn split_unquoted_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote = QuoteState::None;
    let mut escaped = false;
    // Heredoc state: Some(delim) while inside a heredoc body.
    let mut heredoc_delimiter: Option<String> = None;
    // Accumulates the current line while inside a heredoc body, for terminator detection.
    let mut heredoc_line_buf = String::new();
    // True while reading the delimiter word that follows `<<`.
    let mut reading_heredoc_word = false;
    let mut heredoc_word_buf = String::new();
    let mut chars = command.chars().peekable();

    let push_segment = |segments: &mut Vec<String>, current: &mut String| {
        let trimmed = current.trim();
        if !trimmed.is_empty() {
            segments.push(trimmed.to_string());
        }
        current.clear();
    };

    while let Some(ch) = chars.next() {
        match quote {
            QuoteState::Single => {
                if ch == '\'' {
                    quote = QuoteState::None;
                }
                current.push(ch);
            }
            QuoteState::Double => {
                if escaped {
                    escaped = false;
                    current.push(ch);
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    current.push(ch);
                    continue;
                }
                if ch == '"' {
                    quote = QuoteState::None;
                }
                current.push(ch);
            }
            QuoteState::None => {
                if escaped {
                    escaped = false;
                    if heredoc_delimiter.is_some() {
                        heredoc_line_buf.push(ch);
                    } else {
                        current.push(ch);
                    }
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    if heredoc_delimiter.is_some() {
                        heredoc_line_buf.push(ch);
                    } else {
                        current.push(ch);
                    }
                    continue;
                }

                // Reading the delimiter word that follows `<<`.
                if reading_heredoc_word {
                    if ch == '\n' {
                        // Finalise the delimiter and enter the heredoc body.
                        let raw = heredoc_word_buf.trim().trim_start_matches('-');
                        let delim = raw
                            .trim_matches(|c| c == '\'' || c == '"' || c == '\\')
                            .to_string();
                        if !delim.is_empty() {
                            heredoc_delimiter = Some(delim);
                        }
                        heredoc_word_buf.clear();
                        reading_heredoc_word = false;
                        // The newline after `<<WORD` belongs to the same segment.
                        current.push(ch);
                    } else {
                        heredoc_word_buf.push(ch);
                        current.push(ch);
                    }
                    continue;
                }

                if let Some(delim) = heredoc_delimiter.as_deref() {
                    if ch == '\n' {
                        if heredoc_line_buf.trim() == delim {
                            // Terminator line reached — end of heredoc body.
                            heredoc_delimiter = None;
                            heredoc_line_buf.clear();
                            push_segment(&mut segments, &mut current);
                        } else {
                            heredoc_line_buf.clear();
                        }
                    } else {
                        heredoc_line_buf.push(ch);
                    }
                    continue;
                }

                match ch {
                    '\'' => {
                        quote = QuoteState::Single;
                        current.push(ch);
                    }
                    '"' => {
                        quote = QuoteState::Double;
                        current.push(ch);
                    }
                    ';' | '\n' => push_segment(&mut segments, &mut current),
                    '|' => {
                        if chars.next_if_eq(&'|').is_some() {
                            // Consume full `||`; both characters are separators.
                        }
                        push_segment(&mut segments, &mut current);
                    }
                    '&' => {
                        if chars.next_if_eq(&'&').is_some() {
                            // `&&` is a separator; single `&` is handled separately.
                            push_segment(&mut segments, &mut current);
                        } else {
                            current.push(ch);
                        }
                    }
                    '<' => {
                        current.push(ch);
                        // Detect `<<` (heredoc) but not `<<<` (here-string).
                        if chars.peek() == Some(&'<') {
                            let second = chars.next().unwrap();
                            current.push(second);
                            if chars.peek() != Some(&'<') {
                                reading_heredoc_word = true;
                            }
                            // `<<<` falls through with no heredoc tracking.
                        }
                    }
                    _ => current.push(ch),
                }
            }
        }
    }

    let trimmed = current.trim();
    if !trimmed.is_empty() {
        segments.push(trimmed.to_string());
    }

    segments
}

/// Detect a single unquoted `&` operator (background/chain). `&&` is allowed.
/// Strip fd-merge redirect patterns (`N>&M`, `N<&M`, `>&N`, `<&N`, `N>&-`, etc.)
/// so their `&` doesn't get flagged as a background operator.
fn strip_fd_merge_redirects(command: &str) -> String {
    use std::sync::OnceLock;
    // Matches patterns like: 2>&1, 1>&2, >&2, <&0, 2<&-, >&-
    static FD_MERGE_RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = FD_MERGE_RE.get_or_init(|| {
        regex::Regex::new(r"\d*[><]&[\d-]").expect("FD_MERGE_RE regex must compile")
    });
    re.replace_all(command, "").to_string()
}

/// We treat any standalone `&` as unsafe in policy validation because it can
/// chain hidden sub-commands and escape foreground timeout expectations.
fn contains_unquoted_single_ampersand(command: &str) -> bool {
    let mut quote = QuoteState::None;
    let mut escaped = false;
    let mut chars = command.chars().peekable();

    while let Some(ch) = chars.next() {
        match quote {
            QuoteState::Single => {
                if ch == '\'' {
                    quote = QuoteState::None;
                }
            }
            QuoteState::Double => {
                if escaped {
                    escaped = false;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    continue;
                }
                if ch == '"' {
                    quote = QuoteState::None;
                }
            }
            QuoteState::None => {
                if escaped {
                    escaped = false;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    continue;
                }
                match ch {
                    '\'' => quote = QuoteState::Single,
                    '"' => quote = QuoteState::Double,
                    // This must consume the second '&' so `&&` is not later
                    // re-read as a lone trailing '&'.
                    '&' if chars.next_if_eq(&'&').is_none() => {
                        return true;
                    }
                    _ => {}
                }
            }
        }
    }

    false
}

/// Detect an unquoted character in a shell command.
fn contains_unquoted_char(command: &str, target: char) -> bool {
    let mut quote = QuoteState::None;
    let mut escaped = false;

    for ch in command.chars() {
        match quote {
            QuoteState::Single => {
                if ch == '\'' {
                    quote = QuoteState::None;
                }
            }
            QuoteState::Double => {
                if escaped {
                    escaped = false;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    continue;
                }
                if ch == '"' {
                    quote = QuoteState::None;
                }
            }
            QuoteState::None => {
                if escaped {
                    escaped = false;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    continue;
                }
                match ch {
                    '\'' => quote = QuoteState::Single,
                    '"' => quote = QuoteState::Double,
                    _ if ch == target => return true,
                    _ => {}
                }
            }
        }
    }

    false
}

/// Returns true if `command` contains an unquoted `>` that is NOT a safe
/// stderr form (`2>/dev/null`, `2>&1`).
fn contains_unsafe_output_redirect(command: &str) -> bool {
    // Strip safe redirect-to-dev patterns (with word boundary enforcement),
    // then fd-merge patterns, then check for remaining `>`.
    use regex::Regex;
    use std::sync::OnceLock;

    static SAFE_OUTPUT_RE: OnceLock<Regex> = OnceLock::new();
    let re = SAFE_OUTPUT_RE.get_or_init(|| {
        Regex::new(&format!(
            r"\d*>[ ]?/dev/({})(\s|[;&|)]|$)",
            safe_device_redirect_names_pattern()
        ))
        .unwrap()
    });

    let safe = re.replace_all(command, "$2").to_string();
    // Also strip fd-merge redirects (2>&1, 1>&2, >&N, etc.)
    let safe = strip_fd_merge_redirects(&safe);
    contains_unquoted_char(&safe, '>')
}

/// Returns true if `command` contains an unquoted `<` that is NOT a heredoc (`<<`)
/// or a safe input redirect from `/dev/*`.
fn contains_unquoted_input_redirect(command: &str) -> bool {
    // Strip here-strings (`<<<`) first, then heredocs (`<<`), then safe /dev/* sources
    // with word boundary enforcement.
    use regex::Regex;
    use std::sync::OnceLock;

    static SAFE_INPUT_RE: OnceLock<Regex> = OnceLock::new();
    let re = SAFE_INPUT_RE.get_or_init(|| {
        Regex::new(r"<[ ]?/dev/(null|zero)(\s|[;&|)]|$)").expect("SAFE_INPUT_RE regex must compile")
    });

    let safe = command.replace("<<<", "").replace("<<", "");
    let safe = re.replace_all(&safe, "$2").to_string();
    // Also strip fd-merge redirects (<&0, <&-, etc.) so they don't leave a bare `<`
    let safe = strip_fd_merge_redirects(&safe);
    contains_unquoted_char(&safe, '<')
}

/// Detect unquoted shell variable expansions like `$HOME`, `$1`, `$?`.
/// Escaped dollars (`\$`) are ignored. Variables inside single quotes are
/// treated as literals and therefore ignored.
fn contains_unquoted_shell_variable_expansion(command: &str) -> bool {
    let mut quote = QuoteState::None;
    let mut escaped = false;
    let chars: Vec<char> = command.chars().collect();

    for i in 0..chars.len() {
        let ch = chars[i];

        match quote {
            QuoteState::Single => {
                if ch == '\'' {
                    quote = QuoteState::None;
                }
                continue;
            }
            QuoteState::Double => {
                if escaped {
                    escaped = false;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    continue;
                }
                if ch == '"' {
                    quote = QuoteState::None;
                    continue;
                }
            }
            QuoteState::None => {
                if escaped {
                    escaped = false;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    continue;
                }
                if ch == '\'' {
                    quote = QuoteState::Single;
                    continue;
                }
                if ch == '"' {
                    quote = QuoteState::Double;
                    continue;
                }
            }
        }

        if ch != '$' {
            continue;
        }

        let Some(next) = chars.get(i + 1).copied() else {
            continue;
        };
        if next.is_ascii_alphanumeric()
            || matches!(
                next,
                '_' | '{' | '(' | '#' | '?' | '!' | '$' | '*' | '@' | '-'
            )
        {
            return true;
        }
    }

    false
}

fn strip_wrapping_quotes(token: &str) -> &str {
    token.trim_matches(|c| c == '"' || c == '\'')
}

fn looks_like_path(candidate: &str) -> bool {
    candidate.starts_with('/')
        || candidate.starts_with("./")
        || candidate.starts_with("../")
        || candidate == "~"
        || candidate.starts_with("~/")
        || (candidate.starts_with('~') && candidate.contains('/'))
        || candidate == "."
        || candidate == ".."
        || candidate.contains('/')
        // Windows path patterns: drive letters (C:\, D:\) and UNC paths (\\server\share)
        || (cfg!(target_os = "windows")
            && (candidate
                .get(1..3)
                .is_some_and(|s| s == ":\\" || s == ":/")
                || candidate.starts_with("\\\\")))
}

fn attached_short_option_value(token: &str) -> Option<&str> {
    // Examples:
    // -f/etc/passwd   -> /etc/passwd
    // -C../outside    -> ../outside
    // -I./include     -> ./include
    let body = token.strip_prefix('-')?;
    if body.starts_with('-') || body.len() < 2 {
        return None;
    }
    let mut chars = body.chars();
    chars.next();
    let value = chars.as_str().trim_start_matches('=').trim();
    if value.is_empty() { None } else { Some(value) }
}

enum RedirectionArgument<'a> {
    Target { prefix: &'a str, target: &'a str },
    NeedsNextToken { prefix: &'a str },
    FdOnly { prefix: &'a str },
    None,
}

fn parse_redirection_argument(token: &str) -> RedirectionArgument<'_> {
    let Some(marker_idx) = token.find(['<', '>']) else {
        return RedirectionArgument::None;
    };
    let prefix = token[..marker_idx].trim();
    let mut rest = &token[marker_idx + 1..];
    rest = rest.trim_start_matches(['<', '>']);
    if let Some(after_amp) = rest.strip_prefix('&') {
        let remaining = after_amp.trim_start_matches(|c: char| c.is_ascii_digit() || c == '-');
        if remaining.is_empty() {
            return RedirectionArgument::FdOnly { prefix };
        }
    }
    rest = rest.trim_start_matches('&');
    rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    let trimmed = rest.trim();
    if trimmed.is_empty() {
        RedirectionArgument::NeedsNextToken { prefix }
    } else {
        RedirectionArgument::Target {
            prefix,
            target: trimmed,
        }
    }
}

const SAFE_DEVICE_REDIRECT_TARGETS: [&str; 4] =
    ["/dev/null", "/dev/stdout", "/dev/stderr", "/dev/zero"];

fn safe_device_redirect_names_pattern() -> String {
    SAFE_DEVICE_REDIRECT_TARGETS
        .iter()
        .map(|target| target.trim_start_matches("/dev/"))
        .collect::<Vec<_>>()
        .join("|")
}

fn is_safe_device_redirect_target(target: &str) -> bool {
    SAFE_DEVICE_REDIRECT_TARGETS.contains(&strip_wrapping_quotes(target).trim())
}

/// Extract the basename from a command path, handling both Unix (`/`) and
/// Windows (`\`) separators so that `C:\Git\bin\git.exe` resolves to `git.exe`.
fn command_basename(raw: &str) -> &str {
    let after_fwd = raw.rsplit('/').next().unwrap_or(raw);
    after_fwd.rsplit('\\').next().unwrap_or(after_fwd)
}

/// Strip common Windows executable suffixes (.exe, .cmd, .bat) for uniform
/// matching against allowlists and risk tables. On non-Windows platforms this
/// is a no-op that returns the input unchanged.
fn strip_windows_exe_suffix(name: &str) -> &str {
    if cfg!(target_os = "windows") {
        name.strip_suffix(".exe")
            .or_else(|| name.strip_suffix(".cmd"))
            .or_else(|| name.strip_suffix(".bat"))
            .unwrap_or(name)
    } else {
        name
    }
}

/// Compare two bare command names using the same semantics everywhere a
/// command allowlist is interpreted. Path-like entries are handled by their
/// callers and deliberately do not pass through this case-folding rule.
fn command_names_equivalent(left: &str, right: &str) -> bool {
    let left_lower = left.to_ascii_lowercase();
    let right_lower = right.to_ascii_lowercase();
    if left_lower == right_lower {
        return true;
    }

    // On Windows, an omitted executable suffix does not distinguish command
    // names (for example, `git` and `git.exe` grant the same command access).
    #[cfg(target_os = "windows")]
    {
        for ext in &[".exe", ".cmd", ".bat"] {
            if right_lower == format!("{left_lower}{ext}") {
                return true;
            }
            if left_lower == format!("{right_lower}{ext}") {
                return true;
            }
        }
    }

    false
}

fn command_allowlist_entries_equivalent(left: &str, right: &str) -> bool {
    let left = strip_wrapping_quotes(left).trim();
    let right = strip_wrapping_quotes(right).trim();
    if left.is_empty() || right.is_empty() {
        return false;
    }

    // Preserve exact equality for paths. Case-folding a path would widen the
    // policy on case-sensitive filesystems.
    if looks_like_path(left) || looks_like_path(right) {
        return left == right;
    }

    command_names_equivalent(left, right)
}

fn is_allowlist_entry_match(allowed: &str, executable: &str, executable_base: &str) -> bool {
    let allowed = strip_wrapping_quotes(allowed).trim();
    if allowed.is_empty() {
        return false;
    }

    // Explicit wildcard support for "allow any command name/path".
    if allowed == "*" {
        return true;
    }

    // Path-like allowlist entries must match the executable token exactly
    // after "~" expansion.
    if looks_like_path(allowed) {
        let allowed_path = expand_user_path(allowed);
        let executable_path = expand_user_path(executable);
        return executable_path == allowed_path;
    }

    // Command-name entries continue to match by basename, case-insensitively.
    // Callers lowercase the basename before it reaches here, so folding only
    // one side would leave an entry written as `Git` or `Docker` unable to
    // match anything.
    command_names_equivalent(allowed, executable_base)
}

impl SecurityPolicy {
    // ── Risk Classification ──────────────────────────────────────────────
    // Risk is assessed per-segment (split on shell operators), and the
    // highest risk across all segments wins. This prevents bypasses like
    // `ls && rm -rf /` from being classified as Low just because `ls` is safe.

    /// Classify command risk. Any high-risk segment marks the whole command high.
    pub fn command_risk_level(&self, command: &str) -> CommandRiskLevel {
        let mut saw_medium = false;

        for segment in split_unquoted_segments(command) {
            let cmd_part = skip_env_assignments(&segment);
            let mut words = cmd_part.split_whitespace();
            let Some(base_raw) = words.next() else {
                continue;
            };

            let base_owned = command_basename(base_raw).to_ascii_lowercase();
            let base = strip_windows_exe_suffix(&base_owned);

            let args: Vec<String> = words.map(|w| w.to_ascii_lowercase()).collect();
            let joined_segment = cmd_part.to_ascii_lowercase();

            // High-risk commands (Unix and Windows)
            if matches!(
                base,
                "rm" | "mkfs"
                    | "dd"
                    | "shutdown"
                    | "reboot"
                    | "halt"
                    | "poweroff"
                    | "sudo"
                    | "su"
                    | "chown"
                    | "chmod"
                    | "useradd"
                    | "userdel"
                    | "usermod"
                    | "passwd"
                    | "mount"
                    | "umount"
                    | "iptables"
                    | "ufw"
                    | "firewall-cmd"
                    | "curl"
                    | "wget"
                    | "nc"
                    | "ncat"
                    | "netcat"
                    | "scp"
                    | "ssh"
                    | "ftp"
                    | "telnet"
                    // Windows-specific high-risk commands
                    | "del"
                    | "rmdir"
                    | "format"
                    | "reg"
                    | "net"
                    | "runas"
                    | "icacls"
                    | "takeown"
                    | "powershell"
                    | "pwsh"
                    | "wmic"
                    | "sc"
                    | "netsh"
            ) {
                return CommandRiskLevel::High;
            }

            if joined_segment.contains("rm -rf /")
                || joined_segment.contains("rm -fr /")
                || joined_segment.contains(":(){:|:&};:")
                // Windows destructive patterns
                || joined_segment.contains("del /s /q")
                || joined_segment.contains("rmdir /s /q")
                || joined_segment.contains("format c:")
            {
                return CommandRiskLevel::High;
            }

            // Medium-risk commands (state-changing, but not inherently destructive)
            let medium = match base {
                "git" => args.first().is_some_and(|verb| {
                    matches!(
                        verb.as_str(),
                        "commit"
                            | "push"
                            | "reset"
                            | "clean"
                            | "rebase"
                            | "merge"
                            | "cherry-pick"
                            | "revert"
                            | "branch"
                            | "checkout"
                            | "switch"
                            | "tag"
                    )
                }),
                "npm" | "pnpm" | "yarn" => args.first().is_some_and(|verb| {
                    matches!(
                        verb.as_str(),
                        "install" | "add" | "remove" | "uninstall" | "update" | "publish"
                    )
                }),
                "cargo" => args.first().is_some_and(|verb| {
                    matches!(
                        verb.as_str(),
                        "add" | "remove" | "install" | "clean" | "publish"
                    )
                }),
                "touch" | "mkdir" | "mv" | "cp" | "ln"
                // Windows medium-risk equivalents
                | "copy" | "xcopy" | "robocopy" | "move" | "ren" | "rename" | "mklink" => true,
                _ => false,
            };

            saw_medium |= medium;
        }

        if saw_medium {
            CommandRiskLevel::Medium
        } else {
            CommandRiskLevel::Low
        }
    }

    /// Validate full command execution policy (allowlist + risk gate).
    pub fn validate_command_execution(
        &self,
        command: &str,
        approved: bool,
    ) -> Result<CommandRiskLevel, String> {
        if !self.is_command_allowed(command) {
            return Err(format!("Command not allowed by security policy: {command}"));
        }

        let risk = self.command_risk_level(command);

        if risk == CommandRiskLevel::High {
            if self.block_high_risk_commands && !self.is_command_explicitly_allowed(command) {
                return Err("Command blocked: high-risk command is disallowed by policy".into());
            }
            if self.autonomy == AutonomyLevel::Supervised && !approved {
                return Err(
                    "Command requires explicit approval (approved=true): high-risk operation"
                        .into(),
                );
            }
        }

        if risk == CommandRiskLevel::Medium
            && self.autonomy == AutonomyLevel::Supervised
            && self.require_approval_for_medium_risk
            && !approved
        {
            return Err(
                "Command requires explicit approval (approved=true): medium-risk operation".into(),
            );
        }

        Ok(risk)
    }

    fn is_command_explicitly_allowed(&self, command: &str) -> bool {
        let segments = split_unquoted_segments(command);
        for segment in &segments {
            let cmd_part = skip_env_assignments(segment);
            let mut words = cmd_part.split_whitespace();
            let raw_executable = strip_wrapping_quotes(words.next().unwrap_or("")).trim();
            let executable = if let Some(idx) = raw_executable.find(['<', '>']) {
                &raw_executable[..idx]
            } else {
                raw_executable
            };
            let base_cmd_owned = command_basename(executable).to_ascii_lowercase();
            let base_cmd = strip_windows_exe_suffix(&base_cmd_owned);

            if base_cmd.is_empty() {
                continue;
            }

            let explicitly_listed = self.allowed_commands.iter().any(|allowed| {
                let allowed = strip_wrapping_quotes(allowed).trim();
                // Skip wildcard — it does not count as an explicit entry.
                if allowed.is_empty() || allowed == "*" {
                    return false;
                }
                is_allowlist_entry_match(allowed, executable, base_cmd)
            });

            if !explicitly_listed {
                return false;
            }
        }

        // At least one real command must be present.
        segments.iter().any(|s| {
            let s = skip_env_assignments(s.trim());
            s.split_whitespace().next().is_some_and(|w| !w.is_empty())
        })
    }

    // ── Layered Command Allowlist ──────────────────────────────────────────
    // Defence-in-depth: five independent gates run in order before the
    // per-segment allowlist check. Each gate targets a specific bypass
    // technique. If any gate rejects, the whole command is blocked.

    pub fn is_command_allowed(&self, command: &str) -> bool {
        if self.autonomy == AutonomyLevel::ReadOnly {
            return false;
        }

        // When the operator has explicitly opted out of all command-level
        // restrictions (wildcard + no high-risk blocking), skip the
        // subshell/expansion guard entirely. This allows backticks,
        // $(), heredocs, etc. in trusted environments.
        let has_wildcard = self.allowed_commands.iter().any(|c| c.trim() == "*");
        if has_wildcard && !self.block_high_risk_commands {
            return true;
        }

        if command.contains('`')
            || contains_unquoted_shell_variable_expansion(command)
            || command.contains("<(")
            || command.contains(">(")
        {
            return false;
        }

        // Block shell redirections that target files. Allow safe forms:
        //   - `2>/dev/null`, `>/dev/null`, `1>/dev/null` (output suppression)
        //   - `2>&1`, `1>&2` (fd merging)
        //   - `<<` heredocs, `<<<` here-strings (input literals)
        if contains_unsafe_output_redirect(command) {
            return false;
        }
        if contains_unquoted_input_redirect(command) {
            return false;
        }

        // Block `tee` — it can write to arbitrary files, bypassing the
        // redirect check above (e.g. `echo secret | tee /etc/crontab`)
        if command
            .split_whitespace()
            .any(|w| w == "tee" || w.ends_with("/tee"))
        {
            return false;
        }

        // Block background command chaining (`&`), which can hide extra
        // sub-commands and outlive timeout expectations. Keep `&&` allowed.
        // Strip fd-merge redirects (N>&M, N<&M) first so their `&` isn't
        // flagged as background chaining.
        let ampersand_check = strip_fd_merge_redirects(command);
        if contains_unquoted_single_ampersand(&ampersand_check) {
            return false;
        }

        // Split on unquoted command separators and validate each sub-command.
        let segments = split_unquoted_segments(command);
        for segment in &segments {
            // Strip leading env var assignments (e.g. FOO=bar cmd)
            let cmd_part = skip_env_assignments(segment);

            let mut words = cmd_part.split_whitespace();
            let raw_executable = strip_wrapping_quotes(words.next().unwrap_or("")).trim();
            // Strip inline redirections from the executable token, e.g.
            // `cat</dev/null` -> `cat`, so the allowlist check sees the real
            // command name rather than the redirect target path.
            let executable = if let Some(idx) = raw_executable.find(['<', '>']) {
                &raw_executable[..idx]
            } else {
                raw_executable
            };
            let base_cmd_owned = command_basename(executable).to_ascii_lowercase();
            let base_cmd = strip_windows_exe_suffix(&base_cmd_owned);

            if base_cmd.is_empty() {
                continue;
            }

            if !self
                .allowed_commands
                .iter()
                .any(|allowed| is_allowlist_entry_match(allowed, executable, base_cmd))
            {
                return false;
            }

            // Validate arguments for the command.
            // Both case-preserved and lowercased argument lists are provided:
            //   - `args_cased` for case-sensitive comparisons (e.g. git -C vs -c)
            //   - `args` (lowercased) for case-insensitive matches (e.g. subcommand names)
            let args_cased: Vec<String> = words.map(|w| w.to_string()).collect();
            let args: Vec<String> = args_cased.iter().map(|w| w.to_ascii_lowercase()).collect();
            if !self.is_args_safe(base_cmd, &args, &args_cased) {
                return false;
            }
        }

        // At least one command must be present
        segments.iter().any(|s| {
            let s = skip_env_assignments(s.trim());
            s.split_whitespace().next().is_some_and(|w| !w.is_empty())
        })
    }

    fn is_args_safe(&self, base: &str, args: &[String], args_cased: &[String]) -> bool {
        let base = base.to_ascii_lowercase();
        match base.as_str() {
            "find" => {
                // find -exec and find -ok allow arbitrary command execution
                !args.iter().any(|arg| arg == "-exec" || arg == "-ok")
            }
            "git" => {
                !args_cased.iter().any(|arg| arg == "-c")
                    && !args.iter().any(|arg| {
                        arg == "config"
                            || arg.starts_with("config.")
                            || arg == "alias"
                            || arg.starts_with("alias.")
                    })
            }
            "python" | "python3" => !args
                .iter()
                .any(|arg| arg.starts_with("-c") || arg.starts_with("-m")),
            "node" => {
                // -e/--eval evaluates argument as JavaScript
                // -p/--print same as --eval but prints the result
                // starts_with covers glued form: node -e'code' (one whitespace token)
                // Ref: https://nodejs.org/api/cli.html
                !args.iter().any(|arg| {
                    arg.starts_with("-e")
                        || arg.starts_with("--eval")
                        || arg.starts_with("-p")
                        || arg.starts_with("--print")
                })
            }
            "pip" | "pip3" => {
                // install/download fetch external packages; setup.py runs arbitrary code
                // Ref: https://blog.phylum.io/python-package-installation-attacks/
                !args.iter().any(|arg| arg == "install" || arg == "download")
            }
            "npm" => {
                // exec can fetch+run remote packages (npx behavior)
                // install fetches external packages; lifecycle scripts run arbitrary code
                // Ref: https://cheatsheetseries.owasp.org/cheatsheets/NPM_Security_Cheat_Sheet.html
                !args.iter().any(|arg| {
                    arg == "exec" || arg == "install" || arg == "i" || arg == "add" || arg == "ci"
                })
            }
            "cargo" => {
                // install fetches+builds external crate; build.rs executes arbitrary code
                // Ref: https://shnatsel.medium.com/do-not-run-any-cargo-commands-on-untrusted-projects
                !args.iter().any(|arg| arg == "install")
            }
            _ => true,
        }
    }

    /// Return the first path-like argument blocked by path policy.
    /// This is best-effort token parsing for shell commands and is intended
    /// as a safety gate before command execution.
    /// String-level command path guard: flags a path argument that is absolute
    /// and outside the workspace, uses `..` traversal, a `~user` form, or a
    /// forbidden prefix. Does NOT resolve symlinks, so it is safe for callers
    /// whose working directory is NOT the workspace (e.g. cron jobs run in
    /// `data_dir`). Shell/skill tools, which run IN the workspace, should use
    /// [`SecurityPolicy::forbidden_workspace_path_argument`], which additionally
    /// follows in-workspace symlinks to block escapes.
    pub fn forbidden_path_argument(&self, command: &str) -> Option<String> {
        self.forbidden_path_argument_impl(command, false)
    }

    /// Like [`SecurityPolicy::forbidden_path_argument`] but for a command that
    /// runs IN the workspace: each workspace-relative path argument is also
    /// resolved (following symlinks, including dangling ones) and re-checked
    /// against the workspace boundary, catching an in-workspace symlink that
    /// points outside for the argument forms this static scan can see.
    ///
    /// This is best-effort, defense-in-depth hardening over a token-scanned
    /// command line - NOT a complete workspace boundary, and NOT equivalent to
    /// the file tools, which resolve an operation-aware target at the call site.
    /// It flags a *path-shaped* argument (one with a separator, e.g. `link/x`, a
    /// redirect target, or an absolute / `..` form) that escapes via an
    /// in-workspace symlink. It does NOT, and cannot from a static parse, cover:
    /// a *bare* argument with no separator (`cat somelink`) that is a symlink; a
    /// path computed at run time via variable expansion or command substitution
    /// (`$VAR`, `$(...)`), `eval`, or a write done inside an executed script
    /// (`sh ./x.sh`, where only the script path is scanned); a quoted path
    /// holding whitespace (`"link dir/out"`), which the whitespace tokenizer
    /// fragments; read-vs-write direction (an argument may be read or written, so
    /// a resolved target allowed for EITHER passes, unlike the operation-aware
    /// file tools); or non-Unix relative forms (a `link\file` path on Windows). A
    /// shell command is Turing-complete; complete containment is the execution
    /// boundary (the OS sandbox and the broader granular sandbox-policy work),
    /// not this preflight.
    pub fn forbidden_workspace_path_argument(&self, command: &str) -> Option<String> {
        self.forbidden_path_argument_impl(command, true)
    }

    fn forbidden_path_argument_impl(
        &self,
        command: &str,
        resolve_workspace: bool,
    ) -> Option<String> {
        let forbidden_candidate = |raw: &str| {
            let candidate = strip_wrapping_quotes(raw).trim();
            if candidate.is_empty() || candidate.contains("://") {
                return None;
            }
            if !looks_like_path(candidate) {
                return None;
            }
            // String-level policy: absolute paths outside the workspace, `..`
            // traversal, `~user` forms, and forbidden prefixes.
            if !self.is_path_allowed(candidate) {
                return Some(candidate.to_string());
            }
            // Workspace-relative args can still escape via an in-workspace
            // symlink: the string check above passes (no `..`, not absolute),
            // yet the real target is elsewhere. File tools already block this by
            // canonicalizing first; mirror that here for path-shaped forms this
            // static scan can see (full containment needs the execution-time
            // sandbox). Resolve the deepest existing ancestor (leaf may be about
            // to be created, e.g. `touch link/new.txt`) and re-check. A command
            // argument may be read OR written, so accept the resolved target if
            // it is allowed for EITHER (mirrors `is_path_allowed`). Fail closed
            // otherwise: outside every allowed root, or unresolvable (cycle /
            // unreadable link) — `None` means "unresolvable", not "allowed".
            if resolve_workspace {
                match self.resolve_command_path_argument(candidate) {
                    Some(resolved)
                        if self.is_resolved_path_allowed(&resolved)
                            || self.is_resolved_path_readable(&resolved) => {}
                    _ => return Some(candidate.to_string()),
                }
            }
            None
        };
        let forbidden_non_redirect_candidate = |raw: &str| {
            let candidate = strip_wrapping_quotes(raw).trim();
            if candidate.is_empty() || candidate.contains("://") {
                return None;
            }
            if candidate.starts_with('-') {
                if let Some((_, value)) = candidate.split_once('=')
                    && let Some(blocked) = forbidden_candidate(value)
                {
                    return Some(blocked);
                }
                if let Some(value) = attached_short_option_value(candidate)
                    && let Some(blocked) = forbidden_candidate(value)
                {
                    return Some(blocked);
                }
                return None;
            }
            forbidden_candidate(candidate)
        };

        for segment in split_unquoted_segments(command) {
            let cmd_part = skip_env_assignments(&segment);
            let mut words = cmd_part.split_whitespace();
            let Some(executable) = words.next() else {
                continue;
            };

            let executable_redirect = parse_redirection_argument(strip_wrapping_quotes(executable));
            let mut next_is_redirect_target = false;
            // Cover inline forms like `cat</etc/passwd`.
            match executable_redirect {
                RedirectionArgument::Target { target, .. } => {
                    if !is_safe_device_redirect_target(target)
                        && let Some(blocked) = forbidden_candidate(target)
                    {
                        return Some(blocked);
                    }
                }
                RedirectionArgument::NeedsNextToken { .. } => {
                    next_is_redirect_target = true;
                }
                RedirectionArgument::FdOnly { .. } | RedirectionArgument::None => {}
            }

            for token in words {
                let candidate = strip_wrapping_quotes(token).trim();
                if candidate.is_empty() {
                    continue;
                }

                if next_is_redirect_target {
                    next_is_redirect_target = false;
                    if is_safe_device_redirect_target(candidate) {
                        continue;
                    }
                    if let Some(blocked) = forbidden_candidate(candidate) {
                        return Some(blocked);
                    }
                    continue;
                }

                if candidate.contains("://") {
                    continue;
                }

                match parse_redirection_argument(candidate) {
                    RedirectionArgument::Target { prefix, target } => {
                        if let Some(blocked) = forbidden_non_redirect_candidate(prefix) {
                            return Some(blocked);
                        }
                        if is_safe_device_redirect_target(target) {
                            continue;
                        }
                        if let Some(blocked) = forbidden_candidate(target) {
                            return Some(blocked);
                        }
                    }
                    RedirectionArgument::NeedsNextToken { prefix } => {
                        if let Some(blocked) = forbidden_non_redirect_candidate(prefix) {
                            return Some(blocked);
                        }
                        next_is_redirect_target = true;
                        continue;
                    }
                    RedirectionArgument::FdOnly { prefix } => {
                        if let Some(blocked) = forbidden_non_redirect_candidate(prefix) {
                            return Some(blocked);
                        }
                        continue;
                    }
                    RedirectionArgument::None => {}
                }

                // Handle option assignment forms like `--file=/etc/passwd`.
                if let Some(blocked) = forbidden_non_redirect_candidate(candidate) {
                    return Some(blocked);
                }
                if candidate.starts_with('-') {
                    continue;
                }
            }
        }

        None
    }

    /// Resolve a shell command path argument to the canonical target used for
    /// workspace-boundary checks. Relative arguments are taken relative to the
    /// workspace directory; `~` is expanded. Because a command may be about to
    /// CREATE the leaf (e.g. `touch dir/new.txt`), symlinks are resolved on the
    /// deepest existing ancestor and any non-existent trailing components are
    /// re-appended, so an in-workspace symlink pointing outside is followed to
    /// its real target. Relative arguments are joined onto the workspace
    /// directory BEFORE resolving. Returns `None` when no trustworthy target
    /// exists: a null byte in the input, or an unresolvable path (a symlink
    /// cycle exhausting the resolver's hop budget). Callers MUST treat `None`
    /// as a block (fail closed), never as "nothing to re-check" - see
    /// `forbidden_path_argument_impl`.
    fn resolve_command_path_argument(&self, candidate: &str) -> Option<PathBuf> {
        if candidate.contains('\0') {
            return None;
        }
        let expanded = expand_user_path(candidate);
        let joined = if expanded.is_absolute() {
            expanded
        } else {
            self.workspace_dir.join(expanded)
        };
        resolve_symlinked_path(&joined)
    }

    /// Check if a file path is allowed (no path traversal, within workspace)
    pub fn is_path_allowed(&self, path: &str) -> bool {
        // Block null bytes (can truncate paths in C-backed syscalls)
        if path.contains('\0') {
            return false;
        }

        // Block path traversal: check for ".." as a path component
        if Path::new(path)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return false;
        }

        // Block URL-encoded traversal attempts (e.g. ..%2f)
        let lower = path.to_lowercase();
        if lower.contains("..%2f") || lower.contains("%2f..") {
            return false;
        }

        // Reject "~user" forms because the shell expands them at runtime and
        // they can escape workspace policy.
        if path.starts_with('~') && path != "~" && !path.starts_with("~/") {
            return false;
        }

        // Expand "~" for consistent matching with forbidden paths and allowlists.
        let expanded_path = expand_user_path(path);

        // The null device is always permitted regardless of workspace or
        // forbidden-path config; the rest of /dev remains blocked as usual.
        if is_null_device(&expanded_path) {
            return true;
        }

        if expanded_path.is_absolute() {
            let in_workspace = expanded_path.starts_with(&self.workspace_dir);
            let in_allowed_root = self
                .allowed_roots
                .iter()
                .any(|root| expanded_path.starts_with(root));
            let in_read_only_root = self
                .allowed_roots_read_only
                .iter()
                .any(|root| expanded_path.starts_with(root));
            let in_write_only_root = self
                .allowed_roots_write_only
                .iter()
                .any(|root| expanded_path.starts_with(root));

            if in_workspace || in_allowed_root || in_read_only_root || in_write_only_root {
                return true;
            }

            // Absolute path outside workspace/allowed roots — block when
            // workspace_only, or fall through to forbidden-prefix check.
            if self.workspace_only {
                return false;
            }
        }

        // Block forbidden paths using path-component-aware matching
        for forbidden in &self.forbidden_paths {
            let forbidden_path = expand_user_path(forbidden);
            if expanded_path.starts_with(forbidden_path) {
                return false;
            }
        }

        true
    }

    pub fn is_resolved_path_readable(&self, resolved: &Path) -> bool {
        // Universal POSIX device files: any operator running on Linux,
        // macOS, or BSD expects these to be readable. Adding them to
        // the per-agent config would be friction without security
        // benefit (they have no agent-relevant content).
        const POSIX_DEVICE_READS: &[&str] =
            &["/dev/null", "/dev/zero", "/dev/random", "/dev/urandom"];
        for device in POSIX_DEVICE_READS {
            if resolved == Path::new(device) {
                return true;
            }
        }

        // Workspace + read-write allowlist + read-only allowlist.
        // Inlined rather than delegating to `is_resolved_path_allowed`
        // so the write-only allowlist is intentionally NOT in scope
        // here.
        let workspace_root = self
            .workspace_dir
            .canonicalize()
            .unwrap_or_else(|_| self.workspace_dir.clone());
        if resolved.starts_with(&workspace_root) {
            return true;
        }
        for root in &self.allowed_roots {
            let canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
            if resolved.starts_with(&canonical) {
                return true;
            }
        }
        for root in &self.allowed_roots_read_only {
            let canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
            if resolved.starts_with(&canonical) {
                return true;
            }
        }
        for root in &self.allowed_roots_write_only {
            let canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
            if resolved.starts_with(&canonical) {
                return false;
            }
        }

        // Forbidden paths gate after the explicit allowlists so the
        // allowlists can coexist with broad default forbidden roots
        // such as `/home` and `/tmp`.
        for forbidden in &self.forbidden_paths {
            let forbidden_path = expand_user_path(forbidden);
            if resolved.starts_with(&forbidden_path) {
                return false;
            }
        }
        if !self.workspace_only {
            return true;
        }
        false
    }

    pub fn is_resolved_path_allowed(&self, resolved: &Path) -> bool {
        if is_null_device(resolved) {
            return true;
        }

        // Prefer canonical workspace root so `/a/../b` style config paths don't
        // cause false positives or negatives.
        let workspace_root = self
            .workspace_dir
            .canonicalize()
            .unwrap_or_else(|_| self.workspace_dir.clone());
        if resolved.starts_with(&workspace_root) {
            return true;
        }

        // Check extra allowed roots (e.g. shared skills directories) before
        // forbidden checks so explicit allowlists can coexist with broad
        // default forbidden roots such as `/home` and `/tmp`.
        for root in &self.allowed_roots {
            let canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
            if resolved.starts_with(&canonical) {
                return true;
            }
        }

        // Write-only cross-agent grants land here. The bot can write
        // under these paths but `is_resolved_path_readable` does not
        // see them — `AccessMode::Write` is one-way by design.
        for root in &self.allowed_roots_write_only {
            let canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
            if resolved.starts_with(&canonical) {
                return true;
            }
        }

        // For paths outside workspace/allowlist, block forbidden roots to
        // prevent symlink escapes and sensitive directory access.
        for forbidden in &self.forbidden_paths {
            let forbidden_path = expand_user_path(forbidden);
            if resolved.starts_with(&forbidden_path) {
                return false;
            }
        }

        // When workspace_only is disabled the user explicitly opted out of
        // workspace confinement after forbidden-path checks are applied.
        if !self.workspace_only {
            return true;
        }

        false
    }

    fn runtime_config_dirs(&self) -> Vec<PathBuf> {
        let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Some(parent) = self.config_path.as_deref().and_then(Path::parent) {
            dirs.push(canon(parent));
        }
        if let Some(parent) = self.workspace_dir.parent() {
            let dir = canon(parent);
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        if let Some(data_dir) = self.data_dir.as_deref() {
            let dir = canon(data_dir);
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        dirs
    }

    pub fn is_runtime_config_path(&self, resolved: &Path) -> bool {
        let Some(file_name) = resolved.file_name().and_then(|value| value.to_str()) else {
            return false;
        };
        let is_protected_name = file_name == "config.toml"
            || file_name == "config.toml.bak"
            || file_name.starts_with(".config.toml.tmp-")
            || file_name == "estop-state.json"
            || file_name == "otp-secret"
            || file_name == "webauthn_credentials.json";
        if !is_protected_name {
            return false;
        }
        let Some(parent) = resolved.parent() else {
            return false;
        };
        self.runtime_config_dirs()
            .iter()
            .any(|dir| parent == dir.as_path())
    }

    pub fn runtime_config_violation_message(&self, resolved: &Path) -> String {
        format!(
            "Refusing to modify ZeroClaw runtime config/state file: {}. Use dedicated config tools or edit it manually outside the agent loop.",
            resolved.display()
        )
    }

    pub fn resolved_path_violation_message(&self, resolved: &Path) -> String {
        let guidance = if self.allowed_roots.is_empty() {
            "Add the directory to [autonomy].allowed_roots (for example: allowed_roots = [\"/absolute/path\"]), or move the file into the workspace."
        } else {
            "Add a matching parent directory to [autonomy].allowed_roots, or move the file into the workspace."
        };

        format!(
            "Resolved path escapes workspace allowlist: {}. {}",
            resolved.display(),
            guidance
        )
    }

    /// Check if autonomy level permits any action at all
    pub fn can_act(&self) -> bool {
        self.autonomy != AutonomyLevel::ReadOnly
    }

    // ── Tool Operation Gating ──────────────────────────────────────────────
    // Read operations bypass autonomy and rate checks because they have
    // no side effects. Act operations must pass both the autonomy gate
    // (not read-only) and the sliding-window rate limiter.

    /// Enforce policy for a tool operation.
    /// Read operations are always allowed by autonomy/rate gates.
    /// Act operations require non-readonly autonomy and available action budget.
    pub fn enforce_tool_operation(
        &self,
        operation: ToolOperation,
        operation_name: &str,
    ) -> Result<(), String> {
        match operation {
            ToolOperation::Read => Ok(()),
            ToolOperation::Act => {
                if !self.can_act() {
                    return Err(format!(
                        "Security policy: read-only mode, cannot perform '{operation_name}'"
                    ));
                }

                if !self.record_action() {
                    return Err("Rate limit exceeded: action budget exhausted".to_string());
                }

                Ok(())
            }
        }
    }

    /// Record an action for the current sender and check if rate-limited.
    /// Returns `true` if allowed, `false` if budget exhausted.
    pub fn record_action(&self) -> bool {
        self.tracker.record_for_current(self.max_actions_per_hour)
    }

    /// Check if the current sender would be rate-limited without recording.
    pub fn is_rate_limited(&self) -> bool {
        self.tracker
            .is_limited_for_current(self.max_actions_per_hour)
    }

    pub fn resolve_tool_path(&self, path: &str) -> PathBuf {
        let expanded = expand_user_path(path);
        if expanded.is_absolute() {
            expanded
        } else if let Some(workspace_hint) = rootless_path(&self.workspace_dir) {
            if let Ok(stripped) = expanded.strip_prefix(&workspace_hint) {
                if stripped.as_os_str().is_empty() {
                    self.workspace_dir.clone()
                } else {
                    self.workspace_dir.join(stripped)
                }
            } else if let Some(stripped) =
                workspace_prefixed_relative_suffix(&expanded, &self.workspace_dir)
            {
                if stripped.as_os_str().is_empty() {
                    self.workspace_dir.clone()
                } else {
                    self.workspace_dir.join(stripped)
                }
            } else {
                self.workspace_dir.join(expanded)
            }
        } else {
            self.workspace_dir.join(expanded)
        }
    }

    pub fn is_under_allowed_root(&self, path: &str) -> bool {
        let expanded = expand_user_path(path);
        if !expanded.is_absolute() {
            return false;
        }
        roots_contain(&self.allowed_roots, &expanded)
            || roots_contain(&self.allowed_roots_write_only, &expanded)
    }

    #[must_use]
    pub fn is_under_read_only_allowed_root(&self, path: &str) -> bool {
        let expanded = expand_user_path(path);
        if !expanded.is_absolute() {
            return false;
        }
        roots_contain(&self.allowed_roots_read_only, &expanded)
    }

    /// Union of all three root tiers; directionality is enforced later
    /// by the resolved-path checks.
    #[must_use]
    pub fn is_under_any_allowed_root(&self, path: &str) -> bool {
        self.is_under_allowed_root(path) || self.is_under_read_only_allowed_root(path)
    }

    pub fn ensure_no_escalation_beyond(
        &self,
        parent: &SecurityPolicy,
    ) -> Result<(), EscalationViolation> {
        // Autonomy: child must not exceed parent. ReadOnly < Supervised
        // < Full per the AutonomyLevel ordering.
        if self.autonomy > parent.autonomy {
            return Err(EscalationViolation::AutonomyAboveParent {
                child: self.autonomy,
                parent: parent.autonomy,
            });
        }

        for root in &self.allowed_roots {
            if !parent.allowed_roots.iter().any(|p| path_contains(p, root)) {
                return Err(EscalationViolation::ReadWriteRootNotInParent { path: root.clone() });
            }
        }
        for root in &self.allowed_roots_read_only {
            let in_parent_rw = parent.allowed_roots.iter().any(|p| path_contains(p, root));
            let in_parent_ro = parent
                .allowed_roots_read_only
                .iter()
                .any(|p| path_contains(p, root));
            if !in_parent_rw && !in_parent_ro {
                return Err(EscalationViolation::ReadOnlyRootNotInParent { path: root.clone() });
            }
        }
        for root in &self.allowed_roots_write_only {
            let in_parent_rw = parent.allowed_roots.iter().any(|p| path_contains(p, root));
            let in_parent_wo = parent
                .allowed_roots_write_only
                .iter()
                .any(|p| path_contains(p, root));
            if !in_parent_rw && !in_parent_wo {
                return Err(EscalationViolation::WriteOnlyRootNotInParent { path: root.clone() });
            }
        }
        for cmd in &self.allowed_commands {
            if !parent
                .allowed_commands
                .iter()
                .any(|p| command_allowlist_entries_equivalent(p, cmd))
            {
                return Err(EscalationViolation::CommandNotInParent {
                    command: cmd.clone(),
                });
            }
        }
        if parent.workspace_only && !self.workspace_only {
            return Err(EscalationViolation::WorkspaceOnlyDisabledByChild);
        }

        // Forbidden paths run the OPPOSITE direction from allowlists:
        // the parent's forbidden set must be a subset of the child's,
        // i.e. the child cannot drop a parent's forbidden entry.
        for parent_forbidden in &parent.forbidden_paths {
            if !self.forbidden_paths.iter().any(|c| c == parent_forbidden) {
                return Err(EscalationViolation::ForbiddenPathDroppedByChild {
                    path: parent_forbidden.clone(),
                });
            }
        }

        // shell_env_passthrough is a leak surface: every child entry
        // must already be on the parent's list.
        for var in &self.shell_env_passthrough {
            if !parent.shell_env_passthrough.iter().any(|p| p == var) {
                return Err(EscalationViolation::ShellEnvPassthroughExpanded {
                    variable: var.clone(),
                });
            }
        }

        if self.max_actions_per_hour > parent.max_actions_per_hour {
            return Err(EscalationViolation::MaxActionsExceeded {
                child: self.max_actions_per_hour,
                parent: parent.max_actions_per_hour,
            });
        }
        if self.max_cost_per_day_cents > parent.max_cost_per_day_cents {
            return Err(EscalationViolation::MaxCostExceeded {
                child: self.max_cost_per_day_cents,
                parent: parent.max_cost_per_day_cents,
            });
        }
        if self.shell_timeout_secs > parent.shell_timeout_secs {
            return Err(EscalationViolation::ShellTimeoutExceeded {
                child: self.shell_timeout_secs,
                parent: parent.shell_timeout_secs,
            });
        }
        if parent.block_high_risk_commands && !self.block_high_risk_commands {
            return Err(EscalationViolation::BlockHighRiskCommandsDisabledByChild);
        }
        if parent.require_approval_for_medium_risk && !self.require_approval_for_medium_risk {
            return Err(EscalationViolation::RequireApprovalDisabledByChild);
        }

        Ok(())
    }

    pub fn from_risk_profile(
        risk_profile: &crate::schema::RiskProfileConfig,
        workspace_dir: &Path,
    ) -> Self {
        Self::from_profiles(risk_profile, None, workspace_dir)
    }

    pub fn from_profiles(
        risk_profile: &crate::schema::RiskProfileConfig,
        runtime_profile: Option<&crate::schema::RuntimeProfileConfig>,
        workspace_dir: &Path,
    ) -> Self {
        // When autonomy is Full, disable workspace_only so the agent can
        // access paths outside the workspace. Forbidden-path checks still
        // apply, preventing access to sensitive system directories.
        let effective_workspace_only = if risk_profile.level == AutonomyLevel::Full {
            false
        } else {
            risk_profile.workspace_only
        };

        let runtime_default = crate::schema::RuntimeProfileConfig::default();
        let runtime = runtime_profile.unwrap_or(&runtime_default);

        Self {
            autonomy: risk_profile.level,
            risk_profile_name: String::new(),
            workspace_dir: workspace_dir.to_path_buf(),
            // Set by `for_agent` once the install root is known; the
            // profile-only constructor has no config path.
            config_path: None,
            // Set by `for_agent` once the data dir is known; the
            // profile-only constructor has no data dir.
            data_dir: None,
            workspace_only: effective_workspace_only,
            allowed_commands: risk_profile.allowed_commands.clone(),
            forbidden_paths: risk_profile.forbidden_paths.clone(),
            allowed_roots: risk_profile
                .allowed_roots
                .iter()
                .filter(|root| {
                    let t = root.trim();
                    !t.is_empty() && t != crate::traits::UNSET_DISPLAY && t != "*"
                })
                .map(|root| {
                    let expanded = expand_user_path(root);
                    if expanded.is_absolute() {
                        expanded
                    } else {
                        workspace_dir.join(expanded)
                    }
                })
                .collect(),
            allowed_roots_read_only: Vec::new(),
            allowed_roots_write_only: Vec::new(),
            max_actions_per_hour: runtime.max_actions_per_hour,
            max_cost_per_day_cents: runtime.max_cost_per_day_cents,
            require_approval_for_medium_risk: risk_profile.require_approval_for_medium_risk,
            block_high_risk_commands: risk_profile.block_high_risk_commands,
            shell_env_passthrough: risk_profile.shell_env_passthrough.clone(),
            shell_timeout_secs: runtime.shell_timeout_secs,
            allowed_tools: risk_profile.allowed_tools.clone(),
            excluded_tools: if risk_profile.excluded_tools.is_empty() {
                None
            } else {
                Some(risk_profile.excluded_tools.clone())
            },
            mcp_discovered_tool_policy: risk_profile.mcp_discovered_tool_policy,
            auto_approve: risk_profile.auto_approve.clone(),
            always_ask: risk_profile.always_ask.clone(),
            sandbox_enabled: risk_profile.sandbox_enabled,
            sandbox_backend: risk_profile.sandbox_backend.clone(),
            firejail_args: risk_profile.firejail_args.clone(),
            tracker: PerSenderTracker::new(),
        }
    }

    pub fn for_agent(config: &crate::schema::Config, agent_alias: &str) -> anyhow::Result<Self> {
        let risk_profile = config.risk_profile_for_agent(agent_alias).ok_or_else(|| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"agent_alias": agent_alias})),
                "SecurityPolicy::for_agent: agent has no resolvable risk_profile"
            );
            anyhow::Error::msg(format!(
                "agents.{agent_alias} has no resolvable risk_profile: neither agents.{agent_alias}.risk_profile nor (via its card, if set) cards.<card>.risk_profile names a configured [risk_profiles.<alias>] entry. \
                 Config::validate() rejects this at load time, but load_or_init only warns and boots anyway on a failed validation — fix the config or re-check why validation did not run"
            ))
        })?;
        let runtime_profile = config.runtime_profile_for_agent(agent_alias);
        // Per-agent workspace becomes the SecurityPolicy boundary so
        // file_read/write/edit and the shell tool jail to the agent's
        // own dir, not the install-wide legacy path.
        let agent_workspace = config.agent_workspace_dir(agent_alias);
        // The per-agent workspace is the shell tool's spawn cwd and the file-tool
        // jail root. Create it here so every path that builds a per-agent policy
        // (agent loop, gateway, channels) has the directory present. A missing cwd
        // makes the shell tool's process spawn fail with ENOENT on a fresh agent.
        std::fs::create_dir_all(&agent_workspace).with_context(|| {
            format!(
                "SecurityPolicy::for_agent: failed to create agent workspace dir {}",
                agent_workspace.display()
            )
        })?;
        let mut policy = Self::from_profiles(risk_profile, runtime_profile, &agent_workspace);
        if let Some(agent_cfg) = config.agents.get(agent_alias) {
            let direct_profile = agent_cfg.risk_profile.trim();
            if !direct_profile.is_empty() {
                policy.risk_profile_name = direct_profile.to_string();
            }
            // Carded agent: `agent_cfg.risk_profile` is empty by
            // construction (validation forbids setting both).
            // `risk_profile_for_agent` already followed the card to resolve
            // the `RiskProfileConfig` above (autonomy, sandbox, shell
            // allow-list, `always_ask` — "the profile owns the rest," per
            // `AgentCard::risk_profile`'s doc). What it does NOT do is touch
            // tools: that accessor is also read directly elsewhere for a
            // profile's OWN `allowed_tools`/`excluded_tools` fields (the
            // retired `spawn_subagent` tool's self-permission check was one
            // such reader), so folding the
            // card's grants into it there would let a card-granted tool be
            // silently gated by an unrelated profile's tool list, or vice
            // versa. Tool grants are resolved only here, once, as a
            // deliberate override.
            if let Some(card) = config.card_for_agent(agent_alias) {
                let card_profile = card.risk_profile.as_str().trim();
                if !card_profile.is_empty() {
                    policy.risk_profile_name = card_profile.to_string();
                }
                // `to_allowed_tools()` is always `Some`, never `None` —
                // an empty grant list means deny-everything, not
                // unrestricted. Preserve that: this is a full replacement of
                // the profile's `allowed_tools` only — the profile's own
                // `excluded_tools` still applies on top (deny wins; see
                // `is_tool_allowed`), so a profile exclusion can veto a card
                // grant. That subtraction is deliberate: the named profile is
                // the card author's own choice (`AgentCard::risk_profile`),
                // so its exclusions are part of the authored posture.
                policy.allowed_tools = card.grants.to_allowed_tools();
                // `CardGrants`'s own doc says naming is the only way to
                // grant — there is no "all". `McpDiscoveredToolPolicy::AutoAdmit`
                // is documented as a profile-level "escape hatch for setups
                // that rely on it"; a card cannot mean "auto-admit" because
                // naming is its entire semantics, so when a card governs,
                // that escape hatch closes regardless of what the profile it
                // points at says. Left at the profile's own setting, an
                // MCP-permissive profile would let `admits_unlisted` (and
                // `tool_search.rs`'s own admission check) re-admit any
                // `<server>__<tool>`-shaped name the card never granted —
                // every consumer of `for_agent` (independent-mode delegation,
                // top-level carded agents) inherits that hole otherwise.
                policy.mcp_discovered_tool_policy =
                    crate::autonomy::McpDiscoveredToolPolicy::ExplicitOnly;
            }
        }
        policy.config_path = Some(config.config_path.clone());
        // Runtime data dir: same predicate extends there so state files
        // that the gateway constructs with `&config.data_dir`
        // (`webauthn_credentials.json` for WebAuthnManager) are protected
        // from agent overwrites when `data_dir` overlaps an allowed root.
        policy.data_dir = Some(config.data_dir.clone());

        policy
            .allowed_roots_read_only
            .push(config.shared_workspace_dir().join("skills"));

        if let Some(agent_cfg) = config.agents.get(agent_alias) {
            for (sibling_alias, mode) in &agent_cfg.workspace.access {
                let sibling_dir = config.agent_workspace_dir(sibling_alias.as_str());
                match mode {
                    crate::multi_agent::AccessMode::Read => {
                        policy.allowed_roots_read_only.push(sibling_dir);
                    }
                    crate::multi_agent::AccessMode::Write => {
                        policy.allowed_roots_write_only.push(sibling_dir);
                    }
                    crate::multi_agent::AccessMode::ReadWrite => {
                        policy.allowed_roots.push(sibling_dir);
                    }
                }
            }

            // The escape-hatch flag retains its all-paths semantics —
            // agents that genuinely need to read or write outside any
            // per-agent scope opt in here. Defaults to false.
            if agent_cfg.workspace.unrestricted_filesystem {
                policy.workspace_only = false;
            }
        }

        Ok(policy)
    }

    pub fn prompt_summary(&self) -> String {
        use std::fmt::Write;

        let mut out = String::new();

        // Autonomy level
        let _ = writeln!(out, "**Autonomy level**: {:?}", self.autonomy);

        // Workspace constraint
        if self.workspace_only {
            let _ = writeln!(
                out,
                "**Workspace boundary**: file operations are restricted to `{}`.",
                self.workspace_dir.display()
            );
        }

        // Allowed roots
        if !self.allowed_roots.is_empty() {
            let roots: Vec<String> = self
                .allowed_roots
                .iter()
                .map(|p| format!("`{}`", p.display()))
                .collect();
            let _ = writeln!(out, "**Additional allowed paths**: {}", roots.join(", "));
        }

        // Allowed commands
        if !self.allowed_commands.is_empty() {
            let cmds: Vec<String> = self
                .allowed_commands
                .iter()
                .map(|c| format!("`{c}`"))
                .collect();
            let _ = writeln!(
                out,
                "**Allowed shell commands**: {}. \
                 You may execute these commands freely.",
                cmds.join(", ")
            );
        }

        // Forbidden paths
        if !self.forbidden_paths.is_empty() {
            let paths: Vec<String> = self
                .forbidden_paths
                .iter()
                .map(|p| format!("`{p}`"))
                .collect();
            let _ = writeln!(
                out,
                "**Forbidden paths**: {}. \
                 Avoid accessing these paths.",
                paths.join(", ")
            );
        }

        // Risk controls
        if self.block_high_risk_commands {
            let _ = writeln!(
                out,
                "Exercise caution with destructive commands (rm, kill, reboot, etc.)."
            );
        }
        if self.require_approval_for_medium_risk {
            let _ = writeln!(
                out,
                "**Medium-risk commands** require user approval before execution."
            );
        }

        // Rate limit
        let _ = writeln!(
            out,
            "**Rate limit**: max {} actions per hour per chat (each conversation has its own independent budget).",
            self.max_actions_per_hour
        );

        out
    }
}

#[cfg(test)]
mod tests;

//! Canonical command and path defaults shared by config profiles and runtime policy.

/// Default allowed commands for Unix platforms.
#[cfg(not(target_os = "windows"))]
pub fn default_allowed_commands() -> Vec<String> {
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
pub fn default_allowed_commands() -> Vec<String> {
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
pub fn default_forbidden_paths() -> Vec<String> {
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
pub fn default_forbidden_paths() -> Vec<String> {
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

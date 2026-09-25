//! Tests to verify .dockerignore excludes sensitive paths from Docker build context.

use std::path::Path;

/// Paths that MUST be excluded from Docker build context (security/performance)
const MUST_EXCLUDE: &[&str] = &[
    ".git",
    ".githooks",
    "target",
    "docs",
    "examples",
    "*.md",
    "*.png",
    "*.db",
    "*.db-journal",
    ".DS_Store",
    ".github",
    "deny.toml",
    "LICENSE",
    ".env",
    ".tmp_*",
    ".git/config",
    "target/release/zeroclaw",
    ".github/workflows/ci.yml",
];

/// Paths that MUST NOT be excluded (required for build)
const MUST_INCLUDE: &[&str] = &["Cargo.toml", "Cargo.lock", "src/"];

/// Parse .dockerignore and return all non-comment, non-empty lines
fn parse_dockerignore(content: &str) -> Vec<String> {
    content
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.to_string())
        .collect()
}

/// Check if a pattern would match a given path
fn pattern_matches(pattern: &str, path: &str) -> bool {
    // Handle negation patterns
    if pattern.starts_with('!') {
        return false; // Negation re-includes, so it doesn't "exclude"
    }

    // Handle glob patterns
    if pattern.starts_with("*.") {
        let ext = &pattern[1..]; // e.g., ".md"
        return path.ends_with(ext);
    }

    // Handle directory patterns (with or without trailing slash)
    let pattern_normalized = pattern.trim_end_matches('/');
    let path_normalized = path.trim_end_matches('/');

    // Exact match
    if path_normalized == pattern_normalized {
        return true;
    }

    // Pattern is a prefix (directory match)
    if path_normalized.starts_with(&format!("{}/", pattern_normalized)) {
        return true;
    }

    // Wildcard prefix patterns like ".tmp_*"
    if pattern.contains('*') && !pattern.starts_with("*.") {
        let prefix = pattern.split('*').next().unwrap_or("");
        if !prefix.is_empty() && path.starts_with(prefix) {
            return true;
        }
    }

    false
}

/// Check if any pattern in the list would exclude the given path
fn is_excluded(patterns: &[String], path: &str) -> bool {
    let mut excluded = false;
    for pattern in patterns {
        if let Some(negated) = pattern.strip_prefix('!') {
            // Negation pattern - re-include
            if pattern_matches(negated, path) {
                excluded = false;
            }
        } else if pattern_matches(pattern, path) {
            excluded = true;
        }
    }
    excluded
}

#[tokio::test]
async fn dockerignore_excludes_security_critical_paths() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".dockerignore");
    let content = tokio::fs::read_to_string(&path)
        .await
        .expect("Failed to read .dockerignore");
    let patterns = parse_dockerignore(&content);

    for must_exclude in MUST_EXCLUDE {
        // For glob patterns, test with a sample file
        let test_path = if must_exclude.starts_with("*.") {
            format!("sample{}", &must_exclude[1..])
        } else {
            must_exclude.to_string()
        };

        assert!(
            is_excluded(&patterns, &test_path),
            "Path '{}' (tested as '{}') MUST be excluded by .dockerignore but is not. \
             This is a security/performance issue.",
            must_exclude,
            test_path
        );
    }
}

#[tokio::test]
async fn dockerignore_does_not_exclude_build_essentials() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".dockerignore");
    let content = tokio::fs::read_to_string(&path)
        .await
        .expect("Failed to read .dockerignore");
    let patterns = parse_dockerignore(&content);

    for must_include in MUST_INCLUDE {
        assert!(
            !is_excluded(&patterns, must_include),
            "Path '{}' MUST NOT be excluded by .dockerignore (required for build)",
            must_include
        );
    }
}

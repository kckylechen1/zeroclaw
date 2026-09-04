//! Wall 4 no-regrowth guard (migration epic 197; TB-22 of the frozen bridge
//! contract, revision 3).
//!
//! The durable control plane was deleted in Wall 4: its last writer died with
//! the spawn wall, and durable task/attempt truth is Tachi's through the
//! bridge (E-annex rows 1 and 6). This test pins the absence structurally: no
//! ZeroClaw runtime source may reference the retired store type, its DB, or
//! the retired coordinator crate, so a second task ledger cannot quietly
//! regrow under the old names. New durable stores are also caught
//! independently by the TB-22 persistence-surface manifest gate
//! (`scripts/ci/persistence_surface_gate.sh`), which any replacement store
//! must be added to — this scan is the belt to that gate's braces.

use std::path::Path;

/// Scan every runtime source file for the retired durable task-store tokens.
#[test]
fn runtime_source_references_no_retired_durable_task_store() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    scan(&src, &mut offenders);

    assert!(
        offenders.is_empty(),
        "retired durable task-store tokens found; durable task truth is \
         Tachi's through the bridge (frozen contract annex rows 1 and 6) — \
         do not regrow a second ledger:\n{}",
        offenders.join("\n")
    );
}

fn scan(dir: &Path, offenders: &mut Vec<String>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.is_dir() {
            scan(&path, offenders);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let content = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            // Comments are stripped before matching: prose that names the
            // retired store documents its absence and regrows nothing, while
            // a live `use`, type, constructor, or DDL string — real code —
            // still trips the guard.
            let code = strip_line_comments(&content);
            for token in BANNED_TOKENS {
                if code.contains(token) && !is_exempted(&path, token) {
                    offenders.push(format!(
                        "{}: contains {token:?}",
                        path.strip_prefix(env!("CARGO_MANIFEST_DIR"))
                            .expect("in-crate path")
                            .display()
                    ));
                }
            }
        }
    }
}

/// Remove `//` line comments (including `///`/`//!` doc forms). Deliberately
/// naive: it can over-strip a `//` inside a string literal, which for these
/// tokens errs toward silence on prose-shaped strings and never misses a
/// `use`/type/constructor reference — the references that would constitute
/// regrowth. Block comments are not used by the retired vocabulary's former
/// call sites; if one appears with these tokens, the persistence gate's
/// DDL scan still covers it.
fn strip_line_comments(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    for line in source.lines() {
        match line.find("//") {
            Some(idx) => out.push_str(&line[..idx]),
            None => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

/// The one sanctioned mention: the daemon's boot-absence test names the file
/// it asserts is never created (`daemon::tests::daemon_boot_creates_no_control_plane_db`).
/// Only the bare file name is exempted there — any store vocabulary token in
/// that file, or the file name anywhere else, still trips.
fn is_exempted(path: &Path, token: &str) -> bool {
    token == "control_plane.db"
        && path
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .expect("in-crate path")
            == Path::new("src/daemon/mod.rs")
}

/// The retired control-plane vocabulary. Any of these strings in runtime
/// source means a piece of the deleted duplicate ledger came back.
const BANNED_TOKENS: &[&str] = &[
    "control_plane.db",
    "SqliteTaskStore",
    "SubagentPersistence",
    "ControlPlaneHandle",
    "CoordinatorHost",
    "zeroclaw_coordinator",
    "GoalTaskRegistry",
    "TaskContinuationContext",
];

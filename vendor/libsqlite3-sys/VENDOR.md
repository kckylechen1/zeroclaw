# Vendored libsqlite3-sys 0.35.0 + SQLite 3.51.1 (temporary)

Why: memcore (git dep of `zeroclaw-memory`, feature `tachi`) carries a
compile-time security floor `SQLITE_VERSION_NUMBER >= 3.50.3` (CVE-2025-7709,
corrupt FTS5 index -> unauthorized memory access). Meeting it normally means
rusqlite 0.38 / libsqlite3-sys 0.36 — but `matrix-sdk-sqlite 0.18` (latest)
pins rusqlite `^0.37`, and libsqlite3-sys is a `links = "sqlite3"` crate, so
only one version may exist in the graph. Structural deadlock.

Shape: verbatim libsqlite3-sys 0.35.0 from crates.io minus its README (its
relative links assume the rusqlite repo layout and break the docs link gate
here), with exactly four files changed, all taken from libsqlite3-sys 0.36.0 (also crates.io — same trust
domain, no third-party sources):
  - sqlite3/sqlite3.c / sqlite3.h / sqlite3ext.h  (SQLite 3.50.2 -> 3.51.1)
  - sqlite3/bindgen_bundled_version.rs            (only the three version
    constants: SQLITE_VERSION, SQLITE_VERSION_NUMBER, SQLITE_SOURCE_ID)

SQLite's C API is append-only across minor/patch releases, so 0.35-shape
bindings remain valid against the 3.51.1 amalgamation; the constants are
aligned so `rusqlite::ffi::SQLITE_VERSION_NUMBER` tells the truth about the
compiled library.

Retire when either lands, then delete this dir and the [patch.crates-io]
entry in the root Cargo.toml:
  - matrix-sdk-sqlite releases with rusqlite >= 0.38, or
  - upstream zeroclaw-labs/zeroclaw moves the workspace to rusqlite 0.38+.

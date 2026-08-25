#!/usr/bin/env bash

# Toolchain pin guard: fails before any Rust-dependent work when the active
# cargo/rustc do not match the repository pin. The expected version is derived
# from the root rust-toolchain.toml (falling back to a legacy `rust-toolchain`
# file) — there is no second version constant in this script.
#
# Classifications:
#   exit 0 — pinned toolchain active (paths and versions echoed)
#   exit 1 — missing cargo/rustc on PATH, or a version mismatch
#   exit 2 — fatal: the pin itself is missing, unreadable, or not a concrete
#            version channel (stable/beta/nightly cannot be checked statically)
#
# The guard never installs, switches, or mutates toolchains.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"

die_fatal() {
    echo "FATAL: $*" >&2
    exit 2
}

# ── Resolve the pin ─────────────────────────────────────────────────────

pin_file="${repo_root}/rust-toolchain.toml"
if [ ! -f "$pin_file" ]; then
    pin_file="${repo_root}/rust-toolchain"
fi
if [ ! -f "$pin_file" ]; then
    die_fatal "no rust-toolchain.toml at the repository root; the pin the guard must enforce is missing."
fi

pin="$(sed -n 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$pin_file" | head -n 1)"
if [ -z "$pin" ]; then
    die_fatal "no channel entry in ${pin_file}; cannot derive the expected Rust version."
fi
case "$pin" in
    *[!0-9.]*)
        die_fatal "channel \"${pin}\" in ${pin_file} is not a concrete version; the guard only enforces numeric pins (e.g. 1.96.1)."
        ;;
esac

# ── Resolve the active toolchain ────────────────────────────────────────

cargo_path="$(command -v cargo 2>/dev/null || true)"
rustc_path="$(command -v rustc 2>/dev/null || true)"

if [ -z "$cargo_path" ] && [ -z "$rustc_path" ]; then
    echo "FAIL: Rust toolchain missing — neither cargo nor rustc is on PATH." >&2
    echo "  required by repository pin (${pin_file}): ${pin}" >&2
    echo "Remediation: install Rust with rustup (https://rustup.rs), then run" >&2
    echo "inside this repository so ${pin_file} selects the pinned toolchain." >&2
    exit 1
fi
if [ -z "$cargo_path" ]; then
    echo "FAIL: cargo is missing on PATH (rustc resolved to ${rustc_path})." >&2
    echo "  required by repository pin (${pin_file}): ${pin}" >&2
    exit 1
fi
if [ -z "$rustc_path" ]; then
    echo "FAIL: rustc is missing on PATH (cargo resolved to ${cargo_path})." >&2
    echo "  required by repository pin (${pin_file}): ${pin}" >&2
    exit 1
fi

# `cargo --version` prints e.g. "cargo 1.96.1 (7dd11e05a 2026-01-08)"; take the
# second field verbatim so a suffixed build (1.96.1-nightly) compares as a
# mismatch rather than a pass. A failed or unparseable probe is a broken
# toolchain, never a clean pass.
probe_version() {
    local bin="$1" out
    if ! out="$("$bin" --version 2>&1)"; then
        echo "PROBE-FAILED"
        return
    fi
    local name ver
    read -r name ver _ <<<"$out"
    if [ -n "$ver" ]; then
        printf '%s' "$ver"
    else
        echo "PROBE-FAILED"
    fi
}

cargo_ver="$(probe_version "$cargo_path")"
rustc_ver="$(probe_version "$rustc_path")"

# ── Compare against the pin ─────────────────────────────────────────────

if [ "$cargo_ver" = "$pin" ] && [ "$rustc_ver" = "$pin" ]; then
    echo "toolchain gate: cargo ${cargo_ver} (${cargo_path}), rustc ${rustc_ver} (${rustc_path}) — matches pin ${pin}"
    exit 0
fi

echo "FAIL: active Rust toolchain does not match the repository pin." >&2
echo "  required  : ${pin}  (from ${pin_file})" >&2
echo "  cargo     : ${cargo_ver}  (${cargo_path})" >&2
echo "  rustc     : ${rustc_ver}  (${rustc_path})" >&2
if command -v rustup >/dev/null 2>&1; then
    echo "  rustup reports active toolchain: $(rustup show active-toolchain 2>/dev/null || echo 'unknown')" >&2
    echo "  rustup resolves cargo to: $(rustup which cargo 2>/dev/null || echo 'unknown')" >&2
fi
echo "Remediation:" >&2
if [ "$cargo_ver" = "PROBE-FAILED" ] || [ "$rustc_ver" = "PROBE-FAILED" ]; then
    echo "  - a resolved binary failed to report its version; fix the broken" >&2
    echo "    installation at the path printed above." >&2
else
    echo "  - rustup: run 'rustup toolchain install ${pin}' and operate inside" >&2
    echo "    this repository; ${pin_file} then selects the pin automatically." >&2
    echo "  - if the paths above are outside your rustup prefix (for example a" >&2
    echo "    Homebrew /opt/homebrew/bin cargo shadowing rustup), fix PATH" >&2
    echo "    ordering so the rustup-managed cargo resolves first." >&2
fi
echo "This guard never installs or switches toolchains itself." >&2
exit 1

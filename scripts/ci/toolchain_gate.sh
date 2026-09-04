#!/usr/bin/env bash

# Toolchain pin guard: fails before any Rust-dependent work when the active
# cargo/rustc do not match the repository pin. The expected version is derived
# from the root toolchain pin file — rust-toolchain.toml, or the legacy
# extensionless rust-toolchain which takes precedence when both exist, matching
# rustup — there is no second version constant in this script.
#
# Exit status:
#   0 — pinned toolchain active (paths and versions echoed)
#   1 — version mismatch (or a resolved binary failed to report a usable
#       version); observed vs required printed
#   2 — fatal: the pin itself is missing, unreadable, malformed, or not a
#       concrete version channel (stable/beta/nightly cannot be checked
#       statically)
#   3 — cargo and/or rustc missing from PATH
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

# rustup resolution order: extensionless rust-toolchain first, then
# rust-toolchain.toml. Mirror it so the guard enforces what rustup enforces.
pin_file="${repo_root}/rust-toolchain"
pin_file_toml="${repo_root}/rust-toolchain.toml"
if [ ! -f "$pin_file" ] && [ ! -f "$pin_file_toml" ]; then
    die_fatal "no rust-toolchain.toml at the repository root; the pin the guard must enforce is missing."
fi
[ -f "$pin_file" ] || pin_file="$pin_file_toml"

# Table-aware channel extraction: only a channel entry inside [toolchain]
# counts, both TOML string quote styles are accepted, and anything after the
# value other than whitespace or a comment makes the line malformed.
parse_channel() {
    awk -v sq="'" '
        /^[[:space:]]*\[/ {
            in_toolchain = ($0 ~ /^[[:space:]]*\[[[:space:]]*toolchain[[:space:]]*\][[:space:]]*(#.*)?$/)
            next
        }
        in_toolchain && /^[[:space:]]*channel[[:space:]]*=/ {
            line = $0
            sub(/^[[:space:]]*channel[[:space:]]*=[[:space:]]*/, "", line)
            q = substr(line, 1, 1)
            if (q == "\"" || q == sq) {
                rest = substr(line, 2)
                i = index(rest, q)
                if (i > 0) {
                    tail = substr(rest, i + 1)
                    if (tail ~ /^[[:space:]]*(#.*)?$/) {
                        print substr(rest, 1, i - 1)
                        exit
                    }
                }
            }
            print "__MALFORMED__"
            exit
        }
    ' "$1"
}

pin="$(parse_channel "$pin_file")"
if [ "$pin" = "__MALFORMED__" ]; then
    die_fatal "malformed channel entry in ${pin_file}; the pin must be a quoted TOML string."
fi

# Legacy extensionless files may hold a bare toolchain name on one line
# (rustup falls back to that when the content is not TOML).
if [ -z "$pin" ] && [ "$pin_file" = "${repo_root}/rust-toolchain" ]; then
    first_line="$(head -n 1 "$pin_file" | tr -d '\r' | sed 's/[[:space:]]*$//')"
    case "$first_line" in
        ""|*\ *|*=*|\[*|\#*) first_line="" ;;
    esac
    pin="$first_line"
fi

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

if [ -z "$cargo_path" ] || [ -z "$rustc_path" ]; then
    echo "FAIL: Rust toolchain missing on PATH." >&2
    echo "  cargo: ${cargo_path:-missing}" >&2
    echo "  rustc: ${rustc_path:-missing}" >&2
    echo "  required by repository pin (${pin_file}): ${pin}" >&2
    echo "Remediation: install Rust with rustup (https://rustup.rs), then run" >&2
    echo "inside this repository so the pin file selects the pinned toolchain." >&2
    exit 3
fi

# `cargo --version` prints e.g. "cargo 1.96.1 (7dd11e05a 2026-01-08)"; the
# second field is taken verbatim so a suffixed build (1.96.1-nightly) compares
# as a mismatch rather than a pass. The leading name must identify the probed
# binary; a failed, unparseable, or misidentified probe is a broken toolchain,
# never a clean pass.
probe_version() {
    local want="$1" bin="$2" out
    if ! out="$("$bin" --version 2>&1)"; then
        echo "PROBE-FAILED"
        return
    fi
    local name ver
    read -r name ver _ <<<"$out"
    if [ "$name" = "$want" ] && [ -n "$ver" ]; then
        printf '%s' "$ver"
    else
        echo "PROBE-FAILED"
    fi
}

cargo_ver="$(probe_version cargo "$cargo_path")"
rustc_ver="$(probe_version rustc "$rustc_path")"

# ── Compare against the pin ─────────────────────────────────────────────

if [ "$cargo_ver" = "$pin" ] && [ "$rustc_ver" = "$pin" ]; then
    echo "toolchain gate: cargo ${cargo_ver} (${cargo_path}), rustc ${rustc_ver} (${rustc_path}) — matches pin ${pin} (from $(basename "$pin_file"))"
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
    echo "  - a resolved binary failed to report a usable version; fix the broken" >&2
    echo "    installation at the path printed above." >&2
else
    echo "  - rustup: run 'rustup toolchain install ${pin}' and operate inside" >&2
    echo "    this repository; the pin file then selects the pin automatically." >&2
    echo "  - if the paths above are outside your rustup prefix (for example a" >&2
    echo "    Homebrew /opt/homebrew/bin cargo shadowing rustup), fix PATH" >&2
    echo "    ordering so the rustup-managed cargo resolves first." >&2
fi
echo "This guard never installs or switches toolchains itself." >&2
exit 1

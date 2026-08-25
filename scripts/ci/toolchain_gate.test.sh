#!/usr/bin/env bash

# Fixture tests for toolchain_gate.sh: with stub cargo/rustc pinned on PATH,
# the match case passes, mismatch (exit 1) and missing-toolchain (exit 3)
# cases fail loudly with the observed/expected versions and resolved paths,
# pin-file problems are fatal (exit 2), the extensionless rust-toolchain file
# takes precedence when both exist (matching rustup), and the pin is derived
# from the fixture pin file (no duplicated version constant).

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
gate="${script_dir}/toolchain_gate.sh"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Fake repository root: the guard locates the pin relative to its own path
# (scripts/ci/../..), so plant the gate copy where it expects to live.
repo="${tmp}/repo"
mkdir -p "${repo}/scripts/ci" "${tmp}/bin" "${tmp}/emptybin"
cp "$gate" "${repo}/scripts/ci/toolchain_gate.sh"

# Each stub prints whatever its <name>_out file holds (an EXIT_7 marker makes
# it fail outright), so every case controls the observed output exactly.
make_stub() {
    local name="$1"
    cat >"${tmp}/bin/${name}" <<STUB
#!/usr/bin/env sh
out="\$(cat "\$(dirname "\$0")/${name}_out" 2>/dev/null || true)"
[ "\$out" = "EXIT_7" ] && exit 7
echo "\$out"
STUB
    chmod +x "${tmp}/bin/${name}"
}
set_out() {
    printf '%s\n' "$2" >"${tmp}/bin/$1_out"
}
make_stub cargo
make_stub rustc

pin_toml() {
    printf '%s' "$1" >"${repo}/rust-toolchain.toml"
}

# Base PATH contains only the stub dir plus the core utilities the guard itself
# needs (sed/awk/head/command); this machine's real cargo/rustc must never leak
# into a fixture result, and the missing-toolchain cases must truly lack them.
STUB_PATH="${tmp}/bin:/usr/bin:/bin"
CORE_PATH="/usr/bin:/bin"

pass_count=0
fail_count=0

run_gate() {
    local path="$1"
    set +e
    out="$(cd "${repo}" && PATH="${path}" bash "${repo}/scripts/ci/toolchain_gate.sh" 2>&1)"
    status=$?
    set -e
}

check() {
    local name="$1" want_status="$2"
    shift 2
    if [ "$status" -ne "$want_status" ]; then
        echo "NOT CAUGHT (wanted exit ${want_status}, got ${status}): ${name}"
        fail_count=$((fail_count + 1))
        return
    fi
    local needle
    for needle in "$@"; do
        if ! grep -qF "$needle" <<<"$out"; then
            echo "DIAGNOSTIC MISSING: ${name} (wanted '${needle}')"
            fail_count=$((fail_count + 1))
            return
        fi
    done
    pass_count=$((pass_count + 1))
}

# ── match: pinned toolchain passes ─────────────────────────────────────

pin_toml '[toolchain]
channel = "1.96.1"
'
set_out cargo "cargo 1.96.1 (deadbeef 2026-01-01)"
set_out rustc "rustc 1.96.1 (deadbeef 2026-01-01)"
run_gate "$STUB_PATH"
check "pinned toolchain passes" 0 "matches pin 1.96.1"

# Pin edits flow through without touching the guard: a different pin with
# matching stubs still passes.
pin_toml '[toolchain]
channel = "1.97.0"
'
set_out cargo "cargo 1.97.0 (deadbeef 2026-01-01)"
set_out rustc "rustc 1.97.0 (deadbeef 2026-01-01)"
run_gate "$STUB_PATH"
check "pin value is derived, not hardcoded" 0 "matches pin 1.97.0"

pin_toml '[toolchain]
channel = '"'"'1.96.1'"'"'
'
set_out cargo "cargo 1.96.1 (deadbeef 2026-01-01)"
set_out rustc "rustc 1.96.1 (deadbeef 2026-01-01)"
run_gate "$STUB_PATH"
check "single-quoted TOML string accepted" 0 "matches pin 1.96.1"

pin_toml '[toolchain]
# comment line
channel = "1.96.1"  # trailing comment
components = ["rustfmt"]
'
run_gate "$STUB_PATH"
check "comments and sibling entries tolerated" 0 "matches pin 1.96.1"

# ── mismatch: wrong version fails before any Rust work ─────────────────

pin_toml '[toolchain]
channel = "1.96.1"
'
set_out cargo "cargo 1.98.0 (deadbeef 2026-08-05)"
set_out rustc "rustc 1.98.0 (deadbeef 2026-08-18)"
run_gate "$STUB_PATH"
check "cargo and rustc mismatch fails" 1 \
    "does not match" "required" "1.96.1" "1.98.0" "${tmp}/bin/cargo" "${tmp}/bin/rustc"

set_out cargo "cargo 1.96.1 (deadbeef 2026-01-01)"
set_out rustc "rustc 1.97.0 (deadbeef 2026-02-02)"
run_gate "$STUB_PATH"
check "rustc-only mismatch fails" 1 "does not match" "1.96.1" "1.97.0"

set_out rustc "rustc 1.96.1-nightly (deadbeef 2026-01-01)"
run_gate "$STUB_PATH"
check "suffixed rustc build is a mismatch, not a pass" 1 "does not match" "1.96.1-nightly"

# ── missing toolchain (exit 3), distinct from mismatch (exit 1) ─────────

set_out cargo "cargo 1.96.1 (deadbeef 2026-01-01)"
set_out rustc "rustc 1.96.1 (deadbeef 2026-01-01)"
run_gate "$CORE_PATH:${tmp}/emptybin"
check "missing cargo and rustc fails as missing" 3 \
    "missing" "cargo: missing" "rustc: missing" "1.96.1"

rm -f "${tmp}/bin/cargo"
run_gate "$STUB_PATH"
check "missing cargo only fails as missing" 3 "cargo: missing" "1.96.1"
make_stub cargo
set_out cargo "cargo 1.96.1 (deadbeef 2026-01-01)"

rm -f "${tmp}/bin/rustc"
run_gate "$STUB_PATH"
check "missing rustc only fails as missing" 3 "rustc: missing" "1.96.1"
make_stub rustc
set_out rustc "rustc 1.96.1 (deadbeef 2026-01-01)"

# ── broken probes never pass silently ───────────────────────────────────

set_out cargo "EXIT_7"
run_gate "$STUB_PATH"
check "failing cargo probe fails" 1 "does not match" "PROBE-FAILED"
set_out cargo "cargo 1.96.1 (deadbeef 2026-01-01)"

set_out rustc "notrustc 1.96.1 (deadbeef 2026-01-01)"
run_gate "$STUB_PATH"
check "misidentified probe name fails" 1 "does not match" "PROBE-FAILED"
set_out rustc "rustc 1.96.1 (deadbeef 2026-01-01)"

set_out cargo "hello"
run_gate "$STUB_PATH"
check "unparseable probe output fails" 1 "does not match" "PROBE-FAILED"
set_out cargo "cargo 1.96.1 (deadbeef 2026-01-01)"

# ── fatal pin problems are exit 2, never a pass ─────────────────────────

pin_toml '[toolchain]
channel = "stable"
'
run_gate "$STUB_PATH"
check "named channel pin is a fatal unsupported form" 2 \
    "FATAL" "not a concrete version"

rm -f "${repo}/rust-toolchain.toml" "${repo}/rust-toolchain"
run_gate "$STUB_PATH"
check "missing pin file is fatal" 2 "FATAL" "no rust-toolchain.toml"

pin_toml '[toolchain]
components = ["rustfmt"]
'
run_gate "$STUB_PATH"
check "pin file without channel is fatal" 2 "FATAL" "no channel entry"

pin_toml '[other]
channel = "9.9.9"

[toolchain]
components = ["rustfmt"]
'
run_gate "$STUB_PATH"
check "channel outside the toolchain table ignored" 2 "FATAL" "no channel entry"

pin_toml '[toolchain]
channel = "1.96.1" trailing-garbage
'
run_gate "$STUB_PATH"
check "trailing garbage after the value is fatal" 2 "FATAL" "malformed"

# ── extensionless rust-toolchain: precedence and legacy forms ───────────

pin_toml '[toolchain]
channel = "1.96.1"
'
printf '[toolchain]\nchannel = "1.97.0"\n' >"${repo}/rust-toolchain"
set_out cargo "cargo 1.97.0 (deadbeef 2026-03-03)"
set_out rustc "rustc 1.97.0 (deadbeef 2026-03-03)"
run_gate "$STUB_PATH"
check "extensionless pin file takes precedence over the toml file" 0 \
    "matches pin 1.97.0" "rust-toolchain"

set_out cargo "cargo 1.96.1 (deadbeef 2026-01-01)"
set_out rustc "rustc 1.96.1 (deadbeef 2026-01-01)"
run_gate "$STUB_PATH"
check "toml pin is not used when the extensionless file exists" 1 \
    "does not match" "1.97.0"

rm -f "${repo}/rust-toolchain.toml"
printf '1.96.1\n' >"${repo}/rust-toolchain"
run_gate "$STUB_PATH"
check "legacy bare-name pin file is honored" 0 "matches pin 1.96.1"

printf '[toolchain]\nchannel = "1.96.1"\n' >"${repo}/rust-toolchain"
run_gate "$STUB_PATH"
check "extensionless file with TOML content is honored" 0 "matches pin 1.96.1"
rm -f "${repo}/rust-toolchain"

echo
echo "${pass_count} passed, ${fail_count} failed"
[ "$fail_count" -eq 0 ]

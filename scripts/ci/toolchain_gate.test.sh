#!/usr/bin/env bash

# Fixture tests for toolchain_gate.sh: with stub cargo/rustc pinned on PATH,
# the match case passes, mismatch and missing-toolchain cases fail loudly with
# the observed/expected versions and resolved paths, and the pin is derived
# from the fixture rust-toolchain.toml (no duplicated version constant).

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

# The stubs print a version read from a sibling file so each case can flip the
# observed version without rewriting the executable.
make_stub() {
    local name="$1"
    cat >"${tmp}/bin/${name}" <<STUB
#!/usr/bin/env sh
v="\$(cat "\$(dirname "\$0")/${name}_version" 2>/dev/null || true)"
[ -n "\$v" ] || exit 3
echo "${name} \$v (deadbeef 2026-01-01)"
STUB
    chmod +x "${tmp}/bin/${name}"
}
set_version() {
    printf '%s' "$2" >"${tmp}/bin/$1_version"
}
make_stub cargo
make_stub rustc

pin_repo() {
    printf '[toolchain]\nchannel = "%s"\n' "$1" >"${repo}/rust-toolchain.toml"
}

# Base PATH contains only the stub dir plus the core utilities the guard itself
# needs (sed/head/command); this machine's real cargo/rustc must never leak
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

pin_repo "1.96.1"
set_version cargo "1.96.1"
set_version rustc "1.96.1"
run_gate "$STUB_PATH"
check "pinned toolchain passes" 0 "matches pin 1.96.1"

# Pin edits flow through without touching the guard: a different pin with
# matching stubs still passes.
pin_repo "1.97.0"
set_version cargo "1.97.0"
set_version rustc "1.97.0"
run_gate "$STUB_PATH"
check "pin value is derived, not hardcoded" 0 "matches pin 1.97.0"

# ── mismatch: wrong version fails before any Rust work ─────────────────

pin_repo "1.96.1"
set_version cargo "1.98.0"
set_version rustc "1.98.0"
run_gate "$STUB_PATH"
check "cargo and rustc mismatch fails" 1 \
    "does not match" "required" "1.96.1" "1.98.0" "${tmp}/bin/cargo" "${tmp}/bin/rustc"

set_version cargo "1.96.1"
set_version rustc "1.97.0"
run_gate "$STUB_PATH"
check "rustc-only mismatch fails" 1 "does not match" "1.96.1" "1.97.0"

set_version cargo "1.96.1"
set_version rustc "1.96.1-nightly"
run_gate "$STUB_PATH"
check "suffixed rustc build is a mismatch, not a pass" 1 "does not match" "1.96.1-nightly"

# ── missing toolchain: distinct from a version mismatch ─────────────────

set_version cargo "1.96.1"
set_version rustc "1.96.1"
run_gate "$CORE_PATH:${tmp}/emptybin"
check "missing cargo and rustc fails as missing, not mismatch" 1 \
    "missing" "neither cargo nor rustc" "1.96.1"

rm -f "${tmp}/bin/cargo"
run_gate "$STUB_PATH"
check "missing cargo only fails as missing" 1 "cargo is missing on PATH" "1.96.1"
make_stub cargo
set_version cargo "1.96.1"

rm -f "${tmp}/bin/rustc" "${tmp}/bin/rustc_version"
run_gate "$STUB_PATH"
check "missing rustc only fails as missing" 1 "rustc is missing on PATH" "1.96.1"
make_stub rustc
set_version rustc "1.96.1"

# ── broken probe never passes silently ──────────────────────────────────

printf '#!/usr/bin/env sh\nexit 7\n' >"${tmp}/bin/cargo"
chmod +x "${tmp}/bin/cargo"
run_gate "$STUB_PATH"
check "broken cargo probe fails" 1 "does not match" "PROBE-FAILED"
make_stub cargo
set_version cargo "1.96.1"

# ── fatal pin problems are exit 2, never a pass ─────────────────────────

pin_repo "stable"
run_gate "$STUB_PATH"
check "named channel pin is a fatal unsupported form" 2 \
    "FATAL" "not a concrete version"

rm -f "${repo}/rust-toolchain.toml"
run_gate "$STUB_PATH"
check "missing pin file is fatal" 2 "FATAL" "no rust-toolchain.toml"

printf '[toolchain]\ncomponents = ["rustfmt"]\n' >"${repo}/rust-toolchain.toml"
run_gate "$STUB_PATH"
check "pin file without channel is fatal" 2 "FATAL" "no channel entry"

rm -f "${repo}/rust-toolchain.toml"
printf '[toolchain]\nchannel = "1.96.1"\n' >"${repo}/rust-toolchain"
run_gate "$STUB_PATH"
check "legacy rust-toolchain file is honored" 0 "matches pin 1.96.1"
rm -f "${repo}/rust-toolchain"

echo
echo "${pass_count} passed, ${fail_count} failed"
[ "$fail_count" -eq 0 ]

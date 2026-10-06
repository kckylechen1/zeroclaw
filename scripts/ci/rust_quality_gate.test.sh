#!/usr/bin/env bash
# Exercise the real helper and hook in an isolated repo; no Cargo work runs.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/repo/scripts/ci" "$tmp/repo/.githooks" "$tmp/bin"
cp "$root/scripts/ci/rust_quality_gate.sh" "$tmp/repo/scripts/ci/"
cp "$root/.githooks/pre-push" "$tmp/repo/.githooks/"
cat > "$tmp/bin/stub" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
name="${0##*/}"
event="$name${*:+ $*}"
printf '%s\n' "$event" >> "$TRACE"
if [ "$event" = "${FAIL_EVENT:-}" ]; then exit 7; fi
STUB
chmod +x "$tmp/bin/stub"
cp "$tmp/bin/stub" "$tmp/bin/cargo"
for gate in toolchain provider_dispatch docs_quality docs_links; do
    cp "$tmp/bin/stub" "$tmp/repo/scripts/ci/${gate}_gate.sh"
done
export PATH="$tmp/bin:/usr/bin:/bin" TRACE="$tmp/trace"
export ZEROCLAW_STRICT_LINT=0 ZEROCLAW_DOCS_LINT=0 ZEROCLAW_DOCS_LINKS=0 FAIL_EVENT=
cd "$tmp/repo"

fmt='cargo fmt --all -- --check'
correctness='cargo clippy --locked --workspace --all-targets --features zeroclaw-channels/heavy-tests,zeroclaw-runtime/heavy-tests -- -D clippy::correctness'
strict='cargo clippy --locked --workspace --all-targets --features ci-all -- -D warnings'
base=(toolchain_gate.sh "$fmt")
normal=("${base[@]}" "$correctness" provider_dispatch_gate.sh)
both=("${normal[@]}" "$strict")
full=("${both[@]}" docs_quality_gate.sh docs_links_gate.sh 'cargo test --locked --workspace')

run() {
    : > "$TRACE"
    status=0
    "$@" > "$tmp/output" 2>&1 || status=$?
}
check() {
    local expected_status="$1"
    shift
    if [ "$status" -ne "$expected_status" ]; then
        cat "$tmp/output" >&2
        echo "Expected exit $expected_status, got $status" >&2
        exit 1
    fi
    : > "$tmp/expected"
    if [ "$#" -gt 0 ]; then printf '%s\n' "$@" > "$tmp/expected"; fi
    diff -u "$tmp/expected" "$TRACE"
    if [ "$status" -ne 0 ] && grep -q 'all checks passed' "$tmp/output"; then
        echo 'Failure reported success' >&2
        exit 1
    fi
}

run ./scripts/ci/rust_quality_gate.sh
check 0 "${normal[@]}"
run ./scripts/ci/rust_quality_gate.sh --strict
check 0 "${base[@]}" "$strict" provider_dispatch_gate.sh
run ./scripts/ci/rust_quality_gate.sh --both
check 0 "${both[@]}"
run ./.githooks/pre-push
check 0 "${normal[@]}" 'cargo test --locked --workspace'
ZEROCLAW_STRICT_LINT=1
run ./.githooks/pre-push
check 0 "${both[@]}" 'cargo test --locked --workspace'
ZEROCLAW_DOCS_LINT=1 ZEROCLAW_DOCS_LINKS=1
run ./.githooks/pre-push
check 0 "${full[@]}"

# Every stage must stop the real hook at the exact failing command.
for ((i=0; i<${#full[@]}; i++)); do
    FAIL_EVENT="${full[i]}"
    run ./.githooks/pre-push
    check 1 "${full[@]:0:i+1}"
done
FAIL_EVENT=
run ./scripts/ci/rust_quality_gate.sh --unknown
check 2
run ./scripts/ci/rust_quality_gate.sh --strict --both
check 2

echo 'Rust quality gate orchestration tests passed.'

#!/usr/bin/env bash
# Exercise the actual launcher in real main/linked Git worktrees; no compilation.
set -euo pipefail
unset CDPATH
source_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fixture="$(mktemp -d "${TMPDIR:-/tmp}/cargo-local-test.XXXXXX")"
trap 'rm -rf -- "$fixture"' EXIT
main="$fixture/main checkout"
linked="$fixture/linked checkout"
mkdir -p "$main/scripts/dev" "$main/scripts/ci" "$main/dev/ci" "$fixture/bin"
cp "$source_root/scripts/dev/cargo-local.sh" "$source_root/scripts/dev/shared_target.py" "$source_root/scripts/dev/leg.sh" "$source_root/scripts/dev/target_sweep.sh" "$main/scripts/dev/"
printf '%s\n' '{"legs":{"demo":["example"]},"excluded":[],"extra_runs":{"demo":[{"name":"extra","features":["extra"],"filter":"boundary"}]}}' > "$main/dev/ci/test-partition.json"
printf '[workspace]\nmembers = []\n' > "$main/Cargo.toml"
cat > "$main/scripts/ci/toolchain_gate.sh" <<'GATE'
#!/usr/bin/env bash
exit "${TEST_GATE_EXIT:-0}"
GATE
cat > "$fixture/bin/cargo" <<'CARGO'
#!/usr/bin/env bash
printf '%s\n' "$CARGO_TARGET_DIR" "$CARGO_INCREMENTAL" "$PWD" "$@" > "$TEST_CARGO_LOG"
printf '%s\n' "${CARGO_PROFILE_DEV_DEBUG:-unset}" "${CARGO_PROFILE_TEST_DEBUG:-unset}" > "$TEST_PROFILE_LOG"
if [[ -n "${TEST_CARGO_TRACE:-}" ]]; then
  printf '%s|%s|%s|%s\n' "$CARGO_TARGET_DIR" "$CARGO_INCREMENTAL" "$PWD" "$*" >> "$TEST_CARGO_TRACE"
fi
if [[ "${1:-}" == metadata ]]; then printf '%s\n' '{"workspace_members":["example 0.1.0"],"packages":[{"id":"example 0.1.0","name":"example","targets":[{"name":"example"}]}]}'; fi
CARGO
chmod +x "$fixture/bin/cargo"
git init -q "$main"
git -C "$main" add .
git -C "$main" -c user.name=Fixture -c user.email=fixture@example.invalid commit -qm fixture
git -C "$main" worktree add -q --detach "$linked"
main="$(cd "$main" && pwd -P)"
linked="$(cd "$linked" && pwd -P)"
export PATH="$fixture/bin:$PATH" TEST_CARGO_LOG="$fixture/cargo.log" TEST_PROFILE_LOG="$fixture/profiles.log"
# Isolate host admission from the developer's installed gate.
export HOME="$fixture/home"
mkdir -p "$HOME"
unset CARGO_TARGET_DIR CARGO_INCREMENTAL CARGO_PROFILE_DEV_DEBUG CARGO_PROFILE_TEST_DEBUG
for checkout in "$main" "$linked"; do
  bash "$checkout/scripts/dev/cargo-local.sh" test -p example -- 'name with spaces'
  printf '%s\n' "$main/target" 0 "$checkout" test -p example -- 'name with spaces' > "$fixture/expected"
  diff -u "$fixture/expected" "$TEST_CARGO_LOG"
  printf '%s\n' 0 unset > "$fixture/expected-profiles"
  diff -u "$fixture/expected-profiles" "$TEST_PROFILE_LOG"
done
CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_PROFILE_TEST_DEBUG=full \
  bash "$linked/scripts/dev/cargo-local.sh" test --lib
printf '%s\n' line-tables-only full > "$fixture/expected-profiles"
diff -u "$fixture/expected-profiles" "$TEST_PROFILE_LOG"
CARGO_PROFILE_TEST_DEBUG=line-tables-only bash "$linked/scripts/dev/cargo-local.sh" test --lib
printf '%s\n' 0 line-tables-only > "$fixture/expected-profiles"
diff -u "$fixture/expected-profiles" "$TEST_PROFILE_LOG"
CARGO_TARGET_DIR="$fixture/explicit target" CARGO_INCREMENTAL=1 \
  bash "$linked/scripts/dev/cargo-local.sh" check
printf '%s\n' "$fixture/explicit target" 1 "$linked" check > "$fixture/expected"
diff -u "$fixture/expected" "$TEST_CARGO_LOG"
export TEST_CARGO_TRACE="$fixture/trace"
LEG_RUNNER=test bash "$linked/scripts/dev/leg.sh" demo --no-fail-fast
printf '%s|0|%s|%s\n' "$main/target" "$linked" 'test --locked -p example --no-fail-fast' "$main/target" "$linked" 'test --locked -p example --features extra -- boundary' > "$fixture/expected"
diff -u "$fixture/expected" "$TEST_CARGO_TRACE"
: > "$TEST_CARGO_TRACE"
unset LEG_RUNNER
CARGO_TARGET_DIR="$fixture/explicit target" bash "$linked/scripts/dev/leg.sh" demo
printf '%s|0|%s|%s\n' "$fixture/explicit target" "$linked" 'nextest --version' "$fixture/explicit target" "$linked" 'nextest run --locked -p example' "$fixture/explicit target" "$linked" 'nextest run --locked -p example --features extra -- boundary' > "$fixture/expected"
diff -u "$fixture/expected" "$TEST_CARGO_TRACE"
: > "$TEST_CARGO_TRACE"
bash "$linked/scripts/dev/leg.sh" --check
printf '%s|0|%s|%s\n' "$main/target" "$linked" 'metadata --no-deps --format-version 1 --locked' > "$fixture/expected"
diff -u "$fixture/expected" "$TEST_CARGO_TRACE"
: > "$TEST_CARGO_TRACE"
mkdir -p "$main/target/debug"
(
  cd "$linked"
  export CDPATH=".:$fixture"
  bash scripts/dev/leg.sh --list >/dev/null
  CARGO_TARGET_DIR="$main/target" bash scripts/dev/target_sweep.sh --dry-run >/dev/null
  bash scripts/dev/cargo-local.sh check
)
printf '%s\n' "$main/target" 0 "$linked" check > "$fixture/expected"
diff -u "$fixture/expected" "$TEST_CARGO_LOG"
unset TEST_CARGO_TRACE
rm "$TEST_CARGO_LOG"
if TEST_GATE_EXIT=9 bash "$linked/scripts/dev/cargo-local.sh" check; then
  echo 'launcher ignored toolchain failure' >&2; exit 1
fi
test ! -e "$TEST_CARGO_LOG"
mkdir -p "$HOME/.local/state/host-build" "$HOME/bin"
printf '{"schema":1}\n' > "$HOME/.local/state/host-build/managed.json"
if bash "$linked/scripts/dev/cargo-local.sh" check; then
  echo 'managed host silently bypassed missing admission' >&2; exit 1
fi
test ! -e "$TEST_CARGO_LOG"
cat > "$HOME/bin/host-build" <<'HOST'
#!/bin/bash
[[ "$1" == -- && "$2" == cargo ]] || exit 64
printf 'admitted\n' > "$TEST_HOST_LOG"
shift
exec "$@"
HOST
chmod +x "$HOME/bin/host-build"
export TEST_HOST_LOG="$fixture/host.log"
bash "$linked/scripts/dev/cargo-local.sh" test -p example -- 'name with spaces'
test "$(cat "$TEST_HOST_LOG")" = admitted
printf '%s\n' "$main/target" 0 "$linked" test -p example -- 'name with spaces' > "$fixture/expected"
diff -u "$fixture/expected" "$TEST_CARGO_LOG"
rm "$TEST_CARGO_LOG"
separate="$fixture/separate"
git init -q --separate-git-dir="$fixture/git-storage" "$separate"
mkdir -p "$separate/scripts/dev" "$separate/scripts/ci"
cp "$source_root/scripts/dev/cargo-local.sh" "$source_root/scripts/dev/shared_target.py" "$separate/scripts/dev/"
cp "$main/scripts/ci/toolchain_gate.sh" "$separate/scripts/ci/"
if bash "$separate/scripts/dev/cargo-local.sh" check 2> "$fixture/error"; then
  echo 'launcher guessed a separate Git directory target' >&2; exit 1
fi
grep -q 'Set CARGO_TARGET_DIR explicitly' "$fixture/error"
test ! -e "$TEST_CARGO_LOG"
printf 'PASS: main/linked worktrees share target; overrides and arguments preserved; test legs/metadata/nextest use the same target; failed guard and unsupported layout stop before Cargo\n'
python3 "$source_root/scripts/dev/shared_target.test.py" "$BASH"

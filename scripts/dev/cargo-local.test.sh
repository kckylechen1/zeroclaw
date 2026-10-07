#!/usr/bin/env bash
# Exercise the actual launcher in real main/linked Git worktrees; no compilation.
set -euo pipefail
source_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fixture="$(mktemp -d "${TMPDIR:-/tmp}/cargo-local-test.XXXXXX")"
trap 'rm -rf -- "$fixture"' EXIT
main="$fixture/main checkout"
linked="$fixture/linked checkout"
mkdir -p "$main/scripts/dev" "$main/scripts/ci" "$fixture/bin"
cp "$source_root/scripts/dev/cargo-local.sh" "$main/scripts/dev/"
printf '[workspace]\nmembers = []\n' > "$main/Cargo.toml"
cat > "$main/scripts/ci/toolchain_gate.sh" <<'GATE'
#!/usr/bin/env bash
exit "${TEST_GATE_EXIT:-0}"
GATE
cat > "$fixture/bin/cargo" <<'CARGO'
#!/usr/bin/env bash
printf '%s\n' "$CARGO_TARGET_DIR" "$CARGO_INCREMENTAL" "$PWD" "$@" > "$TEST_CARGO_LOG"
CARGO
chmod +x "$fixture/bin/cargo"
git init -q "$main"
git -C "$main" add .
git -C "$main" -c user.name=Fixture -c user.email=fixture@example.invalid commit -qm fixture
git -C "$main" worktree add -q --detach "$linked"
main="$(cd "$main" && pwd -P)"
linked="$(cd "$linked" && pwd -P)"
export PATH="$fixture/bin:$PATH" TEST_CARGO_LOG="$fixture/cargo.log"
unset CARGO_TARGET_DIR CARGO_INCREMENTAL
for checkout in "$main" "$linked"; do
  bash "$checkout/scripts/dev/cargo-local.sh" test -p example -- 'name with spaces'
  printf '%s\n' "$main/target" 0 "$checkout" test -p example -- 'name with spaces' > "$fixture/expected"
  diff -u "$fixture/expected" "$TEST_CARGO_LOG"
done
CARGO_TARGET_DIR="$fixture/explicit target" CARGO_INCREMENTAL=1 \
  bash "$linked/scripts/dev/cargo-local.sh" check
printf '%s\n' "$fixture/explicit target" 1 "$linked" check > "$fixture/expected"
diff -u "$fixture/expected" "$TEST_CARGO_LOG"
rm "$TEST_CARGO_LOG"
if TEST_GATE_EXIT=9 bash "$linked/scripts/dev/cargo-local.sh" check; then
  echo 'launcher ignored toolchain failure' >&2; exit 1
fi
test ! -e "$TEST_CARGO_LOG"
separate="$fixture/separate"
git init -q --separate-git-dir="$fixture/git-storage" "$separate"
mkdir -p "$separate/scripts/dev"
cp "$source_root/scripts/dev/cargo-local.sh" "$separate/scripts/dev/"
if bash "$separate/scripts/dev/cargo-local.sh" check 2> "$fixture/error"; then
  echo 'launcher guessed a separate Git directory target' >&2; exit 1
fi
grep -q 'Set CARGO_TARGET_DIR explicitly' "$fixture/error"
test ! -e "$TEST_CARGO_LOG"
printf 'PASS: main/linked worktrees share target; overrides and arguments preserved; failed guard and unsupported layout stop before Cargo\n'

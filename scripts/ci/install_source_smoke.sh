#!/usr/bin/env bash
# Run the actual POSIX bootstrap with disposable checkouts and mutation sentinels.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
installer="$repo_root/install.sh"
scratch="$(mktemp -d)"
real_rm="$(command -v rm)"
trap '"$real_rm" -rf "$scratch"' EXIT
real_git="$(command -v git)"
fork_url="https://github.com/kckylechen1/zeroclaw.git"
prefix="$scratch/install-prefix"
mkdir -p "$scratch/bin" "$scratch/outside"
export ZEROCLAW_SMOKE_LOG="$scratch/mutations.log"
export ZEROCLAW_REAL_GIT="$real_git"
export CARGO_HOME="$scratch/cargo-home"
export RUSTUP_HOME="$scratch/rustup-home"
: > "$ZEROCLAW_SMOKE_LOG"

cat > "$scratch/bin/git" <<'SH'
#!/bin/sh
if [ "${1:-}" = -C ]; then
  checkout=$2
  shift 2
else
  checkout=.
fi
if [ "${1:-}" = remote ] && [ "${2:-}" = get-url ]; then
  exec "$ZEROCLAW_REAL_GIT" -C "$checkout" "$@"
fi
printf 'git %s\n' "$*" >> "$ZEROCLAW_SMOKE_LOG"
exit 97
SH
for command in cargo curl rustup gh tar npm cp rm; do
  cat > "$scratch/bin/$command" <<'SH'
#!/bin/sh
printf '%s %s\n' "${0##*/}" "$*" >> "$ZEROCLAW_SMOKE_LOG"
exit 97
SH
done
cat > "$scratch/bin/rustc" <<'SH'
#!/bin/sh
printf 'rustc 1.96.1 (installer-smoke)\n'
SH
chmod +x "$scratch/bin/"*

make_checkout() {
  local path=$1 origin=$2
  mkdir -p "$path"
  cp "$repo_root/Cargo.toml" "$path/Cargo.toml"
  printf 'preserve this local file\n' > "$path/local-marker"
  "$real_git" -C "$path" init --quiet
  "$real_git" -C "$path" remote add origin "$origin"
  "$real_git" -C "$path" add Cargo.toml local-marker
  "$real_git" -C "$path" -c user.name='Installer fixture' \
    -c user.email='installer@example.invalid' commit --quiet -m fixture
  printf 'uncommitted local edit\n' >> "$path/local-marker"
}

make_checkout "$scratch/fork" "$fork_url"
make_checkout "$scratch/upstream" 'https://github.com/zeroclaw-labs/zeroclaw.git'
fork_head="$("$real_git" -C "$scratch/fork" rev-parse HEAD)"
upstream_head="$("$real_git" -C "$scratch/upstream" rev-parse HEAD)"
export PATH="$scratch/bin:$PATH"

fail() { printf 'installer smoke failed: %s\n%s\n' "$1" "${output:-}" >&2; exit 1; }
checks=0
run_installer() {
  local expected=$1 directory=$2 cache=$3
  shift 3
  local status=0
  output="$(cd "$directory" && ZEROCLAW_INSTALL_DIR="$cache" \
    sh "$installer" --prefix "$prefix" --no-modify-path --skip-quickstart "$@" \
    </dev/null 2>&1)" || status=$?
  if [[ $expected == success ]]; then
    [[ $status == 0 ]] || fail "expected success, got $status"
  else
    [[ $status != 0 ]] || fail 'expected refusal'
  fi
  checks=$((checks + 1))
}
contains() { [[ $output == *"$1"* ]] || fail "missing output: $1"; }
no_mutations() {
  [[ ! -s $ZEROCLAW_SMOKE_LOG ]] || fail "unexpected command: $(cat "$ZEROCLAW_SMOKE_LOG")"
  [[ ! -e $prefix ]] || fail 'install prefix was created'
}

for flags in '--prebuilt' '--dry-run --prebuilt' '--prebuilt --source' \
  '--source --prebuilt' '--minimal --prebuilt'; do
  read -r -a arguments <<< "$flags"
  run_installer refusal "$scratch/outside" "$scratch/missing" "${arguments[@]}"
  contains 'no verified prebuilt binary channel'
  contains "$fork_url"
  contains './install.sh --source'
  no_mutations
done

# No installation-mode flag must still choose source; dry-run must not compile.
run_installer success "$scratch/fork" "$scratch/missing" --dry-run
contains 'cargo install --path . --locked --force'
no_mutations
run_installer success "$scratch/fork" "$scratch/missing" --source --minimal --dry-run
contains 'cargo install --path . --locked --force --no-default-features'
no_mutations
run_installer success "$scratch/fork" "$scratch/missing" --full --dry-run
contains 'cargo generate features --selection all'
contains 'Feature resolution is deferred'
no_mutations

run_installer success "$scratch/outside" "$scratch/missing" --dry-run
contains "Would clone $fork_url"
[[ ! -e $scratch/missing ]] || fail 'dry-run created a checkout'
no_mutations

# Explicit upstream current/cached checkouts must fail before any update or build.
run_installer refusal "$scratch/upstream" "$scratch/missing" --source
contains 'Source origin is not this fork'
no_mutations
for flags in '--dry-run' '--source'; do
  run_installer refusal "$scratch/outside" "$scratch/upstream" "$flags"
  contains 'Source origin is not this fork'
  no_mutations
done
[[ $("$real_git" -C "$scratch/upstream" rev-parse HEAD) == "$upstream_head" ]] || fail 'upstream HEAD changed'
[[ $("$real_git" -C "$scratch/upstream" remote get-url origin) == 'https://github.com/zeroclaw-labs/zeroclaw.git' ]] || fail 'upstream origin changed'
[[ $(tail -n 1 "$scratch/upstream/local-marker") == 'uncommitted local edit' ]] || fail 'upstream local edit changed'

# A verified cached checkout previews without pulling. A real failed FF must
# stop, preserving local work instead of falling back to fetch/reset --hard.
run_installer success "$scratch/outside" "$scratch/fork" --dry-run
contains "Would fast-forward $scratch/fork"
no_mutations
run_installer refusal "$scratch/outside" "$scratch/fork" --source
contains 'no reset was attempted'
[[ $(cat "$ZEROCLAW_SMOKE_LOG") == 'git pull --ff-only --quiet origin master' ]] || fail 'unexpected update command'
[[ $("$real_git" -C "$scratch/fork" rev-parse HEAD) == "$fork_head" ]] || fail 'fork HEAD changed'
[[ $(tail -n 1 "$scratch/fork/local-marker") == 'uncommitted local edit' ]] || fail 'fork local edit changed'
: > "$ZEROCLAW_SMOKE_LOG"

mkdir -p "$scratch/not-a-checkout"
run_installer refusal "$scratch/outside" "$scratch/not-a-checkout" --dry-run
contains 'not a verified checkout'
no_mutations

for origin in 'https://github.com/kckylechen1/zeroclaw' \
  'git@github.com:kckylechen1/zeroclaw.git' \
  'ssh://git@github.com/kckylechen1/zeroclaw.git'; do
  "$real_git" -C "$scratch/fork" remote set-url origin "$origin"
  run_installer success "$scratch/fork" "$scratch/missing" --dry-run
  contains 'cargo install --path . --locked --force'
  no_mutations
done

# Uninstall preview must not call an installed binary, remove its file, ask
# about deleting data, or rewrite the caller's shell profile. The rm sentinel
# also makes a regression stop before it could reach a real profile.
mkdir -p "$CARGO_HOME/bin" "$prefix/.zeroclaw"
cat > "$CARGO_HOME/bin/zeroclaw" <<'SH'
#!/bin/sh
printf 'zeroclaw %s\n' "$*" >> "$ZEROCLAW_SMOKE_LOG"
exit 97
SH
chmod +x "$CARGO_HOME/bin/zeroclaw"
printf 'preserve uninstall data\n' > "$prefix/.zeroclaw/marker"
run_installer success "$scratch/outside" "$scratch/missing" --dry-run --uninstall
contains 'Would stop and uninstall the service'
contains 'No service, binary, configuration, or shell profile was changed'
[[ -f $CARGO_HOME/bin/zeroclaw ]] || fail 'uninstall preview removed the binary'
[[ $(cat "$prefix/.zeroclaw/marker") == 'preserve uninstall data' ]] || fail 'uninstall preview changed data'
[[ ! -s $ZEROCLAW_SMOKE_LOG ]] || fail 'uninstall preview executed a mutation'

printf 'POSIX source installer smoke: %s scenarios passed.\n' "$checks"

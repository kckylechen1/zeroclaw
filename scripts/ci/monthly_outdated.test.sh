#!/usr/bin/env bash
# Execute the production boundary with a closed PATH and no inherited credentials.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
FIXTURE=$(mktemp -d)
trap 'rm -rf "$FIXTURE"' EXIT
mkdir "$FIXTURE/bin"
ln -s /bin/cat "$FIXTURE/bin/cat"
ln -s /bin/date "$FIXTURE/bin/date"
cat > "$FIXTURE/bin/rustc" <<'STUB'
#!/bin/bash
[[ "$*" == --version ]] || exit 99
echo 'rustc fixture-version'
STUB
cat > "$FIXTURE/bin/cargo" <<'STUB'
#!/bin/bash
case "$*" in
  --version) echo 'cargo fixture-version' ;;
  'outdated --version') echo 'cargo-outdated 0.19.0' ;;
  'outdated --workspace --exit-code 10')
    printf '%s\n' "$*" >> "$RUNNER_TEMP/cargo-calls"
    echo 'raw stdout canary'
    echo 'raw stderr resolver canary' >&2
    exit "$FIXTURE_EXIT"
    ;;
  *) exit 99 ;;
esac
STUB
cat > "$FIXTURE/bin/gh" <<'STUB'
#!/bin/bash
printf '%s\n' "$*" >> "$RUNNER_TEMP/gh-calls"
case "$1 $2" in
  'api repos/fixture/repo') echo "$FIXTURE_ISSUES" ;;
  'issue list') [[ "$FIXTURE_REUSE" != yes ]] || echo 'https://example.invalid/existing' ;;
  'issue create') echo 'https://example.invalid/new' ;;
  *) exit 99 ;;
esac
STUB
chmod +x "$FIXTURE/bin/"*

for scenario in clean findings reuse disabled error other; do
  case "$scenario" in
    clean) code=0 ;; findings|reuse|disabled) code=10 ;; error) code=1 ;; other) code=42 ;;
  esac
  case "$code" in 0) result=clean ;; 10) result=inventory ;; *) result=scanner_failure ;; esac
  run="$FIXTURE/$scenario"
  mkdir "$run"
  : > "$run/outputs"
  reuse=no; issues=true
  [[ "$scenario" != reuse ]] || reuse=yes
  [[ "$scenario" != disabled ]] || issues=false
  environment=(env -i "PATH=$FIXTURE/bin" "RUNNER_TEMP=$run"
    "GITHUB_OUTPUT=$run/outputs" "GITHUB_SHA=fixture-head" "RUNNER_OS=fixture-os"
    "RUNNER_ARCH=fixture-arch" "ImageOS=fixture-image" "ImageVersion=fixture-version"
    "GITHUB_REPOSITORY=fixture/repo" "RUN_URL=https://example.invalid/run"
    "FIXTURE_EXIT=$code" "FIXTURE_REUSE=$reuse" "FIXTURE_ISSUES=$issues")
  status=0
  "${environment[@]}" /bin/bash "$ROOT/scripts/ci/monthly_outdated.sh" scan > "$run/log" 2>&1 || status=$?
  [[ "$status" == "$code" ]]
  [[ "$(cat "$run/outputs")" == "scan_exit_code=$code" ]]
  [[ "$(cat "$run/cargo-calls")" == 'outdated --workspace --exit-code 10' ]]
  report=$(cat "$run/outdated-output.txt")
  for expected in 'raw stdout canary' 'raw stderr resolver canary' 'Head: fixture-head' \
    'Runner: fixture-os fixture-arch' 'Image: fixture-image fixture-version' \
    'rustc fixture-version' 'cargo fixture-version' 'cargo-outdated 0.19.0' \
    "Scan exit code: $code" "Result: $result"; do
    [[ "$report" == *"$expected"* ]]
  done
  "${environment[@]}" "SCAN_EXIT_CODE=$code" /bin/bash "$ROOT/scripts/ci/monthly_outdated.sh" issue >> "$run/log" 2>&1
  [[ "$(cat "$run/outdated-output.txt")" == "$report" ]]
  if [[ "$code" != 10 ]]; then
    [[ ! -e "$run/gh-calls" && ! -e "$run/issue-body.md" ]]
    [[ "$(cat "$run/log")" != *'Outdated dependencies detected'* ]]
  elif [[ "$scenario" == findings ]]; then
    [[ "$(cat "$run/gh-calls")" == *'issue create '* ]]
    [[ "$(cat "$run/issue-body.md")" == *"$report"* ]]
    [[ "$(cat "$run/outputs")" == *'issue_url=https://example.invalid/new'* ]]
  else
    [[ "$(cat "$run/gh-calls")" != *'issue create '* ]]
    [[ ! -e "$run/issue-body.md" ]]
    if [[ "$scenario" == reuse ]]; then
      [[ "$(cat "$run/outputs")" == *'issue_url=https://example.invalid/existing'* ]]
    fi
  fi
  printf 'PASS %s (exit %s)\n' "$scenario" "$code"
done

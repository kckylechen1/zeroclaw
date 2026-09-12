#!/usr/bin/env bash
# The scanner exit status owns classification; stderr alone is not an inventory.
set -euo pipefail

case "${1:-}" in
  scan)
    OUTPUT_FILE="${RUNNER_TEMP:?}/outdated-output.txt"
    {
      printf '## Scan environment\n\n'
      printf 'Head: %s\nRunner: %s %s\nImage: %s %s\n' \
        "${GITHUB_SHA:?}" "${RUNNER_OS:-unknown}" "${RUNNER_ARCH:-unknown}" \
        "${ImageOS:-unknown}" "${ImageVersion:-unknown}"
      rustc --version
      cargo --version
      cargo outdated --version
      printf '\nCommand: cargo outdated --workspace --exit-code 10\n\n## Scanner output\n\n'
    } > "$OUTPUT_FILE" 2>&1

    exit_code=0
    cargo outdated --workspace --exit-code 10 >> "$OUTPUT_FILE" 2>&1 || exit_code=$?
    case "$exit_code" in
      0) result=clean ;;
      10) result=inventory ;;
      *) result=scanner_failure ;;
    esac
    printf '\nScan exit code: %s\nResult: %s\n' "$exit_code" "$result" >> "$OUTPUT_FILE"
    printf 'scan_exit_code=%s\n' "$exit_code" >> "${GITHUB_OUTPUT:?}"
    if [[ "$result" == scanner_failure ]]; then
      echo "::error::cargo outdated failed with exit code $exit_code. See the retained scan report."
    fi
    exit "$exit_code"
    ;;
  issue)
    # Keep this guard here as well as in Actions: errors must never reach gh.
    [[ "${SCAN_EXIT_CODE:-}" == 10 ]] || exit 0
    issues_enabled=$(gh api "repos/$GITHUB_REPOSITORY" --jq '.has_issues')
    if [[ "$issues_enabled" != "true" ]]; then
      echo "::notice::Repository issues are disabled; skipping issue creation."
      exit 0
    fi

    # Avoid duplicate while one is still open — link to the existing one.
    existing_url=$(gh issue list \
      --repo "$GITHUB_REPOSITORY" \
      --label "dependencies" \
      --state open \
      --search "Outdated dependencies found in:title" \
      --json url \
      --jq '.[0].url // ""')

    if [[ -n "$existing_url" ]]; then
      echo "issue_url=$existing_url" >> "$GITHUB_OUTPUT"
      echo "An open outdated-dependency issue already exists: $existing_url"
      exit 0
    fi

    OUTPUT_FILE="$RUNNER_TEMP/outdated-output.txt"
    scan_output=$(cat "$OUTPUT_FILE")

    {
      printf '## Outdated dependencies found\n\n'
      printf 'Workflow run: %s\n\n' "${RUN_URL}"
      printf 'The following dependencies have newer versions available:\n\n'
      printf '```\n%s\n```\n\n' "${scan_output}"
      printf 'Review and update dependencies at your earliest convenience.\n'
      printf 'Breaking changes may require more attention than patch bumps.\n'
    } > "$RUNNER_TEMP/issue-body.md"

    issue_url=$(gh issue create \
      --repo "$GITHUB_REPOSITORY" \
      --title "ci: Outdated dependencies found — $(date -u +%Y-%m-%d)" \
      --label "dependencies" \
      --body-file "$RUNNER_TEMP/issue-body.md")
    echo "issue_url=$issue_url" >> "$GITHUB_OUTPUT"
    ;;
  *) echo "Usage: $0 scan|issue" >&2; exit 2 ;;
esac

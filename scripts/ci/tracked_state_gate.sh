#!/usr/bin/env bash

# Tracked-index hygiene gate: fails when local runtime state or migration
# backup artifacts enter the Git index. Canonical input is `git ls-files`
# (what Git tracks), not the developer filesystem — .gitignore prevents
# future untracked additions but says nothing about already-tracked paths.
#
# Exit status: 0 = index clean, 1 = violations found, 2 = fatal error.

set -euo pipefail

if ! repo_root="$(git rev-parse --show-toplevel 2>/dev/null)"; then
    echo "FATAL: tracked-state gate must run inside a Git work tree." >&2
    exit 2
fi
cd "$repo_root"

# Tracked fixtures under an otherwise-forbidden path require an exact entry
# here. Keep this list empty unless a fixture is proven intentional;
# additions are reviewed via the diff on this script.
allowlist=(
)

if [ "${#allowlist[@]}" -ne 0 ]; then
    is_allowed() {
        local allowed
        for allowed in "${allowlist[@]}"; do
            if [ "$1" = "$allowed" ]; then
                return 0
            fi
        done
        return 1
    }
else
    is_allowed() { return 1; }
fi

if ! violations="$(mktemp 2>/dev/null)"; then
    echo "FATAL: mktemp failed for the violations report." >&2
    exit 2
fi
if ! index_list="$(mktemp 2>/dev/null)"; then
    echo "FATAL: mktemp failed for the index listing." >&2
    exit 2
fi
trap 'rm -f "$violations" "$index_list"' EXIT

# Capture the index listing through a file so a git failure is observable;
# process substitution would hide it and the gate would report clean on an
# index it could not read.
if ! git ls-files -z >"$index_list"; then
    echo "FATAL: git ls-files failed; the tracked index cannot be evaluated." >&2
    exit 2
fi

# NUL-delimited so paths with spaces/newlines survive; the guard reads the
# index only, never file contents.
while IFS= read -r -d '' path; do
    rule=""
    case "$path" in
        .tachi|.tachi/*|.zcode|.zcode/*) rule="runtime-state root" ;;
    esac
    if [ -z "$rule" ]; then
        case "${path##*/}" in
            tachi.log)          rule="runtime log" ;;
            *.migration-bak*)   rule="migration backup artifact" ;;
            *.legacy-v[0-9]*)   rule="legacy migration artifact" ;;
        esac
    fi
    if [ -n "$rule" ] && ! is_allowed "$path"; then
        # Escape control characters at capture time so the line-based
        # report cannot be garbled by a path containing a newline or tab.
        disp="${path//$'\n'/\\n}"
        disp="${disp//$'\t'/\\t}"
        printf '%s\t%s\n' "$rule" "$disp" >>"$violations"
    fi
done <"$index_list"

total="$(wc -l <"$violations" | tr -d ' ')"
if [ "$total" -eq 0 ]; then
    echo "tracked-state gate: index clean"
    exit 0
fi

max_report=50
echo "FAIL: tracked runtime-state/migration artifacts present in the Git index:"
head -n "$max_report" "$violations" | while IFS=$'\t' read -r rule path; do
    echo "  [${rule}] ${path}"
done
if [ "$total" -gt "$max_report" ]; then
    echo "  ... and $((total - max_report)) more"
fi
echo "Unstage or remove these paths; .gitignore alone does not untrack them."
exit 1

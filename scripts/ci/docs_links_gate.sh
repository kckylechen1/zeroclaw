#!/usr/bin/env bash

set -euo pipefail
unset CDPATH

cd "$(dirname "${BASH_SOURCE[0]}")/../.."

BASE_SHA="${BASE_SHA:-}"
DOCS_FILES_RAW="${DOCS_FILES:-}"

LINKS_FILE="$(mktemp)"
HTTP_LINKS_FILE="$(mktemp)"
trap 'rm -f "$LINKS_FILE" "$HTTP_LINKS_FILE"' EXIT

# --check-local-targets always checks the whole authored book, even with no
# changed Markdown: deletes/renames can break incoming links on untouched pages.
python3 ./scripts/ci/collect_changed_links.py \
    --base "$BASE_SHA" \
    --docs-files "$DOCS_FILES_RAW" \
    --output "$LINKS_FILE" \
    --http-output "$HTTP_LINKS_FILE" \
    --check-local-targets

echo "Offline docs gate passed: source/navigation/local targets only. HTTP(S) availability and rendered anchors are not checked."

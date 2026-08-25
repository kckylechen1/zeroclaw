#!/usr/bin/env bash

# Fixture tests for retired_docs_gate.sh: each planted teaching reference to
# a retired surface must fire with file/line/term diagnostics; each
# historical, exempted, or out-of-scope context must pass; and a broken
# registry must be FATAL rather than a silent pass.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
gate="${script_dir}/retired_docs_gate.sh"
real_registry="${script_dir}/retired_surfaces.json"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
cd "$tmp"
git init -q .

pass_count=0
fail_count=0

registry="${tmp}/fixture_registry.json"
cat >"$registry" <<'JSON'
{
  "retired": [
    {"term": "frob_tool", "kind": "tool", "retired_in": "PR 9001", "notes": "fixture tool"},
    {"term": "backup_frob", "kind": "tool", "retired_in": "PR 9001", "notes": "fixture ambiguous word",
     "match_regex": "`backup_frob`|\\bbackup_frob tool\\b"},
    {"term": "frob_env", "kind": "op", "retired_in": "PR 9001", "notes": "fixture op"},
    {"term": "frob_dashboard", "kind": "config-key", "retired_in": "PR 9001", "notes": "fixture key"},
    {"term": "ZEROCLAW_gateway__frob__", "kind": "env-prefix", "retired_in": "PR 9001", "notes": "fixture env"},
    {"term": "frob_gate", "kind": "config-key", "retired_in": "PR 9001", "notes": "fixture gated knob",
     "allowed_globs": ["docs/book/src/legacy-frob.md"]}
  ]
}
JSON

GATE_OUT=""
GATE_STATUS=0

run_gate() {
    set +e
    GATE_OUT="$(RETIRED_SURFACES_FILE="$registry" bash "$gate" 2>&1)"
    GATE_STATUS=$?
    set -e
}

plant() {
    mkdir -p "$(dirname "$1")"
    printf '%s\n' "$2" >"$1"
    git add "$1"
    run_gate
    git rm -qf "$1"
}

expect_fail() {
    local name="$1" want_term="$2" path="$3" content="$4"
    plant "$path" "$content"
    if [ "$GATE_STATUS" -ne 1 ]; then
        echo "NOT CAUGHT (wanted exit 1, got ${GATE_STATUS}): ${name}"
        fail_count=$((fail_count + 1))
    elif ! grep -qF "$want_term" <<<"$GATE_OUT" \
        || ! grep -qF "$path" <<<"$GATE_OUT"; then
        echo "DIAGNOSTIC INCOMPLETE: ${name} (wanted term '${want_term}' and path '${path}')"
        grep -E '^(FAIL|  )' <<<"$GATE_OUT" || true
        fail_count=$((fail_count + 1))
    else
        pass_count=$((pass_count + 1))
    fi
}

expect_pass() {
    local name="$1" path="$2" content="$3"
    plant "$path" "$content"
    if [ "$GATE_STATUS" -ne 0 ]; then
        echo "FALSE POSITIVE: ${name}"
        grep -E '^(FAIL|  )' <<<"$GATE_OUT" || true
        fail_count=$((fail_count + 1))
    else
        pass_count=$((pass_count + 1))
    fi
}

# ── teaching references must fire ───────────────────────────────────────

expect_fail "tool taught in guide docs" "frob_tool" "docs/book/src/guide.md" \
    $'Run the `frob_tool` tool to frob the widget.'

expect_fail "config section taught in a docs TOML template" "frob_dashboard" "docs/example.toml" \
    $'[gateway.frob_dashboard]\nenabled = true'

expect_fail "env override taught with key suffix (prefix match)" "ZEROCLAW_gateway__frob__" "docs/book/src/env.md" \
    'Set ZEROCLAW_gateway__frob__code_length = 9 before boot.'

expect_fail "deprecated knob taught as working behavior" "frob_gate" "README.md" \
    'Set `frob_gate` to require a code before each action.'

expect_fail "help text taught in English Fluent catalogue" "frob_tool" "crates/x/locales/en/cli.ftl" \
    'frob-help = Use frob_tool to frob.'

expect_fail "op action string taught" "frob_env" "docs/book/src/ops.md" \
    'Call the widget tool with action `frob_env` to apply.'

expect_fail "code-span form of an ambiguous term taught" "backup_frob" "docs/book/src/backup.md" \
    'Run `backup_frob` nightly to archive state.'

expect_fail "bare-word phrase form of an ambiguous term taught" "backup_frob" "docs/book/src/backup2.md" \
    'The backup_frob tool archives state nightly.'

# ── collision refinement: ordinary-word use of an ambiguous term ────────

expect_pass "bare-word use of ambiguous term outside its match forms" \
    "docs/book/src/archives.md" \
    'The backup_frob directory holds archives.'

# ── historical context must pass ────────────────────────────────────────

expect_pass "same-line deprecation wording" \
    "docs/book/src/notes.md" \
    'The frob_tool is deprecated and unsupported.'

expect_pass "removal wording within the 3-line window" \
    "docs/book/src/window.md" \
    $'The old surface was removed.\n\n\nUse frob_tool freely while migrating.'

expect_pass "retired section heading excuses the section body" \
    "docs/book/src/retired.md" \
    $'## Retired frob surface\n\n| frob_tool | the widget frobber owns this now. |'

expect_pass "section intro wording excuses later rows" \
    "docs/book/src/migration.md" \
    $'## Frob migration\n\nThese are no longer model tools.\n\n| `frob_tool` | see the widget frobber. |'

expect_pass "changelog file is exempt context" \
    "CHANGELOG-next.md" \
    'Added frob_tool. Run frob_tool to frob.'

# ── bounded exemptions and scope exclusions ─────────────────────────────

expect_pass "allowed_globs path exempted for the term" \
    "docs/book/src/legacy-frob.md" \
    'Use frob_gate today.'

expect_pass "test-fixture instruction files are out of scope" \
    "crates/x/tests/fixtures/SKILL.md" \
    'Run frob_tool now.'

expect_pass "non-English generated locales are out of scope" \
    "crates/x/locales/es/cli.ftl" \
    'frob-help = Usa frob_tool.'

expect_pass "source code is out of scope" \
    "crates/x/src/lib.rs" \
    'pub const TOOL: &str = "frob_tool";'

# ── registry integrity: broken registries are FATAL, never silent ──────

fatal_registry_case() {
    local name="$1" body="$2"
    local bad="${tmp}/bad_registry.json"
    printf "%s" "$body" >"$bad"
    local out status
    set +e
    out="$(RETIRED_SURFACES_FILE="$bad" bash "$gate" 2>&1)"
    status=$?
    set -e
    if [ "$status" -ne 2 ]; then
        echo "BROKEN REGISTRY NOT FATAL (wanted 2, got ${status}): ${name}"
        fail_count=$((fail_count + 1))
    elif ! grep -qF "FATAL" <<<"$out"; then
        echo "FATAL DIAGNOSTIC MISSING: ${name}"
        fail_count=$((fail_count + 1))
    else
        pass_count=$((pass_count + 1))
    fi
}

fatal_registry_case "invalid JSON" '{"retired": ['
fatal_registry_case "empty retired list" '{"retired": []}'
fatal_registry_case "unknown kind" '{"retired": [{"term": "x", "kind": "vibes", "retired_in": "PR 1", "notes": "n"}]}'
fatal_registry_case "missing notes" '{"retired": [{"term": "x", "kind": "tool", "retired_in": "PR 1"}]}'
fatal_registry_case "non-compiling match_regex" '{"retired": [{"term": "x", "kind": "tool", "retired_in": "PR 1", "notes": "n", "match_regex": "(unclosed"}]}'
fatal_registry_case "duplicate term" '{"retired": [{"term": "x", "kind": "tool", "retired_in": "PR 1", "notes": "n"}, {"term": "x", "kind": "op", "retired_in": "PR 1", "notes": "n"}]}'

set +e
missing_out="$(RETIRED_SURFACES_FILE="${tmp}/does-not-exist.json" bash "$gate" 2>&1)"
missing_status=$?
set -e
if [ "$missing_status" -ne 2 ] || ! grep -qF "FATAL" <<<"$missing_out"; then
    echo "MISSING REGISTRY NOT FATAL (status ${missing_status})"
    fail_count=$((fail_count + 1))
else
    pass_count=$((pass_count + 1))
fi

# ── fatal path: outside any Git work tree ───────────────────────────────

outside="$(mktemp -d)"
set +e
out="$(cd "$outside" && bash "$gate" 2>&1)"
status=$?
set -e
rm -rf "$outside"
if [ "$status" -ne 2 ]; then
    echo "NON-REPO FATAL NOT PROPAGATED (wanted 2, got ${status})"
    fail_count=$((fail_count + 1))
elif ! grep -qF "FATAL" <<<"$out"; then
    echo "FATAL DIAGNOSTIC MISSING ON NON-REPO"
    fail_count=$((fail_count + 1))
else
    pass_count=$((pass_count + 1))
fi

# ── real registry integrity: parses, scans clean, corpus seeded ─────────

set +e
out="$(RETIRED_SURFACES_FILE="$real_registry" bash "$gate" 2>&1)"
status=$?
set -e
if [ "$status" -ne 0 ]; then
    echo "REAL REGISTRY DOES NOT RUN CLEAN IN EMPTY REPO (status ${status})"
    echo "$out"
    fail_count=$((fail_count + 1))
else
    pass_count=$((pass_count + 1))
fi

seeded_count=0
for seeded in model_switch model_routing_config proxy_config security_ops backup data_management apply_env clear_env pairing_dashboard ZEROCLAW_gateway__pairing_dashboard__ gated_actions gated_domains gated_domain_categories challenge_max_attempts glm.rs "platform/wasm.rs"; do
    if ! grep -qF "\"term\": \"${seeded}\"" "$real_registry"; then
        echo "SEEDED TERM MISSING FROM REAL REGISTRY: ${seeded}"
        fail_count=$((fail_count + 1))
    else
        pass_count=$((pass_count + 1))
        seeded_count=$((seeded_count + 1))
    fi
done

echo
echo "${pass_count} passed, ${fail_count} failed (seeded corpus terms verified: ${seeded_count})"
[ "$fail_count" -eq 0 ]

#!/usr/bin/env bash

# Fixture tests for retired_docs_gate.sh: each planted teaching reference to
# a retired surface must fire with file/line/term diagnostics; each
# historical, exempted, or out-of-scope context must pass; a broken registry
# or missing python3 must be FATAL rather than a silent pass.

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

run_gate_with() {
    local reg="$1"
    set +e
    GATE_OUT="$(RETIRED_SURFACES_FILE="$reg" bash "$gate" 2>&1)"
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

plant_with() {
    local reg="$1" path="$2" content="$3"
    mkdir -p "$(dirname "$path")"
    printf '%s\n' "$content" >"$path"
    git add "$path"
    run_gate_with "$reg"
    git rm -qf "$path"
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

expect_fail "config section taught in a deploy TOML template outside docs" "frob_dashboard" "scripts/deploy.toml" \
    $'[gateway.frob_dashboard]\nenabled = true'

expect_fail "env override taught with key suffix (prefix match)" "ZEROCLAW_gateway__frob__" "docs/book/src/env.md" \
    'Set ZEROCLAW_gateway__frob__code_length = 9 before boot.'

expect_fail "deprecated knob taught as working behavior" "frob_gate" "README.md" \
    'Set `frob_gate` to require a code before each action.'

expect_fail "help text taught in English Fluent catalogue" "frob_tool" "crates/x/locales/en/cli.ftl" \
    'frob-help = Use frob_tool to frob.'

expect_fail "Fluent tool-description key form caught despite hyphens" "frob_tool" "crates/x/locales/en/tools.ftl" \
    'tool-frob-tool = Use this to frob the widget.'

expect_fail "Fluent tool key form caught for match_regex-refined term" "backup_frob" "crates/x/locales/en/tools2.ftl" \
    'tool-backup-frob = Archives state nightly.'

expect_fail "op action string taught" "frob_env" "docs/book/src/ops.md" \
    'Call the widget tool with action `frob_env` to apply.'

expect_fail "code-span form of an ambiguous term taught" "backup_frob" "docs/book/src/backup.md" \
    'Run `backup_frob` nightly to archive state.'

expect_fail "bare-word phrase form of an ambiguous term taught" "backup_frob" "docs/book/src/backup2.md" \
    'The backup_frob tool archives state nightly.'

# ── collision refinement and hyphen specificity ─────────────────────────

expect_pass "bare-word use of ambiguous term outside its match forms" \
    "docs/book/src/archives.md" \
    'The backup_frob directory holds archives.'

expect_pass "unrelated hyphenated Fluent key does not match" \
    "crates/x/locales/en/app.ftl" \
    'zc-frob-tool-applying = Applying frob change...'

expect_pass "command-path Fluent key does not match" \
    "crates/x/locales/en/chan.ftl" \
    'channel-runtime-frob-tool-hint = Switch with /frob command.'

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
    'tool-frob-tool = Usa esto para frob.'

expect_pass "source code is out of scope" \
    "crates/x/src/lib.rs" \
    'pub const TOOL: &str = "frob_tool";'

expect_pass "root tool-config TOML is out of scope" \
    "taplo.toml" \
    $'[gateway.frob_dashboard]\nenabled = true'

expect_pass "Cargo.toml anywhere is out of scope" \
    "demo/Cargo.toml" \
    '[gateway.frob_dashboard]'

# ── replacement teaching (rename contract) ──────────────────────────────

repl_registry="${tmp}/fixture_registry_repl.json"
cat >"$repl_registry" <<'JSON'
{
  "retired": [
    {"term": "frob_tool", "kind": "tool", "retired_in": "PR 9001",
     "notes": "fixture rename", "replacement": "widget-frobber"}
  ]
}
JSON

plant_with "$repl_registry" "docs/book/src/repl.md" \
    'The frob surface moved; see the operator docs.'
if [ "$GATE_STATUS" -ne 1 ] || ! grep -qF "no active doc teaches the replacement" <<<"$GATE_OUT"; then
    echo "RENAME WITHOUT REPLACEMENT TEACHING NOT CAUGHT (status ${GATE_STATUS})"
    grep -E '^(FAIL|  )' <<<"$GATE_OUT" || true
    fail_count=$((fail_count + 1))
else
    pass_count=$((pass_count + 1))
fi

plant_with "$repl_registry" "docs/book/src/repl.md" \
    'The frob surface moved; run `widget-frobber` instead.'
if [ "$GATE_STATUS" -ne 0 ]; then
    echo "REPLACEMENT TEACHING FALSE POSITIVE"
    grep -E '^(FAIL|  )' <<<"$GATE_OUT" || true
    fail_count=$((fail_count + 1))
else
    pass_count=$((pass_count + 1))
fi

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
fatal_registry_case "non-string kind" '{"retired": [{"term": "x", "kind": [], "retired_in": "PR 1", "notes": "n"}]}'
fatal_registry_case "non-string term" '{"retired": [{"term": [], "kind": "tool", "retired_in": "PR 1", "notes": "n"}]}'
fatal_registry_case "missing notes" '{"retired": [{"term": "x", "kind": "tool", "retired_in": "PR 1"}]}'
fatal_registry_case "non-compiling match_regex" '{"retired": [{"term": "x", "kind": "tool", "retired_in": "PR 1", "notes": "n", "match_regex": "(unclosed"}]}'
fatal_registry_case "duplicate term" '{"retired": [{"term": "x", "kind": "tool", "retired_in": "PR 1", "notes": "n"}, {"term": "x", "kind": "op", "retired_in": "PR 1", "notes": "n"}]}'
fatal_registry_case "blanket allowed_glob" '{"retired": [{"term": "x", "kind": "tool", "retired_in": "PR 1", "notes": "n", "allowed_globs": ["*"]}]}'
fatal_registry_case "pathless allowed_glob" '{"retired": [{"term": "x", "kind": "tool", "retired_in": "PR 1", "notes": "n", "allowed_globs": ["legacy-*.md"]}]}'
fatal_registry_case "empty replacement string" '{"retired": [{"term": "x", "kind": "tool", "retired_in": "PR 1", "notes": "n", "replacement": " "}]}'

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

# ── fatal path: python3 unavailable ─────────────────────────────────────

nopy_bin="${tmp}/nopybin"
mkdir -p "$nopy_bin"
ln -s "$(command -v bash)" "${nopy_bin}/bash"
ln -s "$(command -v git)" "${nopy_bin}/git"
ln -s "$(command -v dirname)" "${nopy_bin}/dirname"
set +e
out="$(PATH="$nopy_bin" bash "$gate" 2>&1)"
status=$?
set -e
if [ "$status" -ne 2 ] || ! grep -qF "FATAL" <<<"$out"; then
    echo "MISSING PYTHON3 NOT FATAL (status ${status})"
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

# ── real registry integrity: shape, kinds, patterns, seeded corpus ──────
# The real registry is validated structurally here (the gate validates it
# again on every run); a small canary list guards against accidental
# truncation of the seeded corpus.

real_check="${tmp}/real_check.out"
if python3 - "$real_registry" <<'PY' >"$real_check" 2>&1
import json
import re
import sys

with open(sys.argv[1], encoding="utf-8") as fh:
    doc = json.load(fh)
entries = doc["retired"]
kinds = {"tool", "op", "config-key", "env-prefix", "code-path"}
assert len(entries) >= 16, f"corpus shrank: {len(entries)} entries"
assert {e["kind"] for e in entries} == kinds, "kind coverage shrank"
for e in entries:
    assert e["kind"] in kinds, e
    for field in ("term", "retired_in", "notes"):
        assert isinstance(e[field], str) and e[field].strip(), (e["term"], field)
    if "match_regex" in e:
        re.compile(e["match_regex"])
    for g in e.get("allowed_globs", []):
        assert "/" in g and re.search(r"[^*?]", g), (e["term"], g)
    if "replacement" in e:
        assert e["replacement"].strip(), e["term"]
canary = {"model_switch", "pairing_dashboard", "glm.rs"}
terms = {e["term"] for e in entries}
assert canary <= terms, f"canary terms missing: {canary - terms}"
print("real registry structurally valid")
PY
then
    pass_count=$((pass_count + 1))
else
    echo "REAL REGISTRY INTEGRITY CHECK FAILED"
    cat "$real_check"
    fail_count=$((fail_count + 1))
fi

echo
echo "${pass_count} passed, ${fail_count} failed"
[ "$fail_count" -eq 0 ]

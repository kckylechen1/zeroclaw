#!/usr/bin/env bash

set -euo pipefail

if [[ "$(basename "$0")" == "cargo" ]]; then
    printf '%s\n' "$*" >> "${CARGO_STUB_LOG:?}"
    case "${CARGO_STUB_SCENARIO:?}" in
        pass)
            echo "test result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 100 filtered out; finished in 0.01s"
            ;;
        zero-selected)
            echo "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 103 filtered out; finished in 0.01s"
            ;;
        ignored-only)
            echo "test result: ok. 0 passed; 0 failed; 2 ignored; 0 measured; 101 filtered out; finished in 0.01s"
            ;;
        cargo-failure)
            echo "test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 101 filtered out; finished in 0.01s"
            exit 17
            ;;
        malformed-summary)
            echo "test result: ok, but fields are unavailable"
            ;;
        missing-summary)
            echo "Finished test profile in 0.01s"
            ;;
        reported-failed)
            echo "test result: ok. 1 passed; 1 failed; 0 ignored; 0 measured; 101 filtered out; finished in 0.01s"
            ;;
        *)
            echo "unexpected fixture scenario: ${CARGO_STUB_SCENARIO}" >&2
            exit 99
            ;;
    esac
    exit 0
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
gate="$repo_root/scripts/ci/optional_channel_test_gate.sh"
fixture_root="$(mktemp -d)"
trap 'rm -rf "$fixture_root"' EXIT
mkdir -p "$fixture_root/bin"
ln -s "$repo_root/scripts/ci/optional_channel_test_gate.test.sh" "$fixture_root/bin/cargo"

features="channel-lark,channel-matrix,channel-slack,channel-wechat,whatsapp-web,channel-mattermost,channel-wecom-ws"
filters=(
    "lark::tests::"
    "matrix::tests::"
    "slack::tests::"
    "wechat::tests::"
    "whatsapp_web::tests::"
    "mattermost::tests::"
    "wecom_ws::tests::"
)

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

run_gate() {
    local scenario="$1"
    local output="$fixture_root/$scenario.out"
    local calls="$fixture_root/$scenario.calls"
    local summary="$fixture_root/$scenario.summary"

    set +e
    PATH="$fixture_root/bin:$PATH" \
        CARGO_STUB_LOG="$calls" \
        CARGO_STUB_SCENARIO="$scenario" \
        GITHUB_STEP_SUMMARY="$summary" \
        "$gate" > "$output" 2>&1
    RUN_STATUS=$?
    set -e
    RUN_OUTPUT="$output"
    RUN_CALLS="$calls"
    RUN_SUMMARY="$summary"
}

expect_failure() {
    local scenario="$1"
    local message="$2"

    run_gate "$scenario"
    (( RUN_STATUS != 0 )) || fail "$scenario unexpectedly passed"
    grep -Fq -- "$message" "$RUN_OUTPUT" \
        || fail "$scenario did not report: $message"
}

run_gate pass
(( RUN_STATUS == 0 )) || fail "passing fixture exited $RUN_STATUS"
[[ "$(wc -l < "$RUN_CALLS" | tr -d ' ')" == "7" ]] \
    || fail "passing fixture did not invoke all seven suites"
[[ "$(grep -Fc -- "--features $features" "$RUN_CALLS")" == "7" ]] \
    || fail "feature union was not passed to every suite"
[[ "$(grep -Fc -- "selected=3 passed=2 ignored=1" "$RUN_OUTPUT")" == "7" ]] \
    || fail "positive per-suite counts were not reported seven times"

for filter in "${filters[@]}"; do
    [[ "$(grep -Fc -- "$filter" "$RUN_CALLS")" == "1" ]] \
        || fail "$filter was not invoked exactly once"
    grep -Fq -- "\`$filter\` | 3 | 2 | 1 |" "$RUN_SUMMARY" \
        || fail "$filter counts were not written to the step summary"
done

expect_failure zero-selected "selected no executable tests (0 ignored)"
expect_failure ignored-only "selected no executable tests (2 ignored)"

run_gate cargo-failure
[[ "$RUN_STATUS" == "17" ]] || fail "cargo failure status was not preserved"
grep -Fq -- "test command failed with status 17" "$RUN_OUTPUT" \
    || fail "cargo failure was not reported"

expect_failure malformed-summary "Could not parse the test summary"
expect_failure missing-summary "Could not parse the test summary"
expect_failure reported-failed "reported 1 failed tests despite cargo exiting successfully"

echo "optional channel test gate fixtures passed"

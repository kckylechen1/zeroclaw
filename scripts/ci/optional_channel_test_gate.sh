#!/usr/bin/env bash

set -euo pipefail

features="channel-lark,channel-matrix,channel-slack,channel-wechat,whatsapp-web,channel-mattermost,channel-wecom-ws"
suites=(
    "Lark|lark::tests::"
    "Matrix|matrix::tests::"
    "Slack|slack::tests::"
    "WeChat|wechat::tests::"
    "WhatsApp Web|whatsapp_web::tests::"
    "Mattermost|mattermost::tests::"
    "WeCom WebSocket|wecom_ws::tests::"
)

summary_file="${GITHUB_STEP_SUMMARY:-}"
scratch_dir="$(mktemp -d)"
trap 'rm -rf "$scratch_dir"' EXIT

if [[ -n "$summary_file" ]]; then
    {
        echo "### Optional channel unit suites"
        echo
        echo "Features: \`$features\`"
        echo
        echo "| Suite | Filter | Selected | Passed | Ignored |"
        echo "| --- | --- | ---: | ---: | ---: |"
    } >> "$summary_file"
fi

result_pattern='^test result: (ok|FAILED)\. ([0-9]+) passed; ([0-9]+) failed; ([0-9]+) ignored; ([0-9]+) measured; ([0-9]+) filtered out;'

for suite_entry in "${suites[@]}"; do
    IFS='|' read -r suite filter <<< "$suite_entry"
    log_file="$scratch_dir/${filter%%::*}.log"

    echo "==> optional channel suite: $suite ($filter)"
    set +e
    CARGO_TERM_COLOR=never cargo test --locked --quiet \
        -p zeroclaw-channels \
        --lib \
        --features "$features" \
        "$filter" \
        -- --quiet 2>&1 | tee "$log_file"
    cargo_status=${PIPESTATUS[0]}
    set -e

    result_line="$(grep '^test result:' "$log_file" | tail -n 1 || true)"
    if [[ ! "$result_line" =~ $result_pattern ]]; then
        echo "::error::Could not parse the test summary for $suite ($filter)"
        exit 1
    fi

    passed="${BASH_REMATCH[2]}"
    failed="${BASH_REMATCH[3]}"
    ignored="${BASH_REMATCH[4]}"
    measured="${BASH_REMATCH[5]}"
    selected=$((passed + failed + ignored + measured))
    executed=$((passed + failed + measured))

    echo "==> $suite: selected=$selected passed=$passed ignored=$ignored"
    if [[ -n "$summary_file" ]]; then
        echo "| $suite | \`$filter\` | $selected | $passed | $ignored |" >> "$summary_file"
    fi

    if (( cargo_status != 0 )); then
        echo "::error::$suite test command failed with status $cargo_status"
        exit "$cargo_status"
    fi
    if (( failed != 0 )); then
        echo "::error::$suite reported $failed failed tests despite cargo exiting successfully"
        exit 1
    fi
    if (( executed == 0 )); then
        echo "::error::$suite selected no executable tests ($ignored ignored)"
        exit 1
    fi
done

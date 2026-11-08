#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -ne 2 ]]; then
  echo "usage: $0 <url> <output-file>" >&2
  exit 2
fi

# Three attempts bound network time to 184 seconds including retry delays.
# CI fixtures may shorten, but cannot disable or widen, the per-attempt limit.
download_max_time="${CI_DOWNLOAD_MAX_TIME_SECONDS:-60}"
if [[ ! "$download_max_time" =~ ^([1-9]|[1-5][0-9]|60)$ ]]; then
  echo "CI_DOWNLOAD_MAX_TIME_SECONDS must be an integer from 1 through 60" >&2
  exit 2
fi

download_url="$1"
download_output="$2"
download_temp=""
trap 'if [[ -n "$download_temp" ]]; then rm -f -- "$download_temp"; fi' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

for download_attempt in 1 2 3; do
  # Never retry into stdout or reuse a partially written archive. Keep the
  # previous destination intact until a complete transfer can replace it.
  download_temp="$(mktemp "${download_output}.partial.XXXXXX")"
  if curl --disable --fail --silent --show-error --location \
    --connect-timeout 10 --max-time "$download_max_time" \
    --output "$download_temp" -- "$download_url"; then
    mv -f -- "$download_temp" "$download_output"
    exit 0
  else
    download_status=$?
  fi
  rm -f -- "$download_temp"
  download_temp=""
  if [[ "$download_attempt" -lt 3 ]]; then
    echo "CI tool download attempt $download_attempt/3 failed (curl $download_status); retrying in 2 seconds" >&2
    sleep 2
  fi
done

echo "CI tool download failed after 3 attempts (curl $download_status)" >&2
exit "$download_status"

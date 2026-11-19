#!/bin/sh
# Opt-in live smoke test. The header file contains a downstream key, never a Home secret.
set -eu
umask 077

if [ "$#" -ne 3 ]; then
    echo 'Usage: sh scripts/probe-worker.sh WORKER_ORIGIN AUTH_HEADER_FILE CHAT_MODEL' >&2
    echo 'Makes two billable Chat requests (JSON and SSE); see docs/worker-acceptance.md.' >&2
    exit 2
fi
origin=${1%/}
headers=$2
model=$3
case "$origin" in
    https://*|http://127.0.0.1:*|http://localhost:*) ;;
    *) echo 'Use HTTPS or an explicit loopback HTTP port; tunnel remote plaintext listeners.' >&2; exit 2 ;;
esac
case "$origin" in
    *'@'*|*'?'*|*'#'*) echo 'Origin must not contain credentials, query, or fragment.' >&2; exit 2 ;;
esac
[ -r "$headers" ] || { echo 'Auth header file is not readable.' >&2; exit 2; }
[ -n "$model" ] || { echo 'CHAT_MODEL must not be empty.' >&2; exit 2; }
command -v curl >/dev/null
command -v jq >/dev/null
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

request() {
    expected=$1
    output=$2
    shift 2
    status=$(curl --silent --show-error --connect-timeout 10 --max-time 120 \
        --output "$output" --write-out '%{http_code}' "$@")
    if [ "$status" != "$expected" ]; then
        printf 'Expected HTTP %s, got %s; response withheld (may contain sensitive data).\n' "$expected" "$status" >&2
        exit 1
    fi
}
request 200 "$work/health.json" "$origin/healthz"
jq -e '.status == "ready"' "$work/health.json" >/dev/null
request 200 "$work/models.json" --header "@$headers" "$origin/v1/models"
jq -e '.object == "list" and (.data | type == "array")' "$work/models.json" >/dev/null
jq -e --arg model "$model" 'any(.data[]; .id == $model)' "$work/models.json" >/dev/null

jq -n --arg model "$model" \
    '{model: $model, messages: [{role: "user", content: "Reply with the word OK."}], stream: false}' > "$work/request.json"
request 200 "$work/chat.json" --header "@$headers" --header 'Content-Type: application/json' \
    --data-binary "@$work/request.json" "$origin/v1/chat/completions"
jq -e '(.error == null) and (.choices | type == "array" and length > 0)' "$work/chat.json" >/dev/null
jq '.stream = true' "$work/request.json" > "$work/stream.json"
request 200 "$work/chat.sse" --no-buffer --header "@$headers" --header 'Content-Type: application/json' \
    --data-binary "@$work/stream.json" "$origin/v1/chat/completions"
# Validate each data record, not just HTTP 200 (SSE can fail after headers).
tr -d '\r' < "$work/chat.sse" > "$work/normalized.sse"
grep -q '^data: \[DONE\]$' "$work/normalized.sse"
sed -n 's/^data: //p' "$work/normalized.sse" | sed '/^\[DONE\]$/d' > "$work/chunks.jsonl"
jq -se 'length > 0 and all(.[]; .error == null and (.choices | type == "array"))' "$work/chunks.jsonl" >/dev/null
printf '%s\n' 'HTTP smoke passed: readiness, authenticated catalog, Chat JSON and SSE.' \
    'This does not verify Home usage, releases, snapshots, recovery, or provider parity.'

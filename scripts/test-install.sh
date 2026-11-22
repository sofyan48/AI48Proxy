#!/usr/bin/env bash
# Offline installer checks. No Docker daemon or running Home is required.
set -Eeuo pipefail
ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
# Load installer functions without dispatching a deployment action.
sed '/^case "$ACTION" in/,$d' "$ROOT/install.sh" > "$TMP/functions.sh"
source "$TMP/functions.sh"
ENV_FILE="$TMP/env"
printf 'MANAGEMENT_KEY=synthetic-test-key\n' > "$ENV_FILE"
STATE="$TMP/state"
CALLS="$TMP/calls"
MODE=normal
curl() {
  printf '%s\n' "$*" >> "$CALLS"
  case "${!#}" in
    */v8/management/plugins)
      [[ "$MODE" != read-failure ]] || return 22
      if [[ "$MODE" == invalid ]]; then
        printf '{"plugins_enabled":"unknown"}\n'
      else
        printf '{"plugins_enabled":%s}\n' "$(cat "$STATE")"
      fi
      ;;
    */v8/management/config)
      [[ "$*" == *'-X PATCH'* ]] || return 1
      [[ "$*" == *'{"plugins":{"enabled":false}}'* ]] || return 1
      [[ "$MODE" != patch-failure ]] || return 22
      if [[ "$MODE" != unchanged ]]; then printf 'false\n' > "$STATE"; fi
      printf '{"status":"ok"}\n'
      ;;
    *) return 1 ;;
  esac
}
reset() {
  MODE="$1"
  printf '%s\n' "$2" > "$STATE"
  : > "$CALLS"
}
reset normal false
ensure_plugins_disabled
! grep -q -- '-X PATCH' "$CALLS"
reset normal true
ensure_plugins_disabled
[[ "$(cat "$STATE")" == false ]]
[[ "$(grep -c '/v8/management/plugins' "$CALLS")" == 2 ]]
grep -q -- '-X PATCH' "$CALLS"
for mode in read-failure invalid patch-failure unchanged; do
  reset "$mode" true
  # Execute independently: failures must terminate the installer under errexit.
  export MODE STATE CALLS ENV_FILE
  export -f curl value fail log ensure_plugins_disabled
  if bash -e -o pipefail -c 'ensure_plugins_disabled' > "$TMP/output" 2>&1; then
    printf 'FAIL: installer accepted %s\n' "$mode" >&2
    exit 1
  fi
done
# New installations must explicitly disable plugins in their import document.
ROOT="$TMP"
ENV_FILE="$TMP/.env"
printf 'POSTGRES_USER=test\nPOSTGRES_PASSWORD=test\nPOSTGRES_DB=test\nMANAGEMENT_KEY=test\nGATEWAY_API_KEY=test\nAI48PROXY_IMAGE=test\n' > "$ENV_FILE"
(cd "$TMP"; initialize)
grep -A 1 '^plugins:$' "$TMP/deploy/config.yaml" | grep -q 'enabled: false'
printf 'Installer plugin checks passed.\n'

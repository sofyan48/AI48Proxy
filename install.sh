#!/usr/bin/env bash
set -Eeuo pipefail
umask 077
ROOT="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
cd "$ROOT"
ENV_FILE="$ROOT/.env"
ACTION="${1:-deploy}"
log() { printf '%s\n' "$*"; }
fail() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
require() { command -v "$1" >/dev/null || fail "Missing command: $1"; }
compose() { docker compose --env-file "$ENV_FILE" -f "$ROOT/docker-compose.yml" "$@"; }
value() { awk -v key="$1" 'index($0,key "=")==1 {v=substr($0,length(key)+2)} END {print v}' "$ENV_FILE"; }
set_value() {
  local key="$1" val="$2" tmp
  tmp="$(mktemp "$ROOT/.env.tmp.XXXXXX")"
  awk -v key="$key" -v val="$val" 'BEGIN {found=0} index($0,key "=")==1 {print key "=" val;found=1;next} {print} END {if(!found) print key "=" val}' "$ENV_FILE" > "$tmp"
  chmod 600 "$tmp"
  mv "$tmp" "$ENV_FILE"
}
initialize() {
  require openssl
  mkdir -p deploy
  if [[ ! -e "$ENV_FILE" ]]; then
    local pg management gateway
    pg="$(openssl rand -hex 32)"
    management="$(openssl rand -hex 32)"
    gateway="$(openssl rand -hex 32)"
    cat > "$ENV_FILE" <<EOF
# Generated locally; do not commit or share this file.
POSTGRES_USER=cliproxy
POSTGRES_PASSWORD=$pg
POSTGRES_DB=cliproxy_home
HOME_IMAGE=ghcr.io/sofyan48/ai48home:latest
AI48PROXY_IMAGE=ai48proxy:local
RUST_IMAGE=rust:bookworm
MANAGEMENT_KEY=$management
GATEWAY_API_KEY=$gateway
HOME_JWT_1=
HOME_JWT_2=
HOME_JWT_3=
RUST_LOG=ai48proxy=info
WORKER_START_DELAY=25
EOF
    log 'Created .env with unique local database, management and gateway secrets.'
  else
    log 'Preserving existing .env.'
  fi
  chmod 600 "$ENV_FILE"
  [[ -n "$(value POSTGRES_PASSWORD)" && -n "$(value MANAGEMENT_KEY)" && -n "$(value GATEWAY_API_KEY)" ]] || fail 'Required secrets missing in .env.'
  if [[ ! -e deploy/cluster.yaml ]]; then
    cat > deploy/cluster.yaml <<EOF
pgsql:
  host: postgres
  port: 5432
  user: "$(value POSTGRES_USER)"
  password: "$(value POSTGRES_PASSWORD)"
  database: "$(value POSTGRES_DB)"
  sslmode: disable
node:
  external-ip: home
  external-port: 8327
  port: 8327
  heartbeat-interval: 5s
  heartbeat-timeout: 20s
  event-poll-interval: 3s
EOF
  fi
  if [[ ! -e deploy/config.yaml ]]; then
    cat > deploy/config.yaml <<EOF
host: ""
port: 8317
allow-host: []
remote-management:
  allow-remote: true
  secret-key: "$(value MANAGEMENT_KEY)"
  disable-control-panel: false
auth-dir: "~/.cli-proxy-api"
api-keys:
  - "$(value GATEWAY_API_KEY)"
EOF
  fi
  chmod 600 deploy/cluster.yaml deploy/config.yaml
}
wait_home() {
  local attempt
  for attempt in {1..90}; do
    if curl -fsS --connect-timeout 2 --max-time 5 \
      -H "X-MANAGEMENT-KEY: $(value MANAGEMENT_KEY)" \
      http://127.0.0.1:8327/v8/management/nodes >/dev/null 2>&1; then return 0; fi
    sleep 2
  done
  fail 'Home did not become ready. Inspect: docker compose logs home'
}
provision_workers() {
  local i token existing seen='|'
  for i in 1 2 3; do
    existing="$(value "HOME_JWT_$i")"
    if [[ -z "$existing" ]]; then
      token="$(curl -fsS --connect-timeout 5 --max-time 30 -X POST \
        -H "X-MANAGEMENT-KEY: $(value MANAGEMENT_KEY)" \
        -H 'Content-Type: application/json' \
        --data "{\"node_name\":\"ai48proxy-$i\"}" \
        http://127.0.0.1:8327/v8/management/certificates/clients | jq -er '.home_jwt | select(type == "string" and length > 0)')"
      [[ "$token" =~ ^[A-Za-z0-9_.=-]+$ ]] || fail 'Invalid enrollment token response.'
      set_value "HOME_JWT_$i" "$token"
      existing="$token"
      log "Provisioned unique Home identity for worker $i."
    fi
    [[ "$seen" != *"|$existing|"* ]] || fail 'Worker JWTs must be distinct.'
    seen+="$existing|"
  done
}
case "$ACTION" in
  init) initialize ;;
  deploy)
    for cmd in docker curl jq openssl awk; do require "$cmd"; done
    docker compose version >/dev/null
    initialize
    compose config --quiet
    if [[ ! -e deploy/.home-imported ]]; then
      [[ ! -d home-data || -z "$(ls -A home-data)" ]] || fail 'Existing Home data without bootstrap marker: refusing automatic import. Inspect/migrate it explicitly.'
      [[ ! -d postgres-data || -z "$(ls -A postgres-data)" ]] || fail 'Existing PostgreSQL data without bootstrap marker: refusing automatic import. Inspect/migrate it explicitly.'
    fi
    compose build cliproxy-1
    compose up -d --wait --wait-timeout 180 postgres
    if [[ ! -e deploy/.home-imported ]]; then
      # Import once into a new database, before starting the persistent Home process.
      # Never import over an existing stack. Use this installer in a fresh directory.
      compose run --rm --no-deps -v "$ROOT/deploy/config.yaml:/AI48Home/config.yaml:ro" home ./AI48Home -import
      touch deploy/.home-imported
    fi
    compose up -d home
    wait_home
    provision_workers
    compose up -d --wait --wait-timeout 240 cliproxy-1 cliproxy-2 cliproxy-3
    compose up -d nginx
    compose ps
    log 'Stack started: Home :8327, gateway/nginx :8317. Secrets remain in .env.'
    log 'Check worker readiness: docker compose ps; docker compose logs cliproxy-1'
    ;;
  start)
    [[ -f "$ENV_FILE" && -e deploy/.home-imported ]] || fail 'Run install.sh deploy first.'
    for i in 1 2 3; do [[ -n "$(value "HOME_JWT_$i")" ]] || fail 'Worker JWT missing: finish install.sh deploy first.'; done
    compose up -d --wait --wait-timeout 180 postgres
    compose up -d home
    wait_home
    compose up -d --wait --wait-timeout 240 cliproxy-1 cliproxy-2 cliproxy-3
    compose up -d nginx
    ;;
  stop) compose stop nginx cliproxy-1 cliproxy-2 cliproxy-3; compose stop home postgres ;;
  status) compose ps ;;
  logs) compose logs --tail=100 "${2:-cliproxy-1}" ;;
  *) fail 'Usage: bash install.sh {init|deploy|start|stop|status|logs [service]}' ;;
esac

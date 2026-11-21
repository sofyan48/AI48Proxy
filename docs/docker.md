# Docker deployment

Deploy AI48Home and AI48Proxy together using Docker Engine and the Compose
plugin. This repository's Dockerfile builds the Rust worker; Compose supplies the
control plane, database and API ingress.

| Service | Image / role | Published port |
| --- | --- | --- |
| `postgres` | `postgres:17-alpine`, Home database | None |
| `ai48home` | `HOME_IMAGE`, alias `ai48home` | `8327:8327` |
| `ai48proxy-1` | `AI48PROXY_IMAGE`, worker 1 | None |
| `ai48proxy-2` | `AI48PROXY_IMAGE`, worker 2 | None |
| `ai48proxy-3` | `AI48PROXY_IMAGE`, worker 3 | None |
| `nginx` | `nginx:alpine`, least-connections worker ingress | `8317:8317` |

All services join the explicitly named `ai48proxy-network`. nginx uses
`ai48proxy-1:8317`, `ai48proxy-2:8317`, and `ai48proxy-3:8317`, with streaming and
WebSocket forwarding enabled.
Every service uses `restart: unless-stopped`.

## Deploy

Stop the old stack before deployment: the Home installer and this stack publish
the same ports `8327`/`8317`. New service/container/network names use AI48Home and
AI48Proxy branding. Do not run both stacks side by side. Back up existing data
before migration; stopping the old stack does not migrate its database or identities.

For a deployment created before the naming change, stop its containers using the
old Compose file before starting the renamed services. Do not remove volumes or
change database credentials in the existing `.env`: PostgreSQL's user/database
names are not migrated by renaming Compose services. Existing `.env`, generated
configuration, bind-mount data paths and worker volume keys remain unchanged.
The `home` DNS alias is retained for existing cluster configuration and issued
worker identities; new cluster configuration advertises `ai48home`.

From the AI48Proxy root:

```sh
./install.sh deploy
```

The installer generates the root `.env`, `deploy/cluster.yaml`, and
`deploy/config.yaml`, starts PostgreSQL, imports the initial Home configuration,
pulls the published worker image, starts Home, issues three distinct worker JWTs
into `.env`, and starts the workers and nginx. No manually provisioned secret files are required.
Do not start the entire stack before this bootstrap sequence completes.

### Installer environment and artifact contract

| Variable in `.env` | Purpose |
| --- | --- |
| `POSTGRES_USER` | Database user; installer default `ai48home` |
| `POSTGRES_PASSWORD` | Generated database password |
| `POSTGRES_DB` | Database name; installer default `ai48home` |
| `HOME_IMAGE` | Home image; Compose default `ghcr.io/sofyan48/ai48home:latest` |
| `AI48PROXY_IMAGE` | Worker image; Compose default `ghcr.io/sofyan48/ai48proxy:latest` |
| `MANAGEMENT_KEY` | Generated management key written to `deploy/config.yaml` |
| `GATEWAY_API_KEY` | Generated downstream API key written to `deploy/config.yaml` |
| `HOME_JWT_1` / `HOME_JWT_2` / `HOME_JWT_3` | Distinct Home-issued bootstrap JWTs |

Compose interpolates variables from `.env`; it does not inject that entire file
into containers. Each worker receives only its own JWT as `HOME_JWT`. The
management and gateway keys are imported through Home's configuration, not sent
to workers as environment variables. Keep `.env` and the generated YAML files
private (mode `0600`) and out of version control. Do not print resolved Compose
configuration: it includes passwords and JWTs. Docker administrators can inspect
container environment values.

`deploy/cluster.yaml` must point PostgreSQL at `postgres:5432`, with credentials
matching `.env`. New configuration advertises node address `ai48home:8327`, with
listener port `8327`; the `home` alias remains available to existing deployments. `deploy/config.yaml` sets worker `host: ""` and `port: 8317`,
`remote-management.secret-key` from `MANAGEMENT_KEY`, and `api-keys` from
`GATEWAY_API_KEY`.

Home mounts `deploy/cluster.yaml` read-only at `/AI48Home/cluster.yaml`. The
initial import loads generated configuration into Home's database. For reference,
the operation uses the following container command; normally let the installer
perform it once rather than invoking it manually:

```sh
docker compose --env-file .env up -d postgres
docker compose --env-file .env run --rm --no-deps \
  -v "$PWD/deploy/config.yaml:/AI48Home/config.yaml:ro" \
  ai48home ./AI48Home -import
```

The installer owns import-once tracking and token issuance. It refuses initial import when PostgreSQL or Home data already exists without its bootstrap marker. Deployment and `start` wait for PostgreSQL, Home, then all three worker healthchecks before starting nginx. Do not repeat imports
on an existing database as a routine restart operation. JWT fields may be empty
while the installer starts/imports Home; all three must be populated and distinct
before workers start. Missing PostgreSQL variables fail Compose interpolation.

## Publish the worker image

Release tags matching `v*` publish `ghcr.io/sofyan48/ai48proxy:<tag>` and
`:latest` for `linux/amd64` and `linux/arm64` through
`.github/workflows/image.yml`. Each architecture builds on a native runner, then
both images are combined into a multi-architecture manifest. Architecture-specific
image tags use the `-amd64` and `-arm64` suffixes. A manual workflow dispatch
publishes the provided tag (default `edge`) without changing `:latest`.

For a local release or emergency image publication, authenticate to GHCR first:

```sh
docker login ghcr.io
IMAGE=ghcr.io/sofyan48/ai48proxy ./scripts/publish-image.sh v0.1.0
```

The script uses Buildx and pushes provenance/SBOM metadata. Do not put a PAT in
the repository or in `.env`; use a short-lived login or CI `GITHUB_TOKEN`.

## Binary releases and CI

`.github/workflows/release.yaml` runs on tag pushes, or manually with an existing
release tag. It checks out that tag, prepares release notes and a draft for new
releases, then builds native binaries for Linux, macOS, and Windows on amd64 and
arm64 runners. Only after all six builds succeed does it upload the archives and
`checksums.txt` and publish the release. Manual reruns replace matching assets.

Archives are named `AI48Proxy_<version>_<os>_<arch>.tar.gz` (`.zip` on Windows),
with the leading `v` removed from the version. OS names are `linux`, `darwin`, and
`windows`; architecture names are `amd64` and `aarch64`. Each archive contains
`ai48proxy` (`ai48proxy.exe` on Windows), `README.md`, and `LICENSE`. Linux binaries
use the hosted Ubuntu runner's glibc; they are not musl/static compatibility builds.

`.github/workflows/worker.yml` runs checks only for pushes to branches other than
`main` and pull requests targeting `main`. Pushes to `main`, tag pushes, pull
requests targeting other branches, and manual runs do not trigger worker checks.

## Operate an initialized stack

After a successful `./install.sh deploy`:

```sh
docker compose --env-file .env config --quiet
./install.sh start
docker compose --env-file .env ps
docker compose --env-file .env logs --tail 100 ai48home ai48proxy-1 ai48proxy-2 ai48proxy-3 nginx
```

Home management is on port `8327`; downstream API requests go through nginx on
port `8317` using the generated gateway API key. Restrict exposed ports with host
firewall rules and protected ingress. Initial worker enrollment needs a trusted
network path; subsequent Home communication uses mTLS.

Worker healthchecks call `/usr/local/bin/ai48proxy --healthcheck
http://127.0.0.1:8317/healthz`. The binary must support that CLI mode. Health status
is readiness feedback; nginx uses passive upstream failure handling rather than
reading Docker health status. A successful Compose config
check does not validate enrollment, provider behavior, or billing.

## Persistent state

Persistent host paths are `postgres-data/`, `home-data/`,
`home-logs/`, and `home-plugins/`. Each Rust worker instead has its own named
volume (`worker-1-state`, `worker-2-state`, `worker-3-state`, project-prefixed by
Compose) mounted at `/var/lib/ai48proxy`, with identity below `identity/`.
The image creates that directory owned by UID/GID `10001`; workers run as
`10001:10001`. Never share a worker volume or run two workers with the same JWT.

```sh
./install.sh stop
./install.sh start
```

Keep JWTs and identity volumes unchanged for normal restarts. Do not delete locks
or rotate bootstrap tokens to bypass membership errors; investigate Home state
and outstanding reports first. Do not use `down --volumes` for routine recovery:
it destroys worker identities and durable reporting state.

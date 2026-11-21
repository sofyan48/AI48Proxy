# Regression results — 2026-11-21

## Scope

Current source regression was run as separate targeted groups, not with
`cargo test --all-targets`. No source changes, server restarts, configuration
mutations, or billable provider requests were performed during this run.

| Test target/filter | Passed | Failed | Ignored |
| --- | ---: | ---: | ---: |
| `--test cpa_contract` | 13 | 0 | 0 |
| `--test zed_contract` | 10 | 0 | 0 |
| `--lib config::` | 52 | 0 | 1 |
| `--lib providers::` | 130 | 0 | 0 |
| `--lib gateway::` | 180 | 0 | 0 |
| `--lib home::` | 45 | 0 | 0 |
| `--lib reporting::` | 26 | 0 | 0 |
| `--lib protocol::` | 35 | 0 | 0 |
| `--lib plugins::` | 5 | 0 | 0 |
| `--lib execution::` | 8 | 0 | 0 |
| `--lib payload::` | 13 | 0 | 0 |
| `--lib observability` | 4 | 0 | 0 |
| `--lib thinking::` | 4 | 0 | 0 |
| `--lib live_home_config_projection -- --ignored` | 1 | 0 | 0 |

All commands used `cargo test --locked`. Filters may overlap; these counts are
not a count of unique tests. The ignored config test was explicitly executed in
the last row against the real sibling AI48Home sanitizer through its Go overlay.
`cargo fmt --all --check`, `cargo check --locked --lib`, and `git diff --check`
also passed.

## Running deployment smoke checks

- `http://localhost:8317/healthz`: HTTP 200, `status: ready`, `home_managed: true`.
- `http://localhost:8317/v1/models` without credentials: HTTP 401.
- Chat preflight from `http://localhost:8327`: HTTP 204, allowing the origin,
  POST, and authorization/content-type headers.
- Home root at `http://localhost:8327/`: HTTP 200.
- Live containers: `ai48proxy-dev`, `ai48home-dev`, `ai48home-dev-postgres`.
- Worker uses a workspace bind mount and cargo-watch. Container checksums for
  Cargo.lock, ws_recovery.rs, and codex_catalog.rs matched host source. Its binary
  timestamp was newer than those sampled source files; this is not a complete
  reproducible-binary provenance check.

## Coverage boundary

Gateway tests include local socket/HTTP fixtures for WS compression, pipelined
response ownership, recovery, fallback restoration, multi-agent rewrites, and
bootstrap retry classification. Catalog tests use local fixtures for refresh and
cancellation. These do not prove live upstream entitlement or every provider's
current wire behavior.

Authenticated model listing, billable Chat/Responses JSON/SSE, live tool calls,
forced live WS disconnect/recovery, and verification of their persisted usage in
Home remain untested. No verified API-header file was available, and no previously
shared chat token was reused. Plugin tests cover the implemented plugin foundation,
not full native plugin execution parity.

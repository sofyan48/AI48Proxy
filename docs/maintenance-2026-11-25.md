# Upstream maintenance review — 2026-11-25

## Scope and ancestry

Fork baseline: `1bb9115b` on `sofyan48/AI48Proxy:main`.
Upstream inspected: `router-for-me/CLIProxyAPI` default `main`, tip
`85225381d89915595598ed13766cd79890c03a84`.

The Rust fork and Go upstream have independent root commits. Git ancestry and
patch IDs therefore cannot establish semantic coverage: `git cherry` reports
4,242 unmatched non-merge commits, not 4,242 proven missing features. Do not merge
unrelated histories or copy the Go tree into this Home-managed Rust worker.

This is a **partial** adaptation of upstream `d2b7dbbdbc9ceeee0312b08c48867ec2789fde30`
(missing Claude tool input). It does not claim coverage of the upstream tip.
The shared Claude call adapter now emits the JSON object string `"{}"` for omitted
`input`. Explicit null, string, array and boolean values remain invalid. Existing
object arguments and tool-result ownership checks remain intact. This applies
only to existing supported translation paths; it does not expand Codex source
format support or relax unsupported-feature gates.

## Features preserved and conflict handling

No textual merge was attempted because the histories and languages differ.
Adapted the upstream improvement to the existing strict Rust translation boundary
without replacing the fork architecture. Preserved AI48 branding, Home-selected
credentials, mTLS/RESP lifecycle, database-backed control-plane ownership,
accounting, plugin startup guards, payload-rule ordering, existing API routes and
fail-closed validation. No Home or UI API shape changes are introduced.

## Database

No worker schema migration is involved. Home `origin/main` schema version is 29;
upstream Home schema version is 20. Home stays **29 -> 29**. No migration was
renumbered, removed, added or executed. Production databases were not accessed.

## Pending upstream review

- `85225381`: credential-scoped signature cache; fork docs explicitly exclude full
  signed reasoning replay. Do not infer that enabled config implies cache parity.
- `bffe6e7e`: credential-replacement transport pool isolation needs a Rust-specific
  pool identity audit, not a copied Go implementation.
- `e049c336`: Imagen predict normalization; specialized Vertex Imagen operations
  are explicitly outside the worker's current supported matrix.
- Recent Gemini/Antigravity terminal, filter-error, tool and thinking fixes require
  path-specific regression review. This PR does not mark those fixes as covered.

## Verification

Added a regression for omitted input producing string-valued `{}`, retained object
arguments, rejection of explicit invalid input and rejection of unmatched tool
results.

- `cargo test --locked --all-targets`: 525 passed, 0 failed, 1 ignored.
- `cargo fmt --all --check`: passed after installing rustfmt.
- `git diff --check`: passed.
- `cargo clippy --locked --all-targets -- -D warnings`: failed under Rust 1.100.0
  in unchanged `codex_catalog.rs`, `ws_recovery.rs`, `websocket.rs`,
  `codex_multiagent_tests.rs` and `gateway_client_tests.rs` (six lint errors).
  These unrelated baseline issues were not silenced or refactored. PR must remain
  draft until required verification succeeds.

Fixtures are synthetic; no live provider, deployment, service restart or production
migration is authorized.

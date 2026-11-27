# Upstream maintenance — 2026-11-28

## Scope
Fork baseline `1e1b2b67ccb17de53f4da95e199d2f5497f48db7`. Upstream default
main inspected `dbe641f286a489ecb39e3c041c554371e6add864`; previous review tip
`85225381d89915595598ed13766cd79890c03a84`. Partial semantic adaptation of
`54ed2ba2016642abd897456f48d84b9f0fee6632` (empty/null schema enum values).
Previous PR #1 is verified merged; no open maintenance PR existed.
The Rust fork and Go upstream have independent roots: 4,420 patch-unmatched
non-merge commits are not 4,420 proven missing features. Do not merge unrelated
histories or declare the inspected upstream tip covered.

## Combined behavior and retained fork additions
Adapt the upstream fix to the existing strict Rust schema cleaner. Filter blank,
whitespace-only and null enum members before serializing Gemini declarations;
preserve empty-string/null intent as advisory descriptions and native Antigravity
nullable state. All-stripped enums are omitted rather than emitting an empty
restriction. Existing numeric/boolean conversion and declared Antigravity types
remain. Compound enums/unsupported references still fail closed. Nested property
names, original descriptions and cleaner idempotence are regression-tested.
No wholesale source replacement or textual merge conflict. The semantic conflict
between upstream's broad Go normalizer and this focused Rust subset is resolved
without widening supported composition or signed-replay claims.

AI48 branding, Home-owned scheduling/credentials/database-backed runtime,
mTLS/RESP, accounting, auth/security, plugin-disable guard, request payload rules
and all Home/UI routes/payloads are unchanged. This affects declarations, never
historical function argument values.

## Database
Home fork schema **31 -> 31**, upstream Home **21**. No schema/migration change,
version bump, production access or migration execution in this worker change.

## Verification
Rust 1.100.0, locked dependencies. Separate exact-origin baseline and maintenance
worktrees with identical toolchain/environment; logs outside repository source.
Focused new `cargo test --locked --all-targets enum_regressions`: 3 passed.
Maintenance full `cargo test --locked --all-targets`: 528 passed, 0 failed,
1 ignored (493 library + 6 binary + 6 signing + 13 CPA + 10 Zed).
`cargo fmt --all --check` and `git diff --check`: passed.
Strict `cargo clippy --locked --all-targets -- -D warnings`: failed six lints in
unchanged codex_catalog.rs, ws_recovery.rs, websocket.rs, codex_multiagent_tests.rs
and gateway_client_tests.rs. Independent baseline target directory rerun:
`cargo test --locked --all-targets`: 525 passed, 0 failed, 1 ignored;
`cargo clippy --locked --all-targets -- -D warnings`: same six errors at the
same unchanged source locations, exit 101. Initial shared-target baseline run
reused the maintenance binary (528 tests), so it is NOT baseline evidence; the
independent rebuild above is authoritative. Baseline fmt also passed.
No release/container build, live provider test, deployment, restart or production
DB write. Keep PR draft while required gate fails.

## Remaining upstream review
Watcher/auth-file fixes are Home/control-plane responsibilities rather than local
worker file-backed runtime features. Review transport pool isolation, signed cache
scope, Gemini tool-response ordering, post-commit SSE errors and new cancellation
behavior in Rust-specific paths separately. This focused change does not claim
coverage for them. Home Copilot/provider namespace changes do not establish worker
executor support; unsupported-provider gates remain intact.

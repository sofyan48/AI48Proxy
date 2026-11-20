# Home worker porting status

Updated: 2026-11-20. This is an implementation and validation checkpoint, not a
claim of complete CLIProxyAPI parity. Earlier handoff documents describe older
snapshots; check the current source and this checkpoint before redoing their work.

## Completed in this increment

- Activated the Antigravity-specific Chat translation entry point. Function
  declarations, supported schema cleanup, choices, call/result pairing, and JSON /
  SSE tool outputs now use the implemented Gemini bridge. See `provider.md` for
  exact schema/signature restrictions. Unsupported replay still fails explicitly.
- Added focused Claude OAuth CLI wire preparation: caller-system preservation and
  relocation, billing fingerprint, cache marker placement/limits/TTL ordering,
  native-client detection and thinking-gated context management. Final serialized
  OAuth bodies run through CCH signing after payload rules. Signing changes only
  existing five-digit attribution signatures; it does not restore filtered fields.
- Activated Claude HTTP failure classification and safe Retry-After propagation.
  Request-scoped fast entitlement refusals stop credential retry and use Home's
  existing request-scoped reporting code. Shared/model scope remains diagnostic;
  Home has no generic Claude deadline/scope reporting contract.
- Preserved Home-compatible canonical error hints for unsupported-model 400/422,
  request-scoped store=false 404, and Antigravity TOS 403. No raw provider messages,
  credentials or arbitrary headers are forwarded or logged. Bounded plaintext
  errors also reach canonical scheduler matching.

## Verification

- `cargo fmt --all`: passed.
- `cargo test --all-targets --quiet`: 298 library tests passed, one ignored;
  other targets passed (6, 6, 12 and 10 tests).
- `cargo clippy --all-targets -- -D warnings`: passed.
- Docker image rebuilt; all three workers recreated and healthy.
- Live Codex `gpt-6-luna`: HTTP 200, completed SSE, usage and `[DONE]`.
- Live Claude `claude-haiku-5-5`: HTTP 503 from Home selection (`auth_not_found`),
  before upstream execution. Read-only database inspection found no nondeleted
  Claude credential. No credential was restored and no cooldown cleared.
- Antigravity tools: deterministic fixtures only; no live upstream success claim.

## Outstanding before full replacement acceptance

- Restore or add an authorized Claude credential through Home, then validate live
  JSON, SSE, tool round trips and failure/reset behavior. Signing/wire changes do
  not prove that a genuine upstream quota rejection has been resolved.
- Claude advanced tools/alias reversal, helper/subagent profiles, continuity and
  session date pinning, thinking replay and advanced replacement policies.
- Antigravity signed reasoning replay/cache fidelity, complex schema compositions,
  incremental argument streams, advanced/server tools and multimedia paths.
- Full Codex SIWC, continuation/steering/overlap and remaining WebSocket semantics.
- Gemini CLI/plugin execution, Vertex specialized operations, complete
  cross-format conversions, and remaining unsupported configuration controls.
- Generic Claude cooldown scope/reset propagation to Home, terminal SSE reset
  classification, configurable request-scoped error matching and corresponding
  cross-repository contract tests.

Keep unsupported functionality fail-closed until its request, response, lifecycle,
and accounting contracts are implemented and tested. A successful image build or
provider request construction alone is not end-to-end provider acceptance.

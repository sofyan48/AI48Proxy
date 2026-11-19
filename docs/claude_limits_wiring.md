# Claude rate-limit compatibility and pending wiring

## Delivery boundary

`claude_limits.rs` and `claude_limits_tests.rs` are new, intentionally unwired provider-scoped files. No gateway, existing provider, Go source, dependency manifest, or scheduler changes are part of this delivery. The helper performs no I/O, retries, rotation, quota modification, sleeping, or logging. Its output retains no upstream text or headers.

## Verified source findings

- Go `internal/runtime/executor/claude_executor_request.go:889–918` classifies HTTP 429 as credential-scoped only when unified/shared quota headers explicitly reject and model-level cooling is not enabled. Otherwise it distinguishes fast-mode credit entitlement (request-scoped) from ordinary model-scoped rate limiting. A bare 429 or `rate_limit_error` does **not** establish account-wide exhaustion.
- Go `claude_executor_execute.go`, `claude_executor_stream.go`, and `claude_executor_tokens.go` invoke the same classifier on non-success responses, including error-body decode failures. Fast requests also have specialized direct-response/wrapper handling in `claude_executor_fast_error.go`; this Rust helper does not implement those wrappers or their control flow.
- Go `helps/claude_ratelimit.go` chooses the latest applicable future reset across Retry-After and rejected 5h/7d/7d_oi/unified windows. Allowed shared windows do not contribute their resets. It accepts positive fractional seconds, Unix reset timestamps, RFC3339 and HTTP dates; discards expired/nonfinite/over-seven-days-plus-one-hour candidates; excludes matching overage billing boundaries.
- Overage/Fable-only rejection with healthy/unevaluated shared windows must not become an account-wide seven-day cooldown. The helper ports Go's narrow omitted-status/utilization exceptions rather than treating all overage signals as an exemption.
- The exact three per-minute messages can suppress an otherwise unclassified unified reset only with no nonempty quota-status entry and exactly one usable Retry-After value. Substrings, combined messages, repeated Retry-After, or any declared quota status retain conservative reset selection.
- Rust `src/gateway/routes.rs::observe_result` currently parses only integer Retry-After for Claude diagnostic output and allowlists `error.type`; it does not implement the shared/model/entitlement classification or reset-header selection above.
- Rust `src/gateway/diagnostics.rs::event_failure` already maps SSE `rate_limit_error` to effective 429. This is existing compatibility, not a missing feature.
- Rust `src/reporting/usage.rs::upstream_failure` currently serializes provider-specific retry hints for Codex and Antigravity, not Claude. Returning a parsed duration alone does not propagate it into Home's cooldown policy.

These observations do not diagnose the reported live Haiku 5.5 rejection. An upstream 429 `rate_limit_error` is real upstream refusal; without applicable response headers there is no evidence identifying its quota window or scope, or proving that client identity/beta changes would resolve it. No live request or raw upstream log inspection was performed.

## API

```rust
classify(status, &headers, &body_bytes, chrono::Utc::now(), model_level_cooling)
    -> ClaudeLimit { status, scope, retry_after }
```

Call only after selecting Claude as the upstream provider. For a decoded SSE error, pass its effective error status (429 for `rate_limit_error`), not the HTTP 200 stream status. HTTP 4xx/5xx can carry retry hints without being classified as rate limited. Non-error statuses never acquire retry hints.

`LimitScope::{Model, Credential, Request}` describes observed failure scope, **not permission to retry**. `None` means no rate-limit scope inferred. Missing/invalid reset information returns `None`; do not convert that into an immediate retry.

### Explicit difference from Go

Go adds a one-time random positive 1–30 second scheduling grace to its selected duration. This helper returns the deterministic base duration without jitter. The future scheduling owner should add nonnegative bounded jitter once per failure if Go timing parity is required, cache that result, and never shorten the applicable deadline. There is no scheduler or randomness dependency here.

## Wiring instructions (not applied)

1. Declare the module next to the other providers in `src/providers.rs`:

   ```rust
   #[path = "providers/claude_limits.rs"]
   pub mod claude_limits;
   ```

   This also includes `claude_limits_tests.rs` in normal Cargo unit-test discovery.

2. In the shared Claude non-success response observation path, preserve response headers before consuming the body. Classify the original bounded error bytes using the upstream HTTP status and a single captured clock value. Apply this before any refresh/retry path discards the response. Do not classify downstream translation/transport errors as upstream quota failures.
3. Use the same helper for buffered and streaming HTTP rejection. For a terminal SSE error, retain the initial response headers and classify its decoded error envelope with the effective status. Never replay a request after stream/response commitment, or turn an upstream error into success.
4. Preserve the original status, body, and caller-visible retry headers. Do not rewrite `speed`, downgrade the model, remove entitlement requirements, manufacture credits, or enable retries on the strength of this helper. Request scope must prevent unrelated credentials from being penalized; it must not silently bypass the refused request.
5. Keep model vs. credential cooldown ownership in Home. Verify a supported CPA/Home reporting contract before propagating Claude duration/scope: the existing Rust usage serializer does not have that contract. Do not invent `fail.body` JSON fields or assume Home consumes generic `retry_after`. Carry metadata through the existing lifecycle without leaking raw bodies/headers. Respect configured request-retry/max-wait policy and cancellation.
6. Add integration tests at that wiring boundary: buffered rejection, initial streaming rejection, terminal SSE error, no replay after commitment, no cross-provider classification, and correct sanitized lifecycle attribution. Unit tests in this delivery validate the helper only, not gateway/scheduler integration.

## Validation

Formatted both new Rust files with Rustfmt. Compiled the deliberately unwired helper directly using `rustc --test` and the project's already-built `chrono`, `http`, and `serde_json` libraries; all 11 synthetic unit tests passed. No Go code was changed or Go tests run. No live upstream requests were made.

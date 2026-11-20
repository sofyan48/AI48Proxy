# Claude execution and rate-limit reference

AI48Proxy executes Claude Messages requests using Home-selected credentials. This guide covers active request preparation, HTTP rejection classification, caller-visible retry metadata, and the boundaries of Home scheduling feedback. The filename is retained for existing links; the classifier is wired, not a pending integration step.

Claude compatibility includes implementation adapted from CLIProxyAPI, including rate-limit selection and focused CLI wire behavior. AI48Proxy supports the subsets described here, not every Claude executor feature.

## Messages and credential requirements

`POST /v1/messages` supports native Messages JSON/SSE. Supported Chat callers are translated through the gateway; this does not imply complete fidelity for all Claude content blocks or advanced tools.

Authentication prefers `attributes.api_key` over Home access-token metadata. OAuth tokens are identified by the Claude OAuth token marker. First-party API keys use `x-api-key`; OAuth, token-only, and custom-base paths use Bearer as determined by credential resolution. The default endpoint is `https://api.anthropic.com`, with trusted `attributes.base_url` override support.

OAuth additionally requires persistent identity prepared by Home:

- `metadata.account_uuid` or `metadata.accountUuid`;
- exactly one canonical `metadata.claude_device_ids` entry: 64 lowercase hexadecimal characters.

The worker does not generate persistent account/device identity. It constructs request metadata from that Home identity and a validated caller/existing session UUID or a new session UUID. Caller identity cannot replace the selected credential identity during built-in preparation.

## Active wire policy

The provider validates Messages input, normalizes incompatible sampling/thinking combinations for non-confirmed-native requests, handles beta headers/body betas, and prepares focused OAuth CLI headers and body policy. OAuth supports `cloak_mode` values `auto`, `always`, and `never`, plus the focused strict-mode behavior. Confirmed native CLI requests have a separate preservation path; a client identification header alone is not a general native-feature capability guarantee.

For transformed OAuth requests, implemented behavior includes CLI identity/billing system blocks, model-dependent system relocation, date reminder placement, cache-control normalization, and the supported thinking context-management edit. Custom function tool declarations retain names rather than using reversible aliases. Foreign tool-use signature metadata is removed during non-native normalization rather than inventing valid Anthropic provenance.

Configured payload rules run after built-in preparation. OAuth CCH signing operates over final serialized bytes and does not perform another business-payload normalization pass.

Known boundaries:

- OAuth extended `thread`, `fallbacks`, `safeguards`, and `diagnostics` handling, server/advanced tool declarations, and reversible tool aliasing are unsupported.
- Focused cloaking does not implement helper/workload/probe profiles, advisor history, Fable/Mythos profiles, arbitrary fingerprint profiles, sensitive-word/pattern replacement, or cached user-ID transformations. Nonempty unsupported settings and recognized unsupported request shapes reject in their applicable paths.
- Arbitrary context-management edits and general thinking-signature/replay repair are not implemented.
- `cloak_mode: never` and confirmed-native preservation are not substitutes for implementing unsupported provider features or obtaining provider entitlement.
- `POST /v1/messages/count_tokens` requires a non-OAuth Claude API key and native Messages input. It removes generation-only fields before payload rules, calls the upstream count route, and does not provide a local token estimate or OAuth count normalization.

## HTTP failure handling

The generation gateway's shared non-success HTTP observation path is used for both buffered requests and initial streaming rejections. It captures upstream status, reads bounded error content, classifies Claude reset/scope information, and preserves sanitized execution attribution before retry or refresh can discard a response.

Configured request-scoped rules match complete original upstream bytes up to 64 KiB,
not normalized JSON; oversized/incomplete bodies cannot match a partial prefix.
`stop` suppresses refresh/rotation; `continue` allows otherwise eligible retry.
Both report `request_scoped`; their `-and-cooldown` variants report `force_cooldown`
to Home without changing stop/continue behavior. Media retry shares this matching
and action policy. Raw provider bodies are never persisted in feedback.

`src/providers/claude_limits.rs` exports:

```rust
classify(status, &headers, &body_bytes, now, model_level_cooling)
    -> ClaudeLimit { status, scope, retry_after }
```

The helper performs no I/O, sleeping, rotation, retries, quota changes, or logging. Its result retains no raw provider message or headers. The active gateway wrapper currently passes `model_level_cooling = false`; the helper parameter is not an exposed guarantee that Home scheduling policy has changed.

### Scope classification

| Observation | Classifier scope |
| --- | --- |
| Non-429 status | `None`; an HTTP error may still have a retry hint |
| 429 with explicit shared/unified quota rejection, unless model-level cooling is requested | `Credential` |
| 429 fast-mode credit/entitlement refusal without an overriding shared rejection | `Request` |
| Other 429, including a bare `rate_limit_error` | `Model` |

A bare 429 does not prove account-wide exhaustion. Shared rejection outranks a body mentioning fast mode. Narrow overage/Fable exceptions prevent an overage-only rejection with healthy or qualifying unevaluated shared windows from being mistaken for exhausted shared quota.

These scopes describe evidence about a failure, not permission to retry. Fast-mode refusal is not silently downgraded, rewritten into a slower request, or made successful.

### Reset selection

For HTTP 4xx/5xx, the helper selects the latest applicable future reset from:

- positive numeric `Retry-After`, including fractional seconds, or a supported date form;
- rejected 5-hour, 7-day, and 7-day-overage reset windows;
- the unified reset when applicable.

Reset timestamps accept Unix seconds, RFC3339, and supported HTTP-date forms. Expired, nonfinite, nonpositive, and greater-than-seven-days-plus-one-hour delays are discarded. Allowed shared windows do not contribute their reset timestamps. Matching overage billing boundaries and qualifying overage-only signals are excluded according to the classifier's narrow conditions.

The three exact recognized per-minute rate-limit messages can suppress an otherwise unclassified unified reset only when no nonempty quota-status entry exists and exactly one usable `Retry-After` is present. Substring matches, combined messages, repeated retry headers, or declared quota status do not qualify.

Missing or invalid reset evidence returns `None`, not an immediate-retry instruction. The helper returns a deterministic base duration without CLIProxyAPI's randomized scheduling grace; it does not add or own scheduler jitter.

## Downstream response and Home feedback

For an initial Claude HTTP rejection, a selected duration is rounded up to whole seconds and placed in `Retry-After`. The rejection response preserves the HTTP failure status but returns sanitized messages:

- 429: `rate_limit_error`;
- Claude 529: `overloaded_error`;
- other upstream rejection: `upstream_error`.

Only validated retry metadata is forwarded; arbitrary upstream headers and raw provider text are withheld. Diagnostic logging uses bounded categories/scope, not secrets or raw error bodies.

Fast-entitlement request scope uses the existing Home `request_scoped` failure code when a configured execution-error rule has not supplied another action. Such request-scoped failures do not trigger the ordinary credential-rotation retry path. Configured execution rules still have their own precedence and behavior.

Model and credential scopes are diagnostic here: the worker does not serialize a generic Claude scope/reset contract into Home feedback or directly set/reset account cooldowns. Home remains the scheduling owner. A caller-visible `Retry-After` therefore does not prove Home adopted that exact deadline. Provider-specific retry hints serialized for other providers must not be assumed to apply to Claude.

## Streaming boundary

Initial HTTP rejection is classified before upstream body streaming, just like
buffered rejection. With `send-keepalive-before-ft`, a keepalive can already have
committed downstream SSE while waiting for upstream headers: errors before
commitment retain HTTP status; errors after commitment use sanitized SSE envelopes.
Disconnect drops the pending upstream execution. Once a successful HTTP stream starts, terminal SSE errors use the existing event/effective-status parser: `rate_limit_error` is recognized as effective 429 for failure accounting.

Terminal SSE errors do **not** currently run through the full reset-header classifier with retained initial headers. They must not be described as having the same reset/scope handling as an initial HTTP 429. An interrupted or terminal-error stream is not successful, and no committed response can be replayed safely merely because a reset was parsed.

## Diagnosing a refusal

Distinguish Home dispatch failure from provider rejection. For example, `auth_not_found` means Home could not select an eligible credential; it is not evidence that Anthropic rejected a request. An actual upstream 429 without applicable headers does not identify a quota window, establish account-wide exhaustion, or prove that changing client identity/betas would fix the refusal.

Check Home credential availability, selected model access, OAuth identity requirements, and sanitized failure categories separately. Do not place tokens, raw headers, account identifiers, or provider bodies in diagnostic reports. Historical credential availability and previous live results are not current support guarantees.

## Automated coverage and validation boundary

Relevant source and test areas:

- `src/providers/claude.rs`, `claude_wire*.rs`, `claude_betas.rs`, and `claude_signing*.rs`: request preparation, identity/cache policy, betas, signing, and unsupported shapes.
- `src/providers/claude_limits_tests.rs`: deterministic synthetic classification/reset cases, including overage, per-minute exceptions, invalid dates, and fractional retry delays.
- `src/gateway/routes.rs` and `rejection.rs`: active HTTP classification, sanitized rejection, and retry-header validation.
- `src/gateway/feedback_tests.rs`: loopback HTTP and simulated mTLS Home feedback, including fractional reset handling and sanitized attribution.
- `src/gateway/diagnostics.rs` and usage tests: SSE effective-status and accounting behavior.

Run from the repository root:

```sh
cargo test --locked claude
cargo test --locked --all-targets
```

Unit fixtures establish helper behavior; local integration tests exercise wiring without contacting Anthropic. Neither establishes live credential validity, model entitlement, production Home behavior, or exhaustive provider compatibility. No live Claude requests or database inspection were performed for this documentation update.

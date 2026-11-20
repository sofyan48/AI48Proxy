# Codex and OpenAI-compatible execution

This reference describes AI48Proxy's Codex and OpenAI-compatible request paths, their active gateway integration, and known limitations. The filename is retained for existing links; it is not a declaration of complete provider parity.

Compatibility behavior includes implementation adapted from CLIProxyAPI. The request fixtures in `tests/cpa_contract.rs` include hand-authored, source-derived expectations, not captured upstream traffic or a differential execution of the two projects.

## Home selection and credentials

Home supplies the selected upstream model, credential, and execution options. AI48Proxy must not select a different route alias after dispatch or depend on local refresh tokens and file-backed provider configuration.

| Selection | Authentication and routing |
| --- | --- |
| Codex OAuth | Home access token as Bearer; optional Home `metadata.account_id` supplies `chatgpt-account-id` |
| Codex API key | `attributes.api_key` wins over an access token; OAuth-only account/default originator headers are not added |
| Codex base | Defaults to `https://chatgpt.com/backend-api/codex`; trusted `attributes.base_url` can override it |
| OpenAI-compatible | Explicit `attributes.base_url`; API key from attributes, not OAuth metadata; an empty key is valid for unauthenticated endpoints |
| Named compatibility provider | Gateway adapts an explicit-base selection to compatible execution while retaining selected credential/model options |

Codex SIWC credentials and SIWC mode flags reject explicitly. AI48Proxy does not implement request-aware SIWC token selection. Refresh remains Home-owned, not a local token exchange.

## HTTP routing

| Caller route | Codex upstream | Compatible upstream |
| --- | --- | --- |
| `/v1/chat/completions` | Translate Chat to Responses, then `/responses`; convert JSON/SSE back to Chat | `/chat/completions` |
| `/v1/responses` | `/responses`, using upstream SSE even for a buffered caller | Translate Responses to `/chat/completions`; convert JSON/SSE back to Responses |
| `/v1/responses/compact` | Native nonstream `/responses/compact` | Native nonstream `/responses/compact` |
| `/v1/messages` | Not a supported Codex bridge | Supported Messages/Chat translation subset |

The gateway owns cross-format translation. Direct calls to `providers::prepare` require native provider input; rejecting untranslated Responses at that boundary is intentional, not a missing gateway bridge.

Compatible Responses conversion supports focused message, reasoning-history, and function-tool mappings. Stateful continuation/compaction requests require full input history; hosted/custom tools, unsupported multimodal tool output, unattached reasoning history, and unsupported text formats reject. A Chat-native backend is not made into a fully stateful Responses service by this adapter.

Compact never uses the regular Chat bridge and never accepts streaming. Codex compact requires full conversation input rather than a nonempty `previous_response_id`. Actual compact availability depends on the selected upstream.

## Codex request normalization

Before configured payload rules, regular HTTP normalization:

- Wraps text input as a user message and changes input `system` roles to `developer`.
- Converts blank function-call arguments to `{}` and normalizes preview web-search tool names.
- Uses `max_output_tokens`, falling back to legacy completion/token fields; positive integer budgets below 16 become 16.
- Defaults parallel calls on when tools exist unless explicitly false; removes the field without tools.
- Requests encrypted reasoning content and preserves the supported web-search sources include.
- Maps fast/priority service tiers to `priority`, preserves `ultrafast`, and removes other tiers.
- Sets selected model, `stream: true`, `store: false`, and empty instructions only when instructions are absent/null.
- Removes unsupported sampling/legacy fields, cache options, and nested `prompt_cache_breakpoint` hints.
- Sanitizes input IDs and matching references before payload rules.

Regular HTTP removes `previous_response_id`; it does not maintain a server-side continuation store. Callers should send full input. Local-compaction IDs, `context_management`, and the `generate` extension reject. Native WebSocket continuation has a separate path described below.

Gateway image-tool injection/filtering runs before provider normalization and the final payload barrier. Native hosted-tool acceptance is not a guarantee of complete search/image orchestration, schema repair, orphan-output repair, or reasoning replay/signature validation. The Chat bridge remains narrower than native Responses.

Payload rules run once after built-ins and can override or filter normalized business fields. HTTP then validates transport requirements without restoring filtered values: regular Codex HTTP still requires `stream: true`, and compact must remain nonstream. Optional OAuth body compression and comprehensive provider history/replay repair are not implemented.

## Cache keys and headers

Codex HTTP cache identity is actively wired for regular and compact preparation
before final payload rules. It uses original caller context rather than translated
fields or credential metadata: explicit source keys take precedence, then trusted
execution/derived session scope, with the supported caller-scoped Chat fallback.
Responses explicit keys remain verbatim; Chat keys are trimmed. The derived
conversation root is caller-isolated and suppressed by explicit session identity.
The resulting key seeds both `prompt_cache_key` and the HTTP `session-id` header.
Payload rules may then override or remove the body key without restoring it. Nonblank incoming session headers can override it; trusted credential headers are applied after provider forwarding. The gateway additionally appends its shared incoming session headers, so request-builder precedence alone must not be treated as a guarantee that the final wire contains only one session-header value.

Codex forwards an allowlist of turn/version/request headers, never downstream authorization. OAuth client identification defaults follow configured, caller, then built-in fallback precedence; trusted credential headers are applied last within provider preparation. Caller `originator` can be forwarded. API-key mode does not acquire OAuth account identity merely because metadata contains an account ID.

The gateway supplies cache identity through request-scoped preparation context,
including refresh/rebuild; it does not obtain execution identity from provider
credential metadata. Downstream WebSocket sessions also derive a provider-scoped
execution key when an explicit key is absent.

For compatible Chat, Home's selected model options control `use-max-completion-tokens`: false/default keeps `max_tokens`, true keeps `max_completion_tokens`, and an existing preferred value wins when both are present. Streaming requests ask for usage. Text-only input-modality options normalize tool-result images into omission text rather than sending unsupported image content.

Compatible prompt-cache support currently preserves explicit keys and trims a nonempty key when enabled. False skips that normalization and does not delete an existing key. It does not implement comprehensive recovery from original translated payloads or session-derived key injection.

## Responses WebSockets

`GET /v1/responses` authenticates through Home before upgrading. The current selected-provider path is Codex; ordinary OpenAI-compatible credentials do not become native Responses WebSocket providers.

Two upstream transports are implemented:

- **Native Codex WS:** selected credential `attributes.websockets` overrides `metadata.websockets`, including explicit false. Opt-in converts the prepared endpoint to `ws`/`wss`, adds the Responses WebSocket beta when absent, and sends `response.create` framing.
- **HTTP/SSE bridge:** without native opt-in, the downstream remains WebSocket while upstream turns use Codex HTTP Responses SSE. This is selected before execution, not an automatic reconnect fallback after a failed native send.

The session retains one Home admission and selected credential, reauthorizes subsequent turns, and reports usage per response. Model changes require reconnecting. Successful terminal responses commit history; interrupted, failed, or ambiguous execution must not be silently replayed on another credential.

Native sessions support sequential `response.create` / `response.append`, upstream response-ID chaining, and active-response `response.cancel` / `response.interrupt` controls with matching IDs. Terminal IDs must match the active response. The HTTP bridge uses bounded local history for sequential turns and cannot promise native in-flight control behavior. History is session-local, not durable or transferable to a reconnect.

WS continuation state is restored before its final payload rules; only event framing follows. Native WS rejects any effective proxy other than absent/`direct`. It uses bounded connection acquisition, a 300-second upstream idle deadline, and a bounded pong write. It does not reconnect/reselect after an ambiguous send or output. An unknown upstream response ID is not made locally recoverable just because native transport can forward it.

## Images

The dedicated `/v1/images/generations` and `/v1/images/edits` handlers support Codex direct Images requests for recognized image models and a Responses image-generation-tool bridge for other selections. The bridge uses configured `gpt-image-2-base-model`, extracts image results only after completion, and can use indexed `output_item.done` output when the completion output is empty.

These are buffered image endpoints, not image SSE or general Responses output retrieval. Compatible image providers use the separate media compatibility checks and trusted base; support for text Chat does not automatically establish every media capability. See [Provider support](provider.md) for xAI image completion SSE, speech, image
policy and credential-bound xAI video routes.

## Response integrity and lifecycle

Codex HTTP uses a bounded incremental event decoder and terminal validation rather than trusting only `Content-Type`. It handles `response.completed` and the supported `response.done` alias, collects the completed response for buffered callers, and translates events for Chat callers. Missing completion, malformed events, provider failure, or truncation must not be reported as a successful execution.

The generation gateway owns cancellation, accounting, execution release, eligible
precommit retries, and Home-owned OAuth refresh. Media uses a shared refresh/retry
conductor with release-before-redispatch, request-scoped actions and cancellation;
pinned video retrieval does not rotate credentials. WebSockets retain separate
lifecycle/retry boundaries. A built request alone does not prove these behaviors.

Request-scoped error rules operate on complete original error bodies bounded to
64 KiB, before sanitization, rather than normalized JSON. Stop actions suppress
refresh/rotation; continue actions permit otherwise eligible retry. Cooldown
variants report `force_cooldown` to Home, which owns scheduling. Incomplete or
oversized bodies cannot match a partial prefix.

## Automated coverage and live-validation boundary

Relevant coverage includes:

- `tests/cpa_contract.rs`: public request-builder contracts for selected credentials, normalization, token options, cache headers, custom headers, blocked modes, and final payload rules. These tests do not send HTTP.
- `src/protocol/responses*.rs`: Chat/Responses conversion and incremental stream fixtures.
- `src/gateway/compat_tests.rs`, `codex_tests.rs`, and provider-matrix tests: gateway preparation/response and local integration coverage.
- `src/gateway/ws*_tests.rs`: opt-in transport, history, controls, and local WebSocket lifecycle coverage.
- `src/gateway/media*_tests.rs`: media preparation, responses, and local integration coverage.

Run from the repository root:

```sh
cargo test --locked --test cpa_contract
cargo test --locked --all-targets
```

The contract suite is a normal acceptance suite, not a list of intentionally ignored parity failures. Historical test counts are not current guarantees. Synthetic fixtures and loopback integration tests do not establish live-provider entitlement, authentication, or exhaustive CLIProxyAPI equivalence. No live-provider requests or cross-project differential runs were performed for this documentation update.

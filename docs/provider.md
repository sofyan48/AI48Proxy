# Provider integration handoff

`src/providers.rs` is already exported and called by the gateway. This change edits
only that file and files below `src/providers/`. No Cargo, lib, gateway, config,
Go source or credential-refresh implementation was changed. No subagent tool is
available in this session; implementation and verification were performed directly.

## Added execution paths

- Vertex (`vertex`, `vertexai`): native Gemini `generateContent` /
  `streamGenerateContent` with Home `metadata.access_token` (including existing
  nested token variants) as Bearer, or `attributes.api_key` as `x-goog-api-key`.
  Tokens require project metadata unless the trusted base already contains a
  project resource. No service-account material or local exchange is used.
- Vertex regional/global/residency hosts; project/location resource routing;
  API-key short hosts, version prefixes and full publisher/models prefixes.
- Vertex OpenAI Chat `endpoints/openapi/chat/completions`, normalized
  `<publisher>/<model>`, Home `attributes.vertex_model_openapi` alias rules,
  explicit `openapi`, and partner-model automatic selection. OpenAPI location
  overrides use `openapi_location` / `openapi-location`. Google OpenAPI endpoints
  reject API-key authentication, matching Go; third-party OpenAPI keys use Bearer.
- Antigravity: Home access token plus mandatory `metadata.project_id`; default
  daily Cloud Code endpoint or trusted attribute/metadata base override;
  `/v1internal:generateContent` and `/v1internal:streamGenerateContent?alt=sse`.
  Wraps native Gemini input in `{model, project, userAgent, requestType, requestId,
  request}`. Uses caller sessionId or Go-compatible SHA-256 first-user-text stable
  session ID, fresh agent UUID, selected model, safetySettings removal, non-Claude
  maxOutputTokens removal, Claude VALIDATED mode and dispatch model output cap.
  Uses the Go fallback Hub 2.29.0 UA unless Home supplies a user_agent override.
- Antigravity JSON / parsed SSE-data response unwrapping and usage extraction,
  including nested usage, top-level usage and trailing usage-only events. Missing
  usage stays absent; cached and thought counts are preserved without inventing
  accounting totals. Usage snapshots must not be summed as deltas.

The source contracts inspected were Home `internal/home/auth_sanitize.go` (removes
refresh tokens/service accounts, retains execution metadata and projects Vertex
routing rules), Go `gemini_vertex_executor.go`, `helps/vertex_openapi.go`,
`antigravity_executor_{auth,request,execute}.go`, `misc/antigravity_version.go`, and
`helps/usage_helpers.go`.

## Existing public API, preserved

- `resolve(&Selection, incoming_protocol)` returns secret-bearing credentials.
- `prepare(...)` delegates to `prepare_context(..., Option<ApplyContext>)`.
  Inputs must already be native selected-provider format with thinking applied.
  Original caller body, headers, source protocol and requested-model alias survive
  through the final payload barrier. No body mutations follow payload rules.
- `Prepared { request, stream, format, response_mode }` is unchanged, so existing
  gateway matches still compile. Antigravity uses Gemini format and Json/Sse modes;
  these transport modes alone do **not** identify its response envelope.
- Codex HTTP Responses, Claude Messages, Gemini keys and OpenAI-compatible paths
  remain. CodexSse handling and Home-only refresh/401 helpers remain unchanged.
- Proxy precedence, explicit direct, no redirects and the existing 30-second
  connect-only timeout remain; no request/read timeout was added.

## Required parent gateway changes (not edited here)

These providers are **not yet reachable through the HTTP gateway**, which still
explicitly rejects them. Do not report full Home-worker replacement/parity.

1. In `src/gateway/upstream.rs::prepare`, outgoing-provider selection:
   - `antigravity` -> `Format::Gemini`.
   - `vertex` / `vertexai` -> call
     `providers::vertex::uses_openapi(selection, requested_model)?` using the
     original caller alias (URL model for Gemini; body model otherwise), before
     translation. True -> `Format::OpenAi`, false -> `Format::Gemini`.
   - Remove these names from the explicit unsupported arm and add all three to
     the native-provider allowlist before the compatibility-provider conversion.
     Otherwise the credential is incorrectly relabeled openai-compatibility.
   - Native Gemini requests and Vertex OpenAPI Chat can pass immediately. Keep
     unsupported cross-format routes closed until actual Gemini translators are
     implemented; `protocol_format(Format::Gemini)` currently rejects translation.
     Do not invent translation or send Chat/Messages bodies as Gemini contents.
2. Carry an explicit response-envelope marker from selected provider into the
   gateway's upstream Prepared object and both response handlers (e.g. a boolean
   `antigravity_envelope`). Do not infer it from host, Gemini format, or SSE mode.
   The providers Prepared struct was deliberately not extended with enum variants
   that would force out-of-scope gateway edits in this task.
3. In `src/gateway/response.rs::buffered`, immediately after parsing successful
   JSON, call `providers::antigravity::unwrap_response(&value)?` when marked,
   **before** `Usage::json` or response translation. Serialize the unwrapped value
   for passthrough too: returning the original byte buffer still leaks the native
   envelope. Map invalid/error envelopes to failed accounting and HTTP 502 without
   exposing provider error secrets.
4. In `src/gateway/response.rs::streaming`, marked Antigravity must use bounded
   incremental SSE framing (not per-network-chunk JSON parsing). Parse each data
   object, call `unwrap_response`, feed unwrapped JSON to Gemini usage/translation,
   then serialize as downstream Gemini `data: <json>\n\n`. Preserve usage-only
   events, split/coalesced frames, cancellation, backpressure and EOF validation.
   Raw upstream passthrough / `usage.feed(&bytes)` does not perform the unwrap.
   APIs: `antigravity::unwrap_response(&Value) -> Result<Value>` and
   `antigravity::usage_metadata(&Value) -> Option<&Value>`; SSE framing remains
   gateway-owned. Do not send Antigravity traffic until this decoder is wired.
5. Keep the existing Home-only OAuth 401 refresh, one-retry/precommit boundary,
   execution release and accounting ownership. Rebuild from refreshed selection.
   Missing Antigravity project fails explicitly; Home must prepare it. No local
   OAuth refresh or project discovery is introduced here.

## Explicit limits

Antigravity tools, function calls/responses, thought signature/replay inputs,
structured output and image generation reject before network execution rather
than pretending to implement the Go schema/signature/replay machinery. No full Go
model-specific normalization, credits fallback, local compaction, version updater,
project discovery or signature cache. Basic native generation is implemented,
not full Antigravity parity. Vertex Imagen, interactions, countTokens and full
reasoning replay are not implemented. Gemini CLI remains plugin-hosted/unsupported.
Codex WebSocket/SIWC/continuation and advanced Claude parity remain out of scope.

Credential auth headers cannot be replaced by caller headers or trusted custom
header attributes. Errors do not echo tokens. No local secrets are persisted.

## Validation

Baseline `cargo test --all-targets --quiet`: 124 passed before edits. Eight added
Google provider fixture tests exercise built RequestBuilder URL/header/body,
Home-token vs API-key selection, native/OpenAPI routing, regional hosts, final
payload rules/context, unsupported inputs, and JSON/parsed SSE-data usage/response
extraction. These are offline fixtures, not live Home/provider or gateway browser
integration tests. Final commands and counts are reported in the task response.

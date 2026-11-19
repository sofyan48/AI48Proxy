# Codex / OpenAI Home-worker contract audit

## Scope and evidence boundary

Audit date: 2026-11-19. Go was read-only. This work owns only this document, `tests/cpa_contract.rs`, and synthetic files under `tests/fixtures/`; no Rust source, Cargo, or Go edits were made by this audit.

The fixture is **hand-authored from the Go sources**, not an upstream capture or the result of a Go differential run. All tokens/accounts/hosts are synthetic. Tests build requests using the current public `providers::prepare` / `resolve` APIs; they never send HTTP. They do not establish gateway, transport, streaming, Home enrollment, refresh, or live-provider parity.

Other agents modified Rust providers/gateway during the audit. In particular, token normalization, compact handling, hosted-tool pass-through, input-ID sanitization, and header precedence appeared during inspection. Initial findings must not be mistaken for the final state. Re-run the tests against the final integrated tree.

## Actual Home → Go worker contract

Relevant sources (relative to the sibling repository):

- `AI48Home/internal/home/auth_sanitize.go:SanitizeAuthForDownstream`: clones selected credentials, projects `credential_options.connection-pool` into `ProxyConnectionPool`, strips refresh material and private source configuration. Model execution options remain in metadata. Workers must not depend on the original file-backed config or refresh token.
- `AI48Home/internal/cluster/cpa_execution_roundtrip_test.go:TestCPAExecutionImportDatabaseDispatchExport`: DB/import/dispatch/export checks include named provider `gateway`, per-model `is-compat`, token options and selected connection-pool overrides.
- `CLIProxyAPI/internal/runtime/executor/openai_compat_executor.go:resolveCompatConfig`: Home mode reads `metadata.credential_options` (`support-prompt-cache-key`, `models`), then the explicit legacy attribute `support_prompt_cache_key`, then resolved Home route model options. Do not resolve a different client's alias after Home selected a route.
- `openai_compat_executor.go:executeWithCredentials`: normal generation always targets **Chat `/chat/completions`**, including Responses callers after translation. `/responses/compact` is a distinct native Responses path. The default UA is `cli-proxy-openai-compat`; credentials/base URL come from selected attributes. A missing API key is not intrinsically rejected by Go, whereas a missing base URL is.
- `helps/openai_compat_max_tokens.go`: select the configured model preference, preserve the preferred token field if both exist, remove the other field. False/default means `max_tokens`; true means `max_completion_tokens`.
- `openai_compat_executor.go:applyPromptCacheKeyWithCompatDoc`: enabling support recovers an explicit key from original/translated input or derives a scoped key from session identity. **False does not delete an already-present key**; it skips injection. Do not implement false as unconditional filtering.
- `codex_executor_auth.go:codexCredsForRequest` and `codex_websockets_request.go:codexAuthUsesAPIKey`: API-key and OAuth/SIWC paths differ; API-key identity must not inherit OAuth account headers. SIWC selection is request-aware, with Codex-native clients retaining the original OAuth path.
- `translator/codex/openai/responses/codex_openai-responses_request.go:ConvertOpenAIResponsesRequestToCodex`: text input wrapping, system→developer, token-budget fallback, blank function-call arguments, required stream/store/include, service-tier mapping, nested cache-breakpoint removal, built-in-tool and orphan-output normalization.
- `codex_executor_execute.go:Execute` and corresponding stream implementation: thinking/model normalization, output floor of 16, history/hosted-tool handling, schema normalization, reasoning replay/signature sanitization, and input-ID sanitization precede the final user-payload barrier. Native Codex instructions have an explicit preservation exception; an unconditional default is not universal fidelity.
- `codex_executor_request.go:cacheHelperDoc`: explicit Responses `prompt_cache_key` also seeds `Session-Id`. Input-ID normalization precedes `FinalizePayloadDoc`; optional OAuth compression follows it. `applyCodexHeadersWithSIWC`: caller turn headers, config/caller/default UA precedence, caller Originator, OAuth account ID, then operator credential header overrides. Normal HTTP Codex uses upstream SSE even for a nonstream downstream caller.

## Supported / blocked / unverified matrix

“Contract tested” below means only the assertions in the public request-builder integration test. It is not end-to-end parity.

| Capability | Current audit disposition | Test / boundary |
| --- | --- | --- |
| Codex OAuth selected token/account/default URL + upstream SSE | Contract fixture | `codex_source_derived_normalization_contracts` |
| Codex API-key precedence, custom base, no OAuth-only headers | Contract fixture | `codex_api_key_wins_without_oauth_account_headers` |
| Text wrapping, role, legacy budgets, floor, service tier, blank arguments, explicit parallel false | Contract fixture | Exact full body comparison for two source-derived cases |
| Codex turn-header forwarding; downstream bearer not reused | Contract fixture | `codex_forwards_turn_headers_but_not_client_authorization` |
| Operator turn header wins over caller | Active contract; concurrent agent added implementation | `gap_codex_custom_turn_header_wins_over_incoming_header` |
| Codex continuation / local compaction trigger / generate extension | Explicitly blocked in regular Responses builder | Rejection fixtures; not equivalent to Go support |
| SIWC | Explicitly blocked | Selected flag/nested credential fixtures; request-aware Go behavior absent |
| Hosted search/image tools | Newly pass through, specialized behavior **unverified** | Not asserted rejected; not evidence of injection/history/schema/replay correctness |
| Codex Chat bridge / compact endpoint | Concurrent source implementation, unverified here | Requires gateway/response-stream contract tests, not just provider preparation |
| OpenAI native Chat selected model/key/base/stream | Contract fixture | `compat_native_chat_uses_selected_model_base_key_and_stream_mode` |
| Home model token preference true/false | Active contracts; concurrent agent added implementation | Both token-field tests, true tested for stream and nonstream |
| Named Home compatibility providers | Gateway adaptation exists; direct API boundary differs | Gateway maps explicit-base providers to `openai-compatibility`; fixture uses that adapted selection while retaining original auth provider/name |
| Responses caller → compat upstream Chat | Known provider-boundary gap / gateway needs verification | Ignored Go-expectation test; public provider API currently requires already-translated Chat |
| Codex nested cache hints | Contract fixture; concurrent agent fixed during audit | Active `gap_codex_strips_nested_cache_hints` test |
| Codex prompt key → Session-Id | Known gap | Ignored Go-expectation test |
| User payload final semantic barrier | Contract fixture | Override reintroduces values after built-ins and filter removes include; no normalization restoration allowed |
| Pool tuning / SOCKS proxy / compression / native-instructions exception | Not established | Inspection findings below; no parity claim |
| SSE terminal repair, errors, retries, cancellation, usage, refresh, enrollment | Outside request-fixture proof | Must be tested separately through worker/gateway transport |

## Highest-risk remaining integration work for parent

1. **Compat Responses routing is not a transparent native `/responses` proxy.** Go translates it to Chat unless compact was requested. Initial Rust gateway treated all OpenAI routes as one format and forwarded `/responses`. Latest provider normalization rejects unconverted `input`, preventing a silently malformed send but not implementing the missing translation. Ensure gateway translates both request and JSON/SSE response, preserves original payload context for cache rules, and tests stream/nonstream through a local upstream. The ignored test describes the end behavior; if the public provider boundary deliberately remains native-only, move that expectation to a gateway test rather than weakening it to passthrough.
2. **Codex cache-hint regression now covered.** The concurrent implementation fixed the item/content fixture during this audit, and that test is now active. Preserve removal at item and content/output-part levels *before* user payload rules. The Go translator documents strict-upstream rejection. This fixture covers item/content removal without deleting user text, not every output-part shape.
3. **Codex session affinity is incomplete.** A payload `prompt_cache_key` must seed `Session-Id` with Go's existing-header precedence. Rust's default header forwarding alone is not sufficient. Separately check gateway session-header appending: after provider header preparation, appending the same incoming header can create duplicates or defeat the cache-derived identity. Add a gateway wire-header test, not just a HeaderMap getter assertion.
4. **Do not accept settings as supported merely because they deserialize.** Go uses Home-projected connection-pool overrides; Rust builds a fresh reqwest client in preparation. Pool tuning/reuse is not reproduced by retaining auth JSON. Config accepts SOCKS proxy schemes, but current provider construction explicitly allows only HTTP/HTTPS; report or reject that mismatch at startup rather than failing only when selected.
5. **Codex native/tool safety remains broader than these fixtures.** Hosted-tool pass-through does not implement automatic image/search injection, history repair, orphan tool output sanitation, reasoning replay/signature origin checks, native instructions preservation, request-aware SIWC or websocket continuation. Gate the unsupported features or test them explicitly. Do not label tools supported from successful request construction alone.
6. **Prompt-cache support needs source-aware recovery and derivation.** Trimming an already-present key is only a subset. Go recovers it from original requests and derives scoped keys from session metadata; attribute false overrides metadata true but does not erase existing keys. Preserve selected-route options when two aliases share one upstream.

## Tests and validation

Run from `AI48Proxy`:

```sh
cargo test --locked --test cpa_contract
cargo test --locked --test cpa_contract -- --ignored
```

The second command intentionally exposes unresolved Go expectations and is not a green acceptance suite. Promote each ignored case only after the consumer implementation is wired and tested. Keep the source-derived expectations; do not snapshot the current bug.

`rustfmt --edition 2024 tests/cpa_contract.rs` was run. Initial test attempts were blocked compiling concurrently edited source: an untyped `HashMap` in `src/providers/codex.rs` and initially unmatched new `ResponsesChat` variants in `src/gateway/response.rs`. Those files were not edited by this audit. Once the other agents fixed compilation, the active suite passed (9 passed, 3 ignored). Explicitly running ignored cases produced 1 pass (new nested-cache fix, subsequently promoted) and 2 failures: missing Session-Id and provider API rejecting untranslated compat Responses. The final active run is recorded in the handoff. No Go tests/overlay or live credential requests were run.

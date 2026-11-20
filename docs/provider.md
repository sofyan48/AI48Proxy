# AI48Proxy provider support

AI48Proxy executes requests using credentials, models, and routing decisions supplied by AI48Home. This guide describes the current Rust gateway and its supported subsets, not a promise that every provider feature or model is available in a deployment.

Provider compatibility includes implementation adapted from CLIProxyAPI. The support boundaries below describe AI48Proxy's own active wiring; they do not imply complete CLIProxyAPI parity or an independently developed clean-room implementation.

## Request lifecycle

Home authorizes the caller and selects the upstream model and credential. The gateway translates supported caller formats, applies thinking capabilities, performs provider normalization, then applies configured payload rules as the final business-payload mutation. Serialization, transport framing, and Claude signing follow that barrier. Original caller context remains available to payload rules across translation.

Home owns credential refresh and persistent credential identity. The generation gateway can request a Home refresh after an eligible OAuth 401 and rebuild the request before response commitment. It does not exchange refresh tokens or discover projects locally. Retry and refresh behavior is path-specific; media and WebSocket execution must not be assumed to inherit the HTTP generation retry loop.

## Provider matrix

“Implemented” means reachable through the gateway with the stated restrictions. Model access, provider entitlement, Home readiness, and valid dispatched credentials are still required.

| Home provider | Implemented execution | Important boundary |
| --- | --- | --- |
| `openai`, `openai-compatibility` | Chat-native compatible HTTP; Responses-to-Chat bridge; native nonstream compact | Explicit trusted `attributes.base_url` required; not a transparent native Responses proxy |
| Named compatible provider | Chat-compatible execution when an explicit trusted base is supplied | Unsupported native executor names are not converted into compatibility providers |
| `codex` | Responses HTTP/SSE, Chat bridge, compact, downstream Responses WebSocket, images | SIWC and local compaction are unsupported; native upstream WS requires opt-in |
| `claude` | Messages JSON/SSE, Chat/Messages translation, API-key token counting | OAuth requires Home-prepared identity and supports only a focused CLI wire policy |
| `gemini` | Native Gemini generation and streaming; focused Chat bridge | API key required; not a general translator for every Gemini API |
| `vertex`, `vertexai` | Native Gemini or OpenAPI Chat selected from Home routing metadata | Google OpenAPI endpoints require an access token; Imagen is unsupported |
| `antigravity` | Native Gemini envelope and focused Chat bridge, JSON/SSE response unwrapping | Home access token and `metadata.project_id` required; restricted tools/replay |
| `xai` | Dedicated image and speech execution | This media executor does not establish a dedicated xAI text-generation implementation |
| `gemini-cli`, `qwen`, `iflow`, `kiro`, `github-copilot` | No native generation executor | Explicitly rejected by the generation gateway |

### Caller routes and translation

- `POST /v1/chat/completions`: compatible Chat, Claude Messages translation, Codex Responses translation, or focused Gemini/Vertex/Antigravity Chat translation according to the selected provider.
- `POST /v1/messages`: native Claude or supported Chat/Messages translation to a Chat-native upstream. It is not a Claude-to-Codex or Claude-to-Gemini bridge.
- `POST /v1/responses`: native Codex, or Responses-to-Chat conversion for compatible providers. The latter converts JSON and SSE output back to Responses events.
- `POST /v1/responses/compact`: nonstream native compact for Codex and compatible providers; requires an upstream that actually implements compact.
- `GET /v1/responses`: downstream WebSocket upgrade for the Codex execution path. It is not generic WebSocket support for all compatible providers.
- `POST /v1/messages/count_tokens`: native Claude API-key counting only, not OAuth counting or a local estimate.
- `POST /v1beta/models/{model}:generateContent` and `:streamGenerateContent`: native Gemini-family input. Other Gemini actions are not implemented.
- `POST /v1/completions` is registered in the OpenAI-format path, but there is no dedicated legacy prompt-to-Chat translator. Registration alone does not establish legacy Completions compatibility.

Cross-format support is intentionally asymmetric. Chat-to-Gemini is implemented, but general Gemini-to-Chat/Messages and Messages-to-Gemini conversion are not. Unsupported shapes reject rather than being forwarded as a different protocol.

## Google-family execution

### Gemini and Vertex

Gemini uses `attributes.api_key` as `x-goog-api-key`. Vertex can use Home access tokens or native Gemini API keys. Token-based Vertex needs project metadata unless the trusted base already contains the project resource.

Vertex chooses native Gemini versus OpenAPI Chat before translation, using the original caller alias and Home's `vertex_model_openapi` rules, explicit `openapi`, or partner-publisher selection. Routing supports regional, global, and residency hosts, project/location resources, and `openapi_location` / `openapi-location` overrides. OpenAPI models are normalized to publisher/model form. API keys for third-party OpenAPI bases use Bearer; Google-hosted OpenAPI rejects API-key authentication.

Vertex Imagen conversion, interactions, token-count routes, and full reasoning replay are not implemented.

### Antigravity

The provider wraps Gemini input with the selected model, Home project, request identity, and a nested `request`. The gateway carries an explicit Antigravity response kind: buffered responses and incrementally framed SSE events are unwrapped before translation, serialization, and usage extraction. This is active wiring, not an unconnected helper.

Usage-only events are retained. Usage snapshots are not summed as deltas, and missing provider usage is not fabricated. Normalization removes safety settings, removes non-Claude output limits, caps Claude output against dispatched model metadata where available, and uses `VALIDATED` function-calling mode for Claude models.

The focused Chat tool bridge supports function declarations, tool calls and matched results, parallel calls, and Gemini tool choices. It retains function-call IDs and transports supported thought signatures on `tool_calls[].extra_content.google.thought_signature`. Antigravity declaration cleanup is scoped to schemas, not function arguments or results.

Known limits:

- Antigravity image generation, media replay, structured-output conversion, cached/prewrapped requests, and general signed reasoning replay reject.
- Tool history requires IDs and complete matched results; native ID-less history is not supported.
- Claude-backed Antigravity requires `VALIDATED`; explicit Chat choices that conflict with that policy reject instead of being silently overridden.
- Supported function-signature envelope checks are not authentication or proof of conversation provenance. Signed Claude thought/text replay, nested server-tool envelopes, and replay caches are not implemented.
- Local schema references, conditional/allOf merging, malformed/MCP repair, and Claude union flattening are not implemented. Union handling depends on the model's schema policy.
- Built-in tools, incremental `partialArgs`/`willContinue`, and signature-only trailing SSE events are outside the focused bridge.
- There is no local project discovery, credits fallback, local compaction, or provider-version updater.

## Images, speech, and video

Media routes use their own Home dispatch and lifecycle path.

| Route | Implemented subset |
| --- | --- |
| `POST /v1/images/generations` | Codex, xAI, and recognized OpenAI-compatible image providers |
| `POST /v1/images/edits` | JSON input or multipart form; provider-specific conversion |
| `POST /v1/audio/speech`, `POST /v1/tts` | xAI `grok-tts`, with `mp3`, `wav`, or `pcm` output |
| Registered video creation/edit/extension/retrieval routes | Return unsupported; no video execution or credential-bound retrieval |

Codex uses direct Images routes for recognized image models, otherwise a Responses image-generation-tool bridge using `gpt-image-2-base-model`. The bridge requires a successful completion and image output; partial output is not sufficient. Image responses are normalized to the requested response format. Speech streams audio bytes and retains admission until EOF or disconnect.

Image SSE (`stream: true`) is unsupported. The image-generation disable policy gates standalone image routes and hosted image tools; speech is separate. Media JSON responses are bounded to 64 MiB, and speech request bodies have a 1 MiB cap in addition to applicable configuration. An implemented transport does not guarantee an upstream supports a particular image model, voice, or option.

## Transport and security boundaries

- Upstream authentication comes from the selected Home credential, not the caller's downstream Bearer token. Native auth headers cannot be replaced by trusted custom header attributes.
- Trusted credential bases are validated; HTTP redirects and implicit environment proxies are disabled.
- Credential proxy settings take precedence over the global proxy; `direct` explicitly bypasses it. HTTP execution supports HTTP/HTTPS proxies, not SOCKS. Native Codex WS rejects non-direct proxy configuration.
- HTTP clients use a 30-second connect timeout without a general established-response read timeout. Native Codex WS has separate liveness deadlines.
- Home-projected connection-pool metadata does not establish matching pool behavior: provider preparation builds a new HTTP client rather than implementing shared per-credential pool tuning.
- Cancellation and Home membership loss terminate execution. Usage, failure attribution, and admission release belong to the gateway lifecycle, not request construction alone.

## Verification boundary

Source entry points are `src/gateway.rs`, `src/gateway/upstream.rs`, `src/providers.rs`, and the feature modules under `src/gateway/`, `src/providers/`, and `src/protocol/`. Tests include request-builder fixtures, protocol unit tests, and local HTTP/WebSocket and simulated Home/mTLS integration tests.

Run from the repository root:

```sh
cargo test --locked --all-targets
```

Automated fixtures and loopback servers do not validate real credentials, current provider entitlement, production Home enrollment, or live-provider behavior. No live-provider validation was performed for this documentation update.

See [Codex and OpenAI-compatible execution](codex-openai-parity.md) and [Claude execution and rate limits](claude_limits_wiring.md) for detailed boundaries.

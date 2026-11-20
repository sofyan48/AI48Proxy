# AI48Home upstream and provider synchronization

AI48Proxy is a Home-managed worker, not an independent credential manager. AI48Home is authoritative for credential selection, model aliases, upstream metadata, retry policy, cooldowns, concurrency identity, refresh, and accounting. AI48Proxy receives a sanitized downstream configuration and a per-request dispatch selection.

## Synchronization flow

```text
GET config
  -> parse and validate sanitized YAML
  -> SUBSCRIBE config <lifecycle revision> <worker instance>
  -> receive initial snapshot
  -> PING command probe
  -> publish readiness

HTTP request
  -> Home auth/model validation
  -> Home credential dispatch (RPOP)
  -> provider preparation from selected auth metadata
  -> payload rules as the final business boundary
  -> upstream execution
  -> usage/request-log/concurrency release back to Home
```

A config revision change fences the current lifetime and requires registration. Invalid or unsupported updates are not partially applied. The worker does not create local credentials, local cooldowns, local model catalogs, or local OAuth refresh state.

## RESP contract used by the worker

| Home operation | Worker behavior |
| --- | --- |
| `GET config` | Fetch initial sanitized downstream YAML |
| `SUBSCRIBE config` | Receive snapshots, cluster messages, rebalance and heartbeat replies |
| `RPOP <auth request>` | Obtain the authorized model/provider/credential selection |
| `RPOP <auth-validate>` | Validate downstream principal for WebSocket and model access |
| `GET <models request>` | Obtain Home-authorized model catalog |
| `GET <refresh request>` | Ask Home to refresh/rotate a credential after an eligible failure |
| `LPUSH concurrency-release` | Acknowledge cumulative release ownership |
| `LPUSH usage` | Persist usage and scheduler feedback |
| `LPUSH in-flight-snapshot` | Publish bounded execution observations |
| `RPUSH request-log` | Submit optional sanitized request metadata |
| `RPUSH app-log` | Submit bounded application logs |
| `GET plugin-sync`, `GET plugin-tasks`, `RPUSH plugin-status` | Plugin contract endpoints; native plugin execution remains disabled until the full host lifecycle is enabled |
| `CLUSTER NODES` | Discover Home failover targets |
| `CLUSTER BALANCE` | Planned replacement-connection rebalance and latency reports |
| Home KV/CAS | Store supported video credential bindings and other explicit worker state |

All normal commands use RESP2 over verified mTLS. RESP password authentication, Redis initialization, local Redis fallback, and HTTP upstream credential inheritance are not used.

## Provider matrix

| Provider selected by Home | AI48Proxy path | Current boundary |
| --- | --- | --- |
| `openai`, `openai-compatibility`, named compatible providers | Chat/Responses compatible HTTP | Trusted `attributes.base_url` is required for custom providers; unsupported native providers are not silently converted |
| `codex` | Responses HTTP/SSE, Chat bridge, constrained downstream Responses WebSocket, images | SIWC, full proxy-aware upstream WS, full duplex steering/recovery and signed reasoning replay are not enabled |
| `claude` | Messages JSON/SSE, focused Chat translation, Home identity, final CCH signing | Advanced helper/subagent continuity and signed thinking replay cache are not enabled |
| `gemini` | Native Gemini generation and SSE | The worker supports the native Gemini generation subset |
| `gemini-interactions` | Recognized Home provider identity | Full Interactions-specific request/response/usage executor is not enabled; requests fail closed until that native contract is wired |
| `vertex`, `vertexai` | Native Gemini or OpenAPI Chat selected from Home metadata | Specialized Interactions/Imagen operations are not enabled |
| `antigravity` | Native envelope and focused function-tool bridge | Full signed reasoning replay cache, incremental partial arguments and multimedia workflows are not enabled |
| `xai` | Responses/Chat text, images, speech, video | xAI upstream WebSockets and advanced replay/continuation are not enabled |
| `kimi`, `kimi-ai` | Native Chat/Responses with basic function tools and tool history | Provider-specific thinking replay, Claude delegation and advanced continuation are not enabled |
| `meta` | Responses/Chat text, basic function tools, Home refresh for DCA credentials | Subscription attestation and advanced reasoning replay are not enabled |
| `gemini-cli`, `aistudio` | Explicitly fail closed | These require CLIProxyAPI's plugin host or AI Studio WebSocket relay; they are intentionally skipped |
| `devin` | Explicitly out of scope | Never selected as a supported AI48Proxy executor |

## Upstream preparation guarantees

Provider preparation follows this order:

1. Use only the selection returned by Home.
2. Resolve provider credentials and trusted base URL.
3. Translate the supported caller format.
4. Apply provider normalization and thinking controls.
5. Apply Home-configured payload rules exactly once.
6. Perform read-only validation.
7. Serialize, sign where required, and frame the transport.

No later stage restores filtered fields or overwrites configured values. Upstream authentication headers are generated from the selected Home credential; downstream caller Bearer tokens are never forwarded as provider credentials.

## Operational safety

- A provider route being registered does not mean every model, format, tool, or account is supported.
- Unsupported active configuration is rejected during registration or hot reload.
- Ambiguous dispatch or release failures fence admission rather than blindly retrying.
- WebSocket replay is session-local and bounded; failed/cancelled/ambiguous turns are not automatically replayed.
- Claude CCH signing is performed only over the final serialized OAuth body.
- Antigravity tool ID restoration requires deterministic call ID, function name, and argument matching.
- Usage and concurrency release are owned by the Home execution lifecycle, not by provider request construction.

## Verification

Use the provider matrix and Home integration fixtures for local validation:

```sh
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --all --check
```

These tests use synthetic credentials, loopback upstreams, and local mTLS/RESP fixtures. They do not prove live provider entitlement or production account availability.

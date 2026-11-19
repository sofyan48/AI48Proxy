# Home-managed Rust gateway: compatibility baseline

Status: audited compatibility target. An initial Rust executable now implements a subset; see `../README.md` for the authoritative implemented/unsupported feature inventory. The requirements below are not a claim of complete parity.

This document describes the current sibling `AI48Home` and `CLIProxyAPI` implementations inspected on 2026-11-18. Source paths are relative to the parent workspace. Revalidate them when either repository changes. Proposed Rust modules and acceptance criteria below are design recommendations, not existing functionality.

## 1. Scope and ownership

Rewrite the gateway data plane using Rust and Tokio, prioritizing compatibility with AI48Home. Do not port standalone CPA administration first.

| Responsibility | Owner |
| --- | --- |
| Persistent configuration, credentials, credential refresh, scheduling, cooldown, downstream access policy | Home |
| Config subscription, membership health, upstream execution, protocol translation, streaming/cancellation | Rust gateway |
| Credential admission/reclamation policy | Home, with mandatory gateway release and lifetime cooperation |
| Usage/billing persistence and scheduler feedback | Home, with gateway execution reports |
| In-flight observation | Gateway publishes; Home aggregates |

A gateway must not fall back to local credentials, local rotation, local refresh, or local KV when its Home operation fails.

Source: `CLIProxyAPI/sdk/cliproxy/service_lifecycle.go`, `service_config.go`, `auth/conductor_home.go`, `internal/runtime/executor/helps/home_refresh.go`.

## 2. Bootstrap and transport

Current CPA precedence is nonempty `-home-jwt`, then `HOME_JWT`, then `home_jwt`. Home mode takes precedence over local config and PostgreSQL/Git/object-store credential sources. A local plugin bootstrap read is a separate pre-flag exception, not general runtime configuration precedence.

The bootstrap JWT carries `certificate_id`, `cluster_id`, `ca_fingerprint`, `enrollment_secret`, `ip`, `port`, and `iat`. CPA currently decodes claims without verifying the JWT signature locally; do not describe that as signature verification. CA fingerprint pinning is a distinct security check. Never log the token or enrollment secret.

Enrollment uses a locally generated private key and CSR:

```text
CERTIFICATE REQUEST <certificate-id> <enrollment-secret> <CSR-PEM>
```

The reply is bulk JSON with `ok`, `certificate`, and `ca`. Enrollment is one-shot, including when the successful reply is lost. Persist the private key securely before enrollment and treat an uncertain enrollment result as requiring operator recovery, not unlimited automatic reissuance. Home assigns the certificate CN from the certificate ID.

Normal traffic is custom RESP2 over mTLS. Home validates both the client certificate chain and the database certificate record. Password `AUTH` is disabled. Do not negotiate HTTP ALPN on RESP connections: HTTP and RESP share the listener, and HTTP ALPN selects the HTTP branch.

Implement RESP simple strings, errors, integers, bulk/null strings, and arrays. JSON business errors can arrive inside successful bulk replies. Avoid Redis client initialization commands such as `HELLO`/`SELECT` that Home does not implement. Codec limits must prevent unbounded allocations.

Sources:
- `CLIProxyAPI/cmd/server/main.go`
- `CLIProxyAPI/internal/home/certificate.go`
- `AI48Home/internal/cluster/certificate.go`
- `AI48Home/internal/protocolmux/mux.go`
- `AI48Home/internal/respserver/server.go`

## 3. Membership lifecycle: prerequisite for dispatch

Startup ordering:

1. Enroll or load the pinned mTLS identity.
2. Open a bootstrap connection and `GET config`.
3. Parse and validate lifecycle configuration and supported runtime settings.
4. Open a dedicated subscription: `SUBSCRIBE config <lifecycle-revision> <instance-uuid>`.
5. Read its acknowledgement and initial full config snapshot.
6. Apply the configuration and initialize required executors.
7. Open **new worker connections to that same Home** after membership acknowledgement.
8. Publish readiness only when config, membership, dispatch pool, execution registry, and release machinery are usable.

Home assigns connection lifetime at accept time. A bootstrap socket is not upgraded into a controlled dispatch socket after membership starts. The subscription itself is not the dispatch worker.

Subscription frames:

```text
["subscribe", "config", 1]
["message", "config", "<YAML>"]
["pong", ""]
["message", "cluster", "<JSON>"]
["message", "rebalance", "<JSON>"]
```

The `cluster` and `rebalance` messages arrive on the config subscription without separate subscriptions. Close the socket to unsubscribe; the current registry does not register `UNSUBSCRIBE`.

Heartbeat health is not equivalent to receiving arbitrary subscription traffic. Ordinary persisted-liveness heartbeats renew the lease; `home-rtt/...` telemetry pongs do not. Honor `cpa-heartbeat-timeout` and `cpa-cancel-bound`: fence new admissions and cancel old-lifetime executions on health loss. Cancellation must close underlying streams/sockets, not merely abandon a Tokio task handle.

Recovering a lifetime requires explicit state handling: pending-dispatch settlement, fencing, cancellation/drain or validated takeover, fresh connection ownership, then readiness. Takeover uses `SUBSCRIBE config <revision> takeover <instance-uuid>`. Do not carry release counters into an unrelated membership. Current CPA has conditional safe-takeover preservation; reproduce that only after matching its ownership guarantees.

An ambiguous post-send dispatch failure may have consumed concurrency. Do not blindly repeat it. Fence/abort the unsafe lifetime and reconcile through the lifecycle protocol.

Sources:
- `AI48Home/internal/respserver/subscribe/config.go`
- `AI48Home/internal/respserver/subscription_liveness.go`
- `AI48Home/internal/cluster/membership.go`
- `CLIProxyAPI/internal/home/client.go`
- `CLIProxyAPI/sdk/cliproxy/service_home.go`
- `CLIProxyAPI/sdk/cliproxy/service_home_retry.go`

## 4. Configuration must control real behavior

Home stores canonical v8 config but sends a **sanitized flattened compatibility projection** through `GET config` and the subscription. Do not deserialize only the nested management API document and assume the wire is identical.

Home removes local management, access keys, auth-directory, TLS, credential groups, and local alias tables from the downstream projection. Credentials and resolved model identity arrive through dispatch. The gateway must preserve provider-specific auth attributes/metadata rather than infer credentials from config.

Current CPA hot reload replaces the remote configuration while retaining running `Host`, `Port`, `TLS`, and `Home`. Initial startup chooses listener settings; later snapshots do not rebind the listener. CPA forces usage enabled, local cooling disabled, cooldown persistence disabled, remote management disabled, control panel disabled, and local KV disabled.

Proposed configuration processing:

```text
wire YAML
  -> normalization (legacy projection; canonical v8 if supported)
  -> typed decoding with absent/false/zero distinction
  -> validation + supported-feature evaluation
  -> staged transports/executors/catalog/policy
  -> immutable generation commit
  -> readiness / dispatch publication
```

Reject duplicate YAML keys. Keep Home lifecycle revision separate from local config generation. Obsolete subscription/reload tasks must not overwrite a newer owner. An invalid update must not partially mutate active settings; retain the prior valid snapshot only while its membership/lifecycle remains safe, otherwise fence readiness.

| Configuration family | Wire/legacy examples | Required runtime consumer |
| --- | --- | --- |
| Retry | `request-retry`, `max-retry-credentials`, `max-retry-interval` | Retry coordinator; exclusion set; release-before-redispatch |
| Transport | `proxy-url`, provider proxy overrides, connection pool settings | Per-selected-credential upstream transport |
| Streaming | `streaming`, nonstream keepalive | SSE/stream lifecycle and pre-output retry handling |
| Payload | `payload` | Final semantic payload barrier |
| Concurrency | `credential-concurrency` | Subscription revision, cancellation, release ledger/backoff |
| Observation | `credential-in-flight` | Snapshot interval, size limits, multipart/overflow |
| Provider/OAuth options | Provider settings and retained OAuth scopes | Selected executor, preserving API-key vs OAuth scope |
| Model capabilities | Catalog settings plus dispatch `model_info` | Thinking, limits, translation and feature validation |
| Logs/usage | Downstream observability settings | Forwarders and execution accounting |
| Plugins | Plugin settings/sync plan | Explicit supported ABI, or a clear unsupported-feature failure |

Each supported setting needs a test proving changed runtime behavior. Parsing and storing a field is not support. Maintain a supported/unsupported configuration inventory; do not silently ignore a setting that changes execution, security, admission, or billing. Broad provider/feature support remains to be selected, not assumed.

Sources:
- `AI48Home/internal/home/config_yaml.go`
- `CLIProxyAPI/internal/config/config_v8.go`
- `CLIProxyAPI/internal/config/parse.go`
- `CLIProxyAPI/internal/config/credential_concurrency.go`
- `CLIProxyAPI/internal/config/credential_in_flight.go`
- `CLIProxyAPI/sdk/cliproxy/service_config.go`
- `CLIProxyAPI/sdk/cliproxy/service_home.go:164-222`

## 5. Command contract inventory

All examples below are RESP argument arrays, not Redis queue semantics.

| Operation | Arguments | Important response/semantics |
| --- | --- | --- |
| Config | `GET config` | YAML bulk string |
| Dispatch | `RPOP <JSON type=auth>` | JSON auth selection or structured JSON error |
| Auth validation | `RPOP <JSON type=auth-validate>` | `authenticated`, principal/provider or error |
| Models | `GET <JSON type=models>` | Auth-scoped provider buckets; **not** literal `GET models` |
| Refresh | `GET <JSON type=refresh>` | Home-owned refreshed/sanitized credential |
| Release | `LPUSH concurrency-release <JSON>` | Integer acknowledgement; cumulative sequence |
| Observation | `LPUSH in-flight-snapshot <JSON>` | Integer ingestion acknowledgement |
| Usage | `LPUSH usage <JSON>` | Persistence/feedback acknowledgement |
| Request log | `RPUSH request-log <JSON>` | Separate from usage and billing |
| App log | `RPUSH app-log <JSON>` | Optional forwarding |
| Discovery | `CLUSTER NODES` | Live Home nodes |
| Rebalance | `CLUSTER BALANCE ENABLE/CLAIM/RESULT/LATENCY ...` | Plan/term/lifetime-bound migration |
| Shared KV | `GET`, `SET`, `SETNX`, `CAS`, `DEL`, `MGET`, `MSET`, `INCRBY`, `EXPIRE`, `TTL` | Needed by selected replay/cache features; no local fallback on Home error |
| Plugin synchronization | `GET plugin-tasks`, `GET plugin-sync <JSON>`, `RPUSH plugin-status <JSON>` | Optional, requires separate ABI compatibility audit |

Do not implement Home-to-Home `CLUSTER PING/NODE/REFRESH` as proxy operations or request Home node secrets.

### Dispatch

```json
{
  "type": "auth",
  "model": "requested-model",
  "headers": {"Authorization": "Bearer <downstream-key>"},
  "concurrency_protocol": 1,
  "retry_round": 0,
  "excluded_auth_ids": [],
  "count": 1
}
```

Optional context includes `pinned_auth_id`, `credential_policy`, `session_id`, `parent_session_id`, and `node_kind`. Presence of `excluded_auth_ids`, even empty, selects newer retry semantics. Preserve omission versus empty. Never log forwarded authorization material.

Selection contains resolved `model`, `provider`, `request_retry`, `auth_index`, `user_api_key`, `auth`, and optionally `model_info`, `force_mapping`, `original_alias`, and `concurrency`. Use the outer `auth_index` for normal dispatch. Do not confuse an alias with the canonical concurrency counter model.

An accounted selection supplies:

```json
{"accounted":true,"credential_id":"counter-id","model":"counter-model"}
```

Validate the selection before executing. Unknown required providers/features must produce explicit errors, not an unrelated executor fallback.

### Release and observation

```json
{"credential_id":"counter-id","model":"counter-model","release_seq":1}
```

`release_seq` is cumulative per counter and owning lifetime, not a delta. Duplicate delivery is safe. Complete each execution exactly once; retain release ownership through delivery failures. Await the prior accounted attempt's release acknowledgement before redispatch. Usage and observation do not release concurrency.

In-flight frames use `kind=part` or `kind=overflow`, monotonically ordered revisions, UTC timestamps, observation barrier, accounted/unaccounted aggregates, and bounded optional details. Multipart frames must agree on shared metadata. Retain encoded frames for consistent retries. Apply limits from config rather than hardcoded defaults.

### Usage

Retain stable `execution_id` across report redelivery, RFC3339Nano-compatible timestamp, requested alias vs execution model, credential index, downstream principal, request/session lineage, failure status, latency/TTFT, and accurate token accounting. Current CPA also supports canonical token accounting schema v2, including nonoverlapping input buckets and separate reasoning output. Preserve unknown/absent values instead of guessing zero.

Usage is scheduler feedback as well as billing data. Current CPA uses an in-memory retention-limited queue; it is not durable exactly-once delivery. A durable Rust outbox is a possible improvement, not existing parity, and would require explicit sensitive-data/storage policy.

Sources:
- `CLIProxyAPI/internal/home/requests.go`
- `CLIProxyAPI/internal/home/concurrency_release.go`
- `CLIProxyAPI/sdk/cliproxy/auth/home_selection.go`
- `CLIProxyAPI/sdk/cliproxy/auth/home_concurrency.go`
- `CLIProxyAPI/sdk/cliproxy/usage/accounting.go`
- `CLIProxyAPI/internal/redisqueue/plugin.go`
- `AI48Home/internal/respserver/pop/dynamic/dispatch.go`
- `AI48Home/internal/respserver/get/default.go`
- `AI48Home/internal/respserver/push/`

## 6. Proposed Rust module boundaries

| Module | Responsibility |
| --- | --- |
| `bootstrap` | CLI/env, identity storage, enrollment, CA pinning |
| `home::resp` | Bounded incremental RESP2 codec and typed replies |
| `home::transport` | TLS connections, isolated subscription/worker/release paths |
| `home::membership` | Health, lifetime state machine, fencing, reconnect/takeover/drain |
| `home::cluster` | Discovery, failover, rebalance claim/result and RTT telemetry |
| `config` | Wire normalization, typed validation, feature support, immutable generations |
| `execution` | Pending/active registry, scope ownership, cancellation and completion |
| `home::release` | Lifetime-scoped cumulative release ledger and acknowledged delivery |
| `home::observation` | Bounded in-flight multipart snapshots and barriers |
| `gateway` | HTTP routes, Home readiness gate and downstream context |
| `dispatch` | Home auth/model resolution, retry/exclusion/affinity coordination |
| `providers` | Selected provider executors and credential-specific transports |
| `protocol` | Request/response/SSE format-pair translators with request-local state |
| `thinking` | Canonical reasoning config, suffix precedence, validation and provider mapping |
| `payload` | Configured rules applied exactly once after all built-in mutation |
| `reporting` | Usage attribution/delivery and optional request/app logs |
| `home::kv` | Required shared replay/cache operations |

Use structured Tokio tasks, cancellation tokens, and explicit asynchronous completion. `Drop` alone cannot guarantee release acknowledgement. A shared immutable snapshot mechanism is appropriate; specific crates and versions are not selected yet.

Pipeline:

```text
HTTP context + requested model
  -> readiness and pending-dispatch tracking
  -> Home-authoritative selection
  -> validated owned execution scope
  -> protocol translation + built-in normalization + thinking
  -> configured payload rules ONCE
  -> transport framing/signature/serialization
  -> upstream execution / response translation / streaming
  -> completion + usage + acknowledged release
```

No post-payload business mutation may restore filtered fields or overwrite configured values, including retry, fallback, WebSocket, image, and token-count paths. Do not impose broad upstream read/stream timeouts after connection establishment; distinguish connection/credential acquisition from lifecycle cancellation and documented liveness exceptions.

Current routes and executors are directly assembled; do not assume a generic Amp module framework from older repository descriptions. Route registration does not imply the associated provider or translation pair is supported. The native plugin implementation uses a versioned C ABI, not Go's `plugin` package; Rust support is plausible but unverified.

Sources:
- `CLIProxyAPI/internal/api/server_routes.go`
- `CLIProxyAPI/internal/api/server_middleware.go`
- `CLIProxyAPI/sdk/cliproxy/service_executors.go`
- `CLIProxyAPI/sdk/translator/registry.go`
- `CLIProxyAPI/internal/thinking/`
- `CLIProxyAPI/internal/runtime/executor/helps/payload_finalizer.go`
- `CLIProxyAPI/sdk/pluginabi/`

## 7. Implementation milestones and acceptance gates

1. **Control-plane foundation:** bootstrap, RESP codec, mTLS, config normalization, membership, readiness and cancellation. Gate: real Home integration proves registration, healthy subscription, fresh controlled worker connections, invalid revision handling and disconnect fencing.
2. **Admission correctness:** execution registry, cumulative release, ambiguous dispatch handling, in-flight snapshots. Gate: exact-once scope completion, lost release ACK retry without double decrement, saturation errors, no redispatch before release ACK, no old-lifetime release on a replacement membership.
3. **First usable provider:** select a concrete deployed provider/protocol pair; implement nonstream + SSE, payload/thinking behavior, usage and Home model listing. Generic OpenAI compatibility is a proposed smaller starting point, not a chosen requirement. Gate: real Home + mock upstream end-to-end tests verify authorization, config-controlled payload/transport/retry behavior, usage deduplication, stream cancellation and token attribution.
4. **Cluster recovery:** failover, safe takeover, drain, rebalance and telemetry. Gate: two-Home tests cover stale plans, owner changes, pending dispatch during loss, cancellation bounds and no stale config publication.
5. **Additional provider/protocol coverage:** expand through explicit capability/config matrix and golden fixtures. Codex Responses/WebSocket sessions need separate retained-selection and multi-turn billing tests.
6. **Optional features:** replay KV, log forwarding, plugin ABI and multimedia/realtime only as required by deployment. Never advertise unavailable functionality.

Reuse existing contracts as test specifications, especially:
- `CLIProxyAPI/internal/home/client_release_lifecycle_test.go`
- `CLIProxyAPI/internal/home/client_fence_test.go`
- `CLIProxyAPI/internal/home/in_flight_contract_test.go`
- `CLIProxyAPI/sdk/cliproxy/auth/home_retry_contract_test.go`
- `CLIProxyAPI/sdk/cliproxy/auth/home_fallback_audit_test.go`
- `CLIProxyAPI/sdk/cliproxy/auth/home_usage_accounting_test.go`
- `CLIProxyAPI/sdk/cliproxy/auth/home_websocket_reuse_test.go`
- `CLIProxyAPI/internal/runtime/executor/helps/payload_finalizer_test.go`

Initially exclude standalone credential stores, local OAuth login flows, local scheduler/cooldown persistence, TUI, embedded management UI, filesystem watchers, local KV, and deprecated management APIs. This reduces breadth, not the admission/lifetime/accounting guarantees.

## 8. Outstanding decisions and validation status

- Confirm the first provider and client-facing protocol pair from deployment requirements.
- Confirm whether the initial release must include native plugins, Codex WebSocket, or media APIs.
- Decide Rust identity storage location and whether certificate-cache compatibility with CPA is desired; do not overwrite CPA identities by default.
- Decide strictness for unsupported execution settings and how that status is surfaced to operators.
- Select dependency versions and CI targets during implementation.

The initial Rust implementation now builds and has 79 passing local tests, including mock Home mTLS registration/dispatch/release, config fencing, heartbeat semantics, mock-upstream HTTP forwarding, and SSE disconnect completion. Formatting and strict Clippy checks pass. No live Home interoperability or full CLIProxyAPI parity has been established. Enrollment currently follows CPA's plaintext RESP bootstrap; normal communication uses mTLS. Protect the enrollment network path. Automatic failover/takeover, OAuth execution, durable reporting and full provider token/billing attribution remain outstanding.

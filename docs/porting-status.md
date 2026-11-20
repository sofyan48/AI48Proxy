# Implementation status

This document tracks AI48Proxy capabilities and acceptance boundaries. The filename
is retained for existing links. It is not a declaration that every provider or
configuration is production-ready.

## Implemented areas

| Area | Current implementation |
| --- | --- |
| Home transport | RESP2 command connections, certificate enrollment and verified mTLS |
| Configuration | Subscription updates, typed normalization, immutable snapshots and explicit unsupported-setting validation |
| Membership | Heartbeat fencing, reconnect recovery, discovery, conditional takeover and planned replacement-connection rebalance; active work remains origin-owned |
| Balance telemetry | Negotiated `cpa_latency_swap_v1` subscription RTT sampling, `CLUSTER BALANCE LATENCY` reports/probes and randomized equal-load node choice |
| Execution ownership | Home admission, cumulative releases, in-flight snapshots, cancellation and ambiguous-dispatch fencing |
| Usage | Durable accepted-event outbox, current-Home restart replay, process-local origin routing during handoff, supported token/image fields and optional `media_counts` |
| Logs | Bounded application logs; opt-in generation request logs submitted once at body completion/error/disconnect (499), with provider attribution |
| OpenAI-compatible upstreams | Home-selected custom bases and credentials, Chat execution and Responses bridges |
| Codex | HTTP Responses, constrained WebSocket sessions, HTTP scoped cache identity before payload rules and supported image execution |
| Claude | Messages, Chat translation, focused OAuth wire preparation, final-body attribution signing and HTTP failure classification |
| Google-format upstreams | Gemini, recognized Gemini Interactions identity, Vertex native/OpenAPI routing, Antigravity envelopes and supported function-tool translation |
| New text providers | xAI and Meta native Responses/Chat bridges; Kimi and `kimi-ai` native Chat/Responses with basic tools/history; unsupported sources/replay fail closed |
| Media | Supported images, xAI image completion SSE, xAI speech and JSON/multipart xAI video generation/edit/extension/retrieval with OpenAI-style facade and Home-KV credential binding |
| Media lifecycle | Shared Home-owned refresh, eligible retry, request-scoped actions, cancellation and release-before-redispatch; video retrieval stays credential-pinned |
| Runtime controls | Payload rules, bounded original-byte request-scoped error matching, reasoning controls, response filtering, pre-header SSE keepalives, shutdown bounds and image-generation modes |

See the [provider guide](provider.md) for request-level restrictions and the
[Home integration guide](home-compatibility.md) for lifecycle ownership.

## Acceptance boundaries

Automated tests cover synthetic credentials, real loopback mTLS/RESP connections,
local upstream HTTP/WebSocket fixtures, payload ordering and lifecycle ownership.
These checks do not demonstrate that a live provider accepts a particular account,
model, tool schema or billing configuration.

This status describes source wiring, not live acceptance or a record that the
newest source is deployed. Verify running revisions, provider account state,
balances, credentials and Home accounting in the intended deployment. Private
account details do not belong in public documentation.

## Remaining work

- Native plugin execution and Gemini CLI execution. Home snapshots with
  `plugins.enabled: true` are rejected; `gemini-cli` and `aistudio` remain skipped
  because their plugin/relay subsystems are not enabled in this worker.
- General realtime workflows, xAI WebSockets, video streaming and non-xAI video
  execution; `gemini-interactions` is recognized but its specialized executor is
  still fail-closed.
- WebSocket proxy transport, full duplex steering and automatic recovery.
- Advanced Claude helper/workload profiles, tool alias reversal, continuity/date
  pinning and signed reasoning replay.
- Advanced Antigravity signed replay, complex schemas, incremental tool arguments
  and multimedia behavior.
- Remaining provider-specific controls, specialized Vertex operations and
  cross-format conversions.
- Broader xAI/Kimi/Meta source bridges, advanced reasoning replay and stateful
  continuation/compaction; Meta subscription attestation.
- Deployment qualification of live handoff/rollback, negotiated latency telemetry,
  keepalive/error/disconnect behavior, media binding expiry and billing dimensions.
  These are validation tasks, not claims that the source wiring is absent.

Devin is intentionally out of scope, not a pending supported executor. Planned
rebalance does not transfer active execution ownership: releases/usage stay bound
to the origin during the process lifetime. Origin routing is not persisted, so
restart usage replay uses the current Home, as in the reference worker.

Only remove a capability restriction when request preparation, response handling,
configuration, execution lifecycle and accounting are implemented and tested.
Never infer readiness solely from a registered route, successful compilation or a
healthy container.

## Release checklist

1. Run locked Cargo tests, strict Clippy and formatting checks.
2. Build the deployment image and record the deployed revision.
3. Verify enrollment and membership on a protected network.
4. Test selected providers with authorized models and synthetic prompts.
5. Confirm usage, concurrency releases, logs and billing in Home.
6. Exercise cancellation, disconnection, restart and bounded shutdown.
7. Record untested and unsupported features explicitly using the
   [acceptance runbook](worker-acceptance.md).

Compatibility references and attribution are documented in the
[project README](../README.md#compatibility-references-and-attribution).

# Implementation status

This document tracks AI48Proxy capabilities and acceptance boundaries. The filename
is retained for existing links. It is not a declaration that every provider or
configuration is production-ready.

## Implemented areas

| Area | Current implementation |
| --- | --- |
| Home transport | RESP2 command connections, certificate enrollment and verified mTLS |
| Configuration | Subscription updates, typed normalization, immutable snapshots and explicit unsupported-setting validation |
| Membership | Heartbeat fencing, reconnect recovery, discovery, conditional takeover and drained-worker rebalance |
| Execution ownership | Home admission, cumulative releases, in-flight snapshots, cancellation and ambiguous-dispatch fencing |
| Usage | Durable accepted-event outbox, replay, bounded admission and supported token/image accounting fields |
| Logs | Bounded application-log forwarding and opt-in sanitized request metadata forwarding |
| OpenAI-compatible upstreams | Home-selected custom bases and credentials, Chat execution and Responses bridges |
| Codex | HTTP Responses, constrained WebSocket sessions, cache identity helpers and supported image execution |
| Claude | Messages, Chat translation, focused OAuth wire preparation, final-body attribution signing and HTTP failure classification |
| Google-format upstreams | Gemini, Vertex native/OpenAPI routing, Antigravity envelopes and supported function-tool translation |
| Media | Supported image generation/editing and xAI speech; unsupported combinations fail explicitly |
| Runtime controls | Payload rules, supported reasoning settings, response filtering, keepalives, shutdown bounds and image-generation modes |

See the [provider guide](provider.md) for request-level restrictions and the
[Home integration guide](home-compatibility.md) for lifecycle ownership.

## Acceptance boundaries

Automated tests cover synthetic credentials, real loopback mTLS/RESP connections,
local upstream HTTP/WebSocket fixtures, payload ordering and lifecycle ownership.
These checks do not demonstrate that a live provider accepts a particular account,
model, tool schema or billing configuration.

Previous development sessions exercised live Codex SSE successfully. That is a
point-in-time observation, not an availability guarantee. Claude testing was
blocked by Home credential availability at the last recorded live check. Provider
account state, balances and credentials must be verified in the deployment itself;
private account details do not belong in public documentation.

The latest attempted Docker rebuild/start was interrupted by the operator. Do not
assume that the newest source has been deployed. Check the running image and worker
health before testing changes.

## Remaining work

- Native plugin execution and Gemini CLI execution.
- Full realtime/video workflows and credential-bound video retrieval.
- WebSocket proxy transport, overlapping turns, advanced steering and recovery.
- Advanced Claude helper/subagent profiles, tool alias reversal, continuity/date
  pinning and signed reasoning replay.
- Advanced Antigravity signed replay, complex schemas, incremental tool arguments
  and multimedia behavior.
- Remaining provider-specific controls, specialized Vertex operations and
  cross-format conversions.
- Active-execution migration during rebalance and latency-based Home selection
  optimizations.
- Complete configured end-to-end validation for new runtime controls, request-log
  completion/cancellation semantics and all media billing dimensions.

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

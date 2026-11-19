# AI48Proxy

Rust/Tokio AI gateway operating as an AI48Home-managed data-plane worker.

## Status

An initial executable implementation, **not a full CLIProxyAPI replacement**. Local tests exercise mock Home over real mTLS and a mock HTTP upstream. Live Home/deployment interoperability and provider OAuth parity have not been validated. Do not replace production CPA nodes with this release yet.

### Implemented

- Bounded RESP2 codec, certificate enrollment and private identity storage, mTLS command connections without Redis initialization or HTTP ALPN.
- Dedicated configuration subscription, lifecycle revision registration, fresh worker connections, heartbeat-based readiness/fencing and active-request cancellation.
- Typed flattened/v8 configuration normalization, duplicate-key/value validation, supported-setting diagnostics and immutable hot-reload snapshots.
- Home-authenticated dispatch and model listing; accounted execution scopes, cumulative releases, ACK-before-redispatch and in-flight snapshots.
- Durable bounded usage outbox under the identity directory, with stable execution IDs, checksummed records, atomic publication, fsync, exclusive locking, replay after restart and persistent ACK deletion. Queue exhaustion fences admission instead of silently evicting records.
- Same-format API-key JSON/SSE forwarding for OpenAI-compatible, Claude native and Gemini native upstreams. Credential-specific HTTP(S) proxies, final payload rules and capability-validated thinking configuration.
- Typed Home KV and cluster command helpers, exposed as library modules. Rebalance is not automatically enabled.
- Integrated OpenAI Chat ↔ Claude Messages request/response translation, including incremental SSE text/tool/usage conversion.
- Codex HTTP Responses and Chat-to-Responses conversion with OAuth bearer/account headers, terminal JSON extraction, text/reasoning/tool SSE conversion, and source usage accounting. Claude OAuth fingerprint/identity handling and Home-only refresh after 401.
- Native upstream Responses WebSocket with a retained Home concurrency lease, per-turn usage, bounded session-history hydration and cancellation. Proxy transport, model changes within a session, overlapping creates and advanced steering remain unsupported.
- OpenAI-compatible Responses-to-Chat upstream conversion with JSON/SSE conversion back to Responses; direct compact endpoint preparation; Home-selected token-field/model options and prompt-cache session-header precedence.
- Automatic normal-registration recovery with exponential jitter, discovered alternate Home targets, fresh lifetime registries and a persistent HTTP listener returning 503 while disconnected. No unsafe takeover is attempted.
- Token v2 source accounting for supported OpenAI, Claude and Gemini usage fields; partial or inconsistent counts remain explicit.
- Context-aware payload rules: source protocol/header/body gates, original requested model, ordered literal/regex replacements and final filters.
- Vertex native Gemini and OpenAPI Chat routing with Home-supplied token/API-key credentials. Antigravity native Gemini envelope execution and JSON/SSE response/usage extraction, subject to the limitations below.
- Native Claude API-key `/v1/messages/count_tokens`, without treating token estimates as inference usage.
- Configured inbound body-size enforcement before dispatch, including chunked requests.
- CI verification, non-root container packaging and opt-in worker acceptance probes; see [worker acceptance](docs/worker-acceptance.md). Container execution still requires validation in your deployment.

### Not implemented / limitations

- Full Codex WebSocket/continuation parity (including proxy transport, overlapping turns and advanced steering), Gemini CLI, plugin runtime, multimedia/realtime and full cross-format translation remain unavailable. Antigravity signature replay/advanced tools/images are not implemented. Vertex requires Home-prepared credentials; no local service-account exchange is performed. Provider implementations are subsets, not full CPA parity.
- Membership takeover and planned rebalance migration are not implemented. Recovery waits for normal Home admission; targets are not persisted across process restarts. The HTTP listener starts after the first valid Home configuration.
- Durable reporting covers events accepted by the outbox, not usage never observed before a crash or events rejected for capacity/persistence errors. Full provider billing parity remains unvalidated.
- Complete configuration parity: active unsupported execution settings reject startup/update explicitly. Advanced provider knobs and streaming keepalive controls are examples. Unknown metadata is not a promise of execution support.
- Connection pooling: controlled commands currently open fresh mTLS connections, prioritizing lifetime isolation over throughput.
- Graceful drain is bounded; unresolved admission/usage/release causes a nonzero exit. External supervision must not assume restart is a safe membership takeover.

## Build and verify

```sh
cargo build --locked
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

`Cargo.lock` pins dependency resolution. No Go repositories are modified by this application.

## Full Docker stack (same topology as Home installer)

The repository includes `docker-compose.yml` for PostgreSQL + Home + three Rust workers + nginx. Local `.env`, bootstrap configuration, and independent worker JWTs are managed by:

```sh
./install.sh init      # Generate private .env/config files; preserve existing files
./install.sh deploy    # Build, import a fresh Home, issue JWTs, start the stack
./install.sh status
./install.sh logs cliproxy-1
```

Home publishes `8327`; nginx publishes gateway port `8317`. Stop an older installer stack first because container names, ports and `cliproxy-network` intentionally match. This does not automatically migrate an existing database. See [Docker deployment](docs/docker.md).

## Run

Provision a client certificate token in AI48Home through `POST /v8/management/certificates/clients`. Supply it through the `HOME_JWT` environment variable using your secret manager, then:

```sh
cargo run --locked -- --identity-dir .ai48proxy
```

**Enrollment warning:** current CPA-compatible enrollment sends the one-time enrollment secret over plaintext RESP. The returned CA fingerprint is checked, but that does not encrypt the secret or prevent interception/consumption races. Use only an operator-protected network path or tunnel for enrollment. Subsequent gateway/Home communication uses verified mTLS. The JWT is treated as a trusted provisioning capability; its signature is not locally verified.

Identity files are separate from CPA's default identity directory. On Unix, the directory and files use `0700` and `0600`. Never commit the identity or reuse one certificate concurrently across processes. An uncertain enrollment result blocks automatic reenrollment; operator recovery is required.

For an existing identity, without setting `HOME_JWT`:

```sh
cargo run --locked -- --identity-dir .ai48proxy --home-host 127.0.0.1 --home-port 8327
```

Listener host/port come from Home (`8317` when port is absent); later config updates retain the running listener. No standalone credential/config fallback is available.

Validate a saved downstream Home snapshot without starting:

```sh
cargo run --locked -- --check-config home-snapshot.yaml
```

### HTTP routes

| Route | Support |
| --- | --- |
| `GET /healthz` | 200 only when Home membership is ready; otherwise 503 |
| `GET /v1/models` | Home-authenticated catalog, normalized to OpenAI list |
| `POST /v1/chat/completions` | OpenAI-compatible forwarding, Claude translation or Codex Responses bridge |
| `POST /v1/completions` | Same-format OpenAI-compatible API-key forwarding |
| `POST /v1/responses` | Compatibility Chat bridge or Codex HTTP, JSON/SSE |
| `GET /v1/responses` | Responses WebSocket; constrained continuation support |
| `POST /v1/responses/compact` | Native compact endpoint; specialized capsule parity not established |
| `POST /v1/messages` | Claude API-key/OAuth or translation to OpenAI Chat |
| `POST /v1beta/models/{model}:generateContent` | Gemini-native API-key forwarding |
| `POST /v1beta/models/{model}:streamGenerateContent` | Gemini-native SSE forwarding |

The provider and execution model are selected by Home. Routes do not imply that every credential/provider/format combination is supported. Unsupported combinations return explicit capability errors. Upstream error bodies are not exposed because they may echo credentials.

See [Home compatibility baseline](docs/home-compatibility.md) for the audited source contract and remaining acceptance milestones.

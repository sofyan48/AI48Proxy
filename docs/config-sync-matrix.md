# AI48Home → AI48Proxy configuration synchronization

This document is the source-of-truth checklist for the sanitized YAML snapshot sent by AI48Home over RESP. The worker always retains the complete normalized YAML in `Config::raw`; typed fields are consumed by the worker only when they affect worker transport, lifecycle, payload, or reporting behavior.

## Synchronization lifecycle

1. AI48Home builds its downstream projection.
2. AI48Proxy fetches `GET config` during registration.
3. AI48Proxy validates and normalizes flattened/v8 layouts.
4. AI48Proxy subscribes with `SUBSCRIBE config <revision> <instance>`.
5. The initial snapshot must have the same lifecycle revision.
6. Supported updates replace the immutable snapshot atomically.
7. Revision changes, malformed YAML, or active unsupported execution settings terminate the current lifetime and require registration.
8. The live projection test runs the real AI48Home sanitizer when the sibling checkout and Go toolchain are available.

## Field matrix

| Home projection family | AI48Proxy handling | Owner/notes |
| --- | --- | --- |
| `host`, `port` | Parsed at registration; retained from the active worker on ordinary reload | Home listener identity; reload requires a new lifetime |
| `request-retry`, `max-retry-credentials`, `max-retry-interval` | Typed and used by retry/dispatch handling | Home selects credentials; worker waits for release before redispatch |
| `proxy-url` | Typed; HTTP/HTTPS upstream proxy supported; SOCKS remains explicitly unsupported for HTTP | Credential proxy overrides are selected from dispatched auth |
| `streaming.*`, nonstream keepalive | Typed and used by response/keepalive paths | Includes pre-header SSE keepalive |
| `payload` | Parsed and applied after built-in provider mutations | Final business-payload barrier |
| `oauth-request-scoped-errors` / OAuth provider rules | Parsed and matched against bounded original upstream error bytes | Home owns cooldown state; worker reports the selected action |
| `disable-image-generation`, image base model, video binding TTL | Typed and used by media paths | Media remains provider/model-specific |
| `credential-concurrency` | Typed and used for lifecycle revision, heartbeat, cancellation and release timing | Home owns admission; worker owns completion/release cooperation |
| `credential-in-flight` | Typed and used for bounded snapshots | Home aggregates observations |
| `request-log`, shutdown/body/TLS handshake settings | Typed and used by observability, bounded drain and request admission | Raw bodies/secrets are never logged |
| `force-model-prefix`, reasoning/provider controls | Preserved in normalized/raw config and consumed only where a provider path implements them | Unsupported active controls are rejected rather than ignored |
| `antigravity` | Supported scalar TOS defaults and selected provider metadata are consumed | Full signed replay/multimedia remains incomplete |
| `quota-exceeded` | Home-owned quota/cooldown policy; worker does not invent local quota state | Provider responses are reported back to Home |
| `models` catalogs | Home-owned catalog; worker uses the dispatched resolved model and `/v1/models` Home response | Worker does not build a credential catalog |
| `plugins` | Typed contract and ABI foundations exist; native plugin execution is not enabled yet | `plugins.enabled: true` is rejected until executor/scheduler/auth/model/usage lifecycle is wired |
| `debug`, logging, file-retention and Home scheduler metadata | Retained in `Config::raw`; these are Home/operational fields and do not change provider payloads | They must not be confused with execution support |
| credential lists (`*-api-key`, OAuth files, `auth-dir`) | Not used as local credential stores | Home dispatch supplies sanitized auth for each request |
| remote management, users, panel, database, cluster secrets | Not distributed as worker execution config | Home-only; never forwarded to upstream |

## Provider/upstream synchronization

The selected Home dispatch carries:

- resolved `provider` and execution `model`;
- `auth_index` and stable credential ID;
- sanitized credential attributes/metadata needed by the selected executor;
- optional `model_info` capabilities;
- downstream principal and concurrency identity;
- retry round, excluded credentials and session/pinning metadata.

AI48Proxy must not replace a selected provider with a different executor merely because a route is registered. Provider-specific unsupported shapes remain explicit until request preparation, response translation, usage accounting and Home release ownership are all implemented.

## Verification commands

From `AI48Proxy`:

```sh
cargo test --locked --lib config::home_projection_tests::live_home_config_projection -- --ignored --nocapture
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --all --check
```

The ignored projection test requires the sibling `AI48Home` checkout and Go dependencies. It compares the real Home sanitizer output with `src/config/home_default_downstream.yaml`; update the fixture only after reviewing the diff and the Home change that caused it.

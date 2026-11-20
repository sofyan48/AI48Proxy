# AI48Home integration architecture

AI48Proxy is the AI48Home-managed gateway data plane. Home owns persistent policy,
credential selection and accounting storage; workers execute authorized requests
and report their outcomes. This document describes the current integration
contract, not a claim of complete provider coverage or deployment acceptance.
Use [worker acceptance](worker-acceptance.md) to validate a particular deployment.

## Ownership and request flow

| Responsibility | Owner |
| --- | --- |
| Configuration, credentials, refresh, scheduling, cooldowns and downstream access policy | AI48Home |
| HTTP ingress, protocol translation, upstream execution, streaming and cancellation | AI48Proxy |
| Concurrency admission and reclamation policy | Home, with worker completion and release cooperation |
| Usage persistence, billing aggregation and scheduler feedback | Home, using worker execution reports |
| In-flight observations | Worker publishes bounded snapshots; Home aggregates |

```text
Downstream request and requested model
  -> ready Home membership and tracked pending admission
  -> Home authorization, model resolution and credential selection
  -> lifetime-owned execution scope
  -> translation, provider normalization and thinking configuration
  -> configured payload rules, once at the final semantic boundary
  -> transport framing, signing and upstream execution
  -> response translation or streaming
  -> completion, usage report and acknowledged concurrency release
```

A Home failure does not enable local credentials, local scheduling, local OAuth
refresh or a local KV fallback. Provider/format combinations are explicit;
unsupported combinations must not silently select another executor. Route
registration alone is not evidence of complete provider support.

## Enrollment, identity and transport

Provision a pending client certificate through authenticated, verified HTTPS
access to `POST /v8/management/certificates/clients`. The resulting Home JWT is a
trusted provisioning capability containing the certificate/cluster identity,
CA fingerprint, one-time enrollment secret and destination `ip`/`port`.

**Initial enrollment is plaintext RESP.** AI48Proxy sends the one-time secret
and CSR in `CERTIFICATE REQUEST <certificate-id> <enrollment-secret> <CSR-PEM>`
to the JWT destination. Protect the entire route with an operator-controlled
encrypted network or transparent tunnel. A tunnel at another destination does
not redirect the JWT; `--home-host` cannot override enrollment routing. The
HTTPS-only non-cluster listener is not a substitute for this enrollment path.

The JWT signature is **not locally verified**. Structural/time checks and checking
the returned CA fingerprint do not encrypt enrollment or prevent interception and
one-time-secret consumption races. Obtain tokens only through trusted provisioning;
never use an unprotected public enrollment route.

Prefer a private secret file through `HOME_JWT_FILE` or `--home-jwt-file`; `HOME_JWT`
is also supported. Do not put tokens in command arguments, logs, Git or image
layers. File and environment token inputs conflict. For an already enrolled
identity, remove bootstrap inputs and use `--home-host` with `--home-port`.

The worker persists its private key before enrollment and records that enrollment
was sent. A lost reply can mean the capability was consumed: preserve the identity
and `enrollment-sent` marker, reconcile/revoke through Home, and obtain operator
approval before reprovisioning. Deleting the marker and blindly retrying is unsafe.
On Unix, identity directories/files use 0700/0600. One identity and reporting
volume must have exactly one process owner; never clone it across replicas.

Normal Home commands use bounded RESP2 over verified mTLS with the pinned CA and
client certificate. Home checks both the certificate chain and its database record.
There is no password `AUTH`, Redis `HELLO`/`SELECT` initialization or HTTP ALPN on
these connections. Home's shared listener distinguishes RESP from HTTP. Business
errors may be encoded inside a successful bulk JSON reply and must be checked.

Implementation: `src/cli.rs`, `src/home/bootstrap.rs`, `src/home/transport.rs` and
`src/home/resp.rs`.

## Membership, readiness and recovery

Startup loads/enrolls the identity, opens the durable reporting store, and discovers
Home targets when available. For registration the worker:

1. Fetches `GET config` over a bootstrap connection and validates support.
2. Opens a dedicated config subscription with the lifecycle revision and process
   instance UUID: `SUBSCRIBE config <revision> <instance-uuid>`.
3. Requires the subscription acknowledgement and initial full config snapshot,
   with the same lifecycle revision.
4. Opens a fresh command connection after acknowledgement and verifies `PING`.
5. Installs a new lifetime-owned execution registry and publishes readiness.

Home assigns command ownership at connection acceptance; the bootstrap connection
is not reused as a controlled dispatch connection. Commands currently open fresh
mTLS connections rather than using a dispatch pool.

The HTTP listener starts only after initial registration. Before that, probes may
see connection refusal. Once started, the listener survives recovery and returns
503 while no ready membership is installed. `GET /healthz` is readiness, not a
process-only liveness check.

Config, cluster discovery and rebalance messages share the config subscription.
Only the ordinary `["pong", ""]` heartbeat renews health; config traffic and
`home-rtt/...` telemetry do not. Subscription failure, heartbeat timeout, invalid
config, unsafe dispatch ambiguity or failed control acknowledgements can fence
admission and cancel active execution. Underlying streams/sockets must close;
merely abandoning a task is not completion.

Recovery drains the old lifetime within configured bounds, retires its tasks and
connections, and registers with exponential jitter and discovered alternate
Home targets. Each accepted registration gets a fresh registry. Old release
counters are never replayed into a replacement registry. Discovered targets are
not persisted: startup still needs a reachable configured/enrollment target.

### Conditional takeover and planned rebalance

Takeover is implemented, but is not permission to preserve live counters across
registries. After acknowledged revision-aware registration, recovery may send
`SUBSCRIBE config <revision> takeover <instance-uuid>`. It suppresses takeover
when the old registry is not drained. Home rejection
`membership_takeover_unavailable` suppresses it; a known subscription argument
rejection selects the legacy form without UUID/takeover. Legacy negotiation does
not waive the requirement for valid lifecycle configuration. Process restart
creates a new instance identity, not automatic safe takeover of its predecessor.

The subscription control path can enable rebalance and process a Home plan.
Migration is **drain-only**: it stops admission and declines to claim while active
executions, pending admissions or unacknowledged releases remain. Once drained,
it claims the plan/term/target, preflights the target, retires the old lifetime and
registers at the destination. The supervisor requires acknowledged rebalance result
reporting before publishing the replacement. Failure paths may report failure,
return to the origin or fence; they do not transfer a live registry. Typed latency
helpers alone do not establish a complete latency-driven migration implementation.

Implementation: `src/home/client.rs`, `src/home/client_lifecycle.rs`,
`src/home/membership.rs`, `src/home/supervisor.rs`, `src/home/rebalance.rs` and
`src/home/cluster.rs`. Exercise takeover, rejection, stale plans and destination
failure against the deployed Home before relying on them operationally.

## Configuration contract

Home's RESP configuration is a sanitized downstream YAML projection, not simply
the Management API document. Credentials and resolved model identity arrive through
dispatch, not local credential tables. AI48Proxy normalizes supported flattened
and canonical v8 paths, rejects duplicate/conflicting values, decodes typed values
and checks known unsupported execution settings before publishing an immutable
snapshot. Unknown metadata retention is not execution support.

A positive `credential-concurrency.lifecycle-config-revision` is required. A
revision change on an existing subscription fences that lifetime and requires
registration; ordinary supported updates replace the snapshot atomically.
Invalid or unsupported updates fence rather than partially apply. Running host/port
are retained on reload; listener changes require a drained process restart.
In-flight work retains its captured configuration.

| Configuration family | Runtime behavior |
| --- | --- |
| Retry and credential limits | Home-coordinated retry, exclusion and release-before-redispatch |
| `proxy-url` and selected credential overrides | HTTP(S) proxy/direct upstream transport; SOCKS is rejected |
| `streaming` and nonstream keepalive | Configured response keepalive and pre-output bootstrap handling |
| `payload` | Context-aware rules after built-in business mutations, before transport/signing |
| `credential-concurrency` | Registration revision, heartbeat/cancel bounds and release timing |
| `credential-in-flight` | Snapshot interval, observation barrier, multipart and cardinality limits |
| Body-size, shutdown and request-log controls | Pre-dispatch body limit, bounded drain and optional sanitized log forwarding |
| Dispatch metadata and model capabilities | Provider auth/routing, thinking validation and execution limits |

Known unsupported active controls reject startup/update. Examples include enabled
plugins, unsupported provider option families, active listener TLS/trusted-proxy
controls and positive configured TLS-handshake timeout. Do not generalize this to
all unknown fields or treat successful parsing as provider acceptance.
`--check-config` validates a saved downstream snapshot's syntax/support, not Home
connectivity, credentials or upstream behavior. See `src/config.rs`,
`src/config/wire.rs`, `src/config/unsupported.rs` and `src/home/config_validation.rs`.

Payload rules are the final business-payload boundary, including retry, WebSocket
and supported media paths. Later framing/signing must not restore filtered fields
or override configured values. Connection acquisition and lifecycle cancellation
are distinct from broad request/read deadlines; long upstream streams must not
acquire an undocumented whole-request timeout.

## RESP operations and execution accounting

These are custom RESP arguments, not generic Redis queue semantics.

| Operation | Arguments | Contract |
| --- | --- | --- |
| Configuration | `GET config` | Downstream YAML bulk string |
| Membership | `SUBSCRIBE config ...` | Acknowledgement, full snapshot and control messages |
| Dispatch | `RPOP <JSON type=auth>` | Authorized selection or structured business error |
| Authorization | `RPOP <JSON type=auth-validate>` | Downstream principal validation |
| Catalog | `GET <JSON type=models>` | Auth-scoped provider buckets, not literal `GET models` |
| Refresh | `GET <JSON type=refresh>` | Home-owned credential refresh |
| Release | `LPUSH concurrency-release <JSON>` | Cumulative lifetime-scoped sequence acknowledgement |
| Observation | `LPUSH in-flight-snapshot <JSON>` | Bounded snapshot ingestion acknowledgement |
| Usage | `LPUSH usage <JSON>` | Persistence/feedback acknowledgement |
| Request log | `RPUSH request-log <JSON>` | Separate optional contract, not billing evidence |
| Discovery | `CLUSTER NODES` | Available Home targets |
| Rebalance | `CLUSTER BALANCE ENABLE/CLAIM/RESULT ...` | Plan/term/ownership checks; latency helpers also exist |
| Shared KV | Typed operations in `src/home/kv.rs` | Home-backed state for supported consumers, no local fallback |

Optional app-log/plugin synchronization contracts in Home do not imply that this
worker implements their forwarders or plugin execution. Workers do not perform
Home-to-Home control operations or request Home node secrets.

Dispatch includes requested model, downstream authorization, concurrency protocol,
retry round, exclusions and supported session/pinning context. Preserve omission
versus an empty exclusion list where the Home retry contract distinguishes them.
Never log forwarded authorization. Selection supplies the resolved `model`,
`provider`, outer `auth_index`, principal, sanitized `auth`, optional `model_info`
and concurrency identity. Preserve selected provider metadata; do not confuse the
requested alias, execution model and canonical concurrency counter model.

An accounted selection identifies `credential_id` and counter `model`. Completion
increments that counter's lifetime-owned cumulative `release_seq`, not a delta.
Duplicate release delivery must not decrement twice. Redispatch waits for the
prior accounted attempt's release ACK. A post-send dispatch error may already
have consumed concurrency and must fence rather than blindly retry. Usage and
in-flight observations never release slots.

In-flight snapshots contain ordered revisions, UTC timestamps, observation barrier,
accounted/unaccounted aggregates and bounded optional details. Multipart metadata
must be consistent. Overflow/staleness must remain visible rather than appear as
a complete observation. Release and snapshot state are not durable across process
restart; Home reconciles/reclaims the old membership.

### Durable usage and its limits

Usage reports retain a stable `execution_id` across redelivery, request/session
lineage, alias/execution model, credential/principal attribution, status,
latency/TTFT and supported schema-v2 token buckets. Missing/partial observations
remain explicit; overlapping input/reasoning buckets must not be double counted.
Usage also feeds Home scheduling decisions.

The usage outbox lives under the private identity directory. It uses bounded,
checksummed records, atomic publication/fsync, exclusive ownership, restart replay
and persistent ACK deletion. Default limits are 4,096 pending items, 16 MiB of
queued payloads and 256 KiB per payload, with a bounded recent-ID cache. These
library defaults are not a promise of runtime tuning flags. Capacity or storage
failure fences admission rather than evicting accepted records. Preserve corrupt
records for investigation; do not delete the reporting directory to force readiness.

Durability covers **only events accepted by the outbox**. It cannot reconstruct
usage never observed before a crash or events rejected for persistence/capacity
errors. Release delivery is a separate lifetime-bound mechanism. Home deduplication
must be verified for replay and stale backups; this is not end-to-end exactly-once
billing or complete provider billing equivalence. See `src/execution.rs`,
`src/reporting.rs` and `src/reporting/`.

## Compatibility references and validation

The deployed AI48Home handlers and wire schemas are authoritative. Relevant Home
sources include `internal/home/config_yaml.go`, `internal/home/auth_sanitize.go`,
`internal/respserver/subscribe/config.go`, `internal/respserver/subscription_liveness.go`,
`internal/respserver/pop/dynamic/dispatch.go`, `internal/respserver/push/` and
`internal/cluster/` membership/rebalance handlers. CLIProxyAPI remains an external
compatibility reference where a shared wire or provider behavior needs comparison;
that does not establish feature equivalence or automatic version synchronization.

[Implementation status](porting-status.md) records a dated checkpoint, including
limited reported live results. It is not blanket deployment acceptance; historical
feature inventories can lag current source. Revalidate the exact
Home/worker revisions and required provider subset using the acceptance runbook.
No full compatibility, failover, billing reconciliation or live acceptance result
is asserted by this architecture document.

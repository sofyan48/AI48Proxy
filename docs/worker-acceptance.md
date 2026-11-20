# AI48Proxy deployment acceptance runbook

## Status and scope

This is a reproducible acceptance procedure, **not evidence of live success**.
No live enrollment, provider success, failover, billing reconciliation or container
deployment result is asserted by this procedure. Local mocks cannot establish
those properties. Keep the existing production deployment serving traffic until
the applicable gates pass for the intended workload.

Run commands from the AI48Proxy root. See [integration architecture](home-compatibility.md)
for membership, configuration and accounting contracts, and
[implementation status](porting-status.md) for a dated checkpoint. Historical live
results in that checkpoint do not satisfy these gates for another deployment.
Record AI48Proxy and AI48Home revisions, `rustc --version`, `cargo --version`,
OS/architecture, image digest, configuration revision, enabled providers and UTC
test window privately. If an external compatibility comparison is needed, record
that implementation's revision separately. There is no automatic source-version
synchronization or full-feature compatibility guarantee. A Home revision or
provider feature change requires rerunning the affected gates.

## 1. Credential-free local gate

Prerequisites: current stable Rust with rustfmt/clippy, a C build toolchain, and
network access for locked Cargo dependencies. The edition is Rust 2024; do not
substitute an old Rust image. From the repository root:

```sh
sh scripts/verify-worker.sh
```

The script checks shell syntax, formatting, warnings-as-errors Clippy, all-target
tests, and a release build. Cargo dependency-consuming commands use `--locked`.
Tests use temporary identities/mock Home mTLS/mock upstreams; no real keys or
running Home are needed. Tests cover registration/fencing, HTTP JSON/SSE,
release acknowledgement, durable outbox replay/corruption/locking, and normal
supervisor recovery. Membership/rebalance tests must also be checked against the
current source; local fixtures do not prove a multi-Home deployment. Inspect actual
test output; a green build is not deployment acceptance. An unrelated source failure is a failed gate, not permission to skip it.

`.github/workflows/worker.yml` runs this gate on push, pull request and manual
invocation, then builds the Docker image and checks its user and executable
without networking. CI has no deployment credentials or live acceptance steps.

## 2. Container gate

```sh
docker build --pull --tag ai48proxy:acceptance .
docker run --rm --network none --read-only ai48proxy:acceptance --version
docker image inspect --format '{{.Config.User}}' ai48proxy:acceptance
```

Expected user: `10001:10001`. The multi-stage build uses current stable Rust on
Bookworm and a Bookworm slim runtime with CA roots, without Rust/Cargo. The
allowlisted build context includes only manifests, source and Docker metadata;
never add identities, environment files or saved Home configuration to it.

`Cargo.lock` fixes dependency resolution, not compiler/base-image bytes. The
stable image and Debian package repositories move. For a release, replace the
builder reference via `--build-arg RUST_IMAGE=rust:<audited-version>-bookworm@sha256:<digest>`
and pin the runtime base in your release process; record both digests, target
architecture and resulting image digest. The angle-bracket values are placeholders,
not runnable image references. CI intentionally follows stable to detect drift.

Use one persistent volume per worker identity. Docker-created volumes inherit the
image directory ownership; bind mounts must be provisioned on the host for UID/GID
10001, mode 0700. Do not share a volume concurrently, bake identities into images,
use a read-only identity mount, or deploy with an ephemeral reporting directory.
An example for an **already enrolled** volume (replace target before running):

```sh
docker volume create ai48proxy-worker-a
docker run --name ai48proxy-worker-a --read-only --cap-drop ALL \
  --security-opt no-new-privileges --tmpfs /tmp:rw,noexec,nosuid,size=16m \
  --mount type=volume,src=ai48proxy-worker-a,dst=/var/lib/ai48proxy \
  --publish 127.0.0.1:8317:8317 --stop-timeout 60 \
  ai48proxy:acceptance --identity-dir /var/lib/ai48proxy/identity \
  --home-host HOME_PRIVATE_DNS --home-port 8327
```

A new empty volume must first be enrolled using section 3, not the existing-identity
command. Do not run host and container workers on the same identity at once.
Home must supply a container-reachable bind host (`0.0.0.0`) and the published port;
`EXPOSE 8317` does not override Home. Configure egress for Home and upstreams.
Keep the plaintext HTTP worker listener private or behind a TLS ingress. Readiness
is `GET /healthz`; it is not a process-only liveness check. Set the supervisor stop
grace above the configured cancellation/drain bound and measure it in section 6;
60 seconds is an example, not a guarantee. Start with no aggressive restart loop.

For the installer-managed stack, service names are `ai48proxy-1`, `ai48proxy-2`
and `ai48proxy-3`; nginx fronts the workers. Use, for example:

```sh
docker compose ps ai48proxy-1 ai48proxy-2 ai48proxy-3
docker compose logs --tail 100 ai48proxy-1
```

Review logs privately before sharing. A healthy nginx endpoint does not prove all
three workers are ready: inspect each worker and its separate persistent volume.
See [Docker deployment](docker.md) for installer topology and provisioning.

## 3. Protected enrollment and mTLS gate (operator-only)

Use an isolated, disposable Home cluster/database with Management API enabled,
reachable RESP protocol-mux listener, valid lifecycle configuration and a dedicated
low-budget downstream test account. Obtain the exact deployed Home contract from
its `/v8/management/capabilities` and current handlers/docs. Do not use deprecated
`/v0/management`. Configure one supported provider/model at a time, concurrency
limits and a budget ceiling. A controlled HTTP upstream is preferable for fault
injection; real providers can incur charges and receive the synthetic prompt.

1. Through authenticated, verified HTTPS Management API access, create a pending
   certificate with `POST /v8/management/certificates/clients`. Use the deployed
   Home node-name request schema; its response includes `id`, `home_jwt`,
   `node_name`. Keep this response private. Never use the management secret as a
   downstream API key, log the JWT, or store secrets in CI artifacts.
2. **Protect the complete initial enrollment path.** This worker sends
   `CERTIFICATE REQUEST` and the one-time secret in plaintext RESP to the JWT's
   `ip`/`port`. A private encrypted network or transparent tunnel must protect that
   exact destination from the worker's namespace. A tunnel listening somewhere
   else does not change JWT routing. `--home-host` cannot override a JWT target.
   Do not edit JWT claims to make a tunnel work. The HTTPS-only non-cluster listener
   is not an enrollment substitute.
3. Prefer a secret-manager-provisioned private file (0600) through `HOME_JWT_FILE`
   or `--home-jwt-file`. `HOME_JWT` is also supported, but file/environment token
   inputs conflict. Never use a literal token in commands, shell history,
   command-line `--home-jwt`, checked-in env files or Docker build arguments.
   The token is a trusted provisioning capability: its signature is **not locally
   verified**. Returned CA fingerprint checking does not encrypt enrollment or
   prevent an interception/consumption race. Never use an unprotected public path.
4. With `HOME_JWT_FILE` or `HOME_JWT` injected and the protected route active, run:

   ```sh
   umask 077
   ./target/release/ai48proxy --identity-dir /PRIVATE/worker-a
   ```

   `/PRIVATE/worker-a` is an operator-selected persistent directory, not a repo path.
   For Docker, mount the private token file separately, make it readable only by
   the worker owner, and inject its path through `HOME_JWT_FILE`. Enroll into the
   persistent identity volume, not the token mount. Environment token values may
   be visible to privileged inspect users. Neither method makes an unprotected
   enrollment route safe.
5. Verify membership/config acceptance and `GET /healthz` returning 200. The HTTP
   listener only starts after the first valid Home config; initial failure may be
   connection refusal rather than 503. Inspect directory/file modes (0700/0600),
   owner, storage durability and absence of secrets in logs without printing files.
6. Stop cleanly; remove `HOME_JWT`, `HOME_JWT_FILE` and token-file CLI arguments
   from the restart configuration. Restart using the
   same identity and explicit `--home-host`/`--home-port`. Subsequent commands use
   verified mTLS with the pinned CA; no Redis password AUTH or HTTP ALPN is used.
7. In a separate disposable identity, verify wrong CA, revoked/unregistered client
   and missing certificate cannot reach ready state. Never corrupt the accepted
   identity just to test failure. If enrollment's response is lost, preserve the
   directory and `enrollment-sent` evidence, reconcile/revoke the pending identity
   in Home and provision a new one only after operator review. Do not delete a
   marker and automatically resend a possibly consumed capability.

Fail this gate if the protected route, certificate validation, single ownership,
or secret handling cannot be demonstrated.

## 4. Endpoint gate and opt-in HTTP harness

Prerequisites: curl, jq, an already-ready worker and a private header file (0600)
containing `Authorization: Bearer <dedicated-downstream-key>`, provisioned by the
operator. This is not an actual credential example. Do not enable `set -x`, curl
verbose output, redirects with auth, or TLS verification bypasses.

```sh
sh scripts/probe-worker.sh http://127.0.0.1:8317 /PRIVATE/downstream.headers TEST_CHAT_ALIAS
```

Replace the alias with a Home-authorized Chat model. The script performs catalog,
JSON and SSE assertions; **it makes two potentially billable requests**. It uses
private temporary responses and removes them on exit, withholding error bodies.
HTTPS uses normal CA verification; loopback HTTP is for local/tunneled access.
Its 120-second limit is a test-client deadline, not an upstream runtime policy.
A failed assertion stops the script; lack of a final success line is failure.
This smoke test does not automate enrollment, destructive faults or accounting.

Run this additional matrix manually using provider-appropriate small request
bodies. Record status, sanitized shape and terminal stream semantics, not secrets
or full upstream bodies. Home controls provider/model selection: route existence
alone is not support for every format/credential combination.

| Endpoint | Required assertions |
| --- | --- |
| `GET /healthz` | 200/ready only after membership; 503 during established-session recovery |
| `GET /v1/models` | Home-scoped OpenAI list; missing/invalid key rejected, forbidden model absent |
| `POST /v1/chat/completions` | JSON/SSE for each required Home-selected OpenAI, Claude, Codex or supported Gemini/Vertex/Antigravity bridge; text, tools, usage and terminal semantics |
| `POST /v1/completions` | Native OpenAI-compatible API-key JSON/SSE only |
| `POST /v1/responses` | Required native/Chat-bridge/Codex HTTP paths, JSON/SSE and terminal collection |
| `GET /v1/responses` | Supported WebSocket sessions: retained lease, per-turn usage, disconnect cancellation; unsupported overlap/model changes reject |
| `POST /v1/responses/compact` | Supported native compact path only; no inferred session/continuation equivalence |
| `POST /v1/messages` | Claude native API-key/OAuth and Messages-to-Chat JSON/SSE; text/tools/usage |
| `POST /v1beta/models/{model}:generateContent` | Native Gemini API-key JSON |
| `POST /v1beta/models/{model}:streamGenerateContent` | Native Gemini API-key SSE |
| `POST /v1/messages/count_tokens` | Native Claude API-key estimate; no fabricated inference usage |
| Image/audio routes | Test only the selected provider's implemented path and configuration gates; verify usage and cancellation separately |
| Video routes | Explicit unsupported response; registration is not execution support |

For Vertex/Antigravity, also verify Home-prepared credentials, required project/
location metadata, envelope handling and the exact supported tool/schema subset.
For image/audio, inspect `src/gateway/media.rs` before selecting the test matrix;
route presence does not establish general multimedia or realtime support.

Also test unauthorized access, disallowed alias, exhausted concurrency (429),
unsupported provider/format, malformed body and controlled upstream failure.
Reject silent executor fallback, leaked credential/error bodies, or a 200 stream
that terminates without its expected terminal event. Cancel a long stream by
closing the test client and confirm cancellation/release below. Do not require
request-log records as proof of usage: those are separate Home contracts.

## 5. Usage, cumulative release and observation gate

Capture baseline and post-test Home views using authenticated Management API
access. Current source registers `GET /v8/management/topology`,
`/credentials/concurrency`, `/credentials/in-flight`,
`/credentials/in-flight/summary`, `/usage/records`, `/usage/overview` and
`/usage/aggregates` under the same base. Use deployed query/response schemas and
bounded test time windows, not guessed filters. Home may paginate or aggregate;
a dashboard total alone is insufficient evidence. Keep raw records private.

For each successful, failed, translated and cancelled request:

- Correlate the Home usage record by execution ID, alias/execution model,
  credential/principal and time. Confirm status, latency/TTFT and v2 token buckets
  against a deterministic upstream fixture. Missing/partial token observations
  must remain explicit, not fabricated zeros or overlapping reasoning counts.
- While a controlled stream remains open longer than a snapshot interval, observe
  it in Home's in-flight view. Verify ordered snapshot revisions, consistent
  multipart metadata/barrier and accounted/unaccounted totals within configured
  limits. Use controlled high cardinality to test overflow; do not overload a
  real provider. Stale/overflow observations must not look complete.
- After completion/cancel, concurrency returns to its isolated baseline and the
  execution disappears from fresh snapshots. Usage or snapshot ingestion alone
  must not release a slot. Observe `LPUSH concurrency-release` acknowledgements
  via sanitized instrumentation: sequences are cumulative per counter/lifetime,
  duplicates do not decrement twice, and redispatch waits for the prior release
  ACK. Never packet-capture unredacted credentials or decrypt production traffic.
- Inject a lost usage/release ACK in the isolated Home fixture or approved fault
  proxy. Expect identical usage execution IDs on retry and idempotent Home
  persistence; lost release ACKs retry the owning lifetime's cumulative sequence.
  Home counters and billing must not duplicate or go negative. If telemetry is
  insufficient to prove this, mark the gate **blocked**, not passed.
- Interrupt Home after usage is durably accepted into the outbox; terminate and
  restart the worker on the same persistent storage. Verify replay, Home dedupe
  and persistent local ACK deletion. No accepted record may silently disappear.
  Snapshot/release state is not a durable outbox and must not be assumed to survive
  process restart. Reconcile the old membership at Home before normal admission.
- Separately test capacity/storage failure on disposable storage: admission must
  fence rather than evict queued usage. Preserve corrupt records for diagnosis;
  never delete the reporting directory to manufacture readiness.

Durability covers only events accepted by the outbox. It cannot reconstruct
unobserved upstream usage at crash time or events rejected for persistence/capacity
errors. This is not a claim of end-to-end exactly-once billing.

## 6. Fencing, recovery, configuration and shutdown gate

Perform faults only in an isolated deployment, with UTC event timestamps and
bounded operator deadlines derived from Home's heartbeat/cancel/snapshot settings.

1. Hold a controlled stream open; sever the config subscription or block Home
   traffic. Within the configured heartbeat/cancel bounds, new admissions must
   stop, active upstream streams must cancel, and the existing listener must
   return 503. Arbitrary subscription traffic must not renew heartbeat health.
2. Restore Home. Observe automatic backoff with accepted registration and fresh
   lifetime/connection/registry ownership, then readiness and a new request.
   Ambiguous dispatch must fence rather than blindly redispatch. Old release ACKs
   must not clear counters in the new registry. Verify conditional takeover only
   after old execution/release owners drain. An undrained registry must suppress
   takeover; `membership_takeover_unavailable` must fall back to normal admission.
   Test legacy subscription negotiation separately if required by the deployment.
   Unresolved old membership may delay recovery. Do not shorten safety policy to
   force green.
3. With a multi-Home test cluster, lose the active target and verify discovered
   alternate-target admission. Test full process restart separately: discovered
   targets are not persisted, so explicit startup targets must still be reachable.
   Exercise a planned rebalance separately: busy/pending/release-unacknowledged
   workers must not migrate live counters. A drained worker must claim the exact
   plan/term/target, preflight it, register with a fresh registry and acknowledge
   the result before restoring readiness. Inject stale/rejected claims, failed
   target preflight, destination registration failure and lost result ACKs. Verify
   failure reporting/fallback or fencing, not silent success. Do not count typed
   latency helper coverage as proof of latency-driven balancing.
4. Apply a supported Home update and verify new-request behavior and immutable
   in-flight configuration. Save a private downstream snapshot and validate with
   `./target/release/ai48proxy --check-config /PRIVATE/home-snapshot.yaml`.
   Unsupported active settings must reject startup/update explicitly; check-config
   validates syntax/support, not connectivity or provider correctness. Listener
   host/port updates do not rebind the running listener; plan a drained restart.
5. Send SIGTERM during an active request with healthy Home: admission stops,
   optional graceful draining follows `shutdown-timeout`, then lifecycle
   cancellation/cleanup uses its separate bound. Record usage/release settlement
   and exit status. Repeat with Home unavailable and verify durable accepted usage
   remains replayable. Incomplete drain can be logged while shutdown succeeds;
   retirement/other errors can fail shutdown. A zero exit alone is not evidence
   that all accounting and releases settled.
   External restart is not safe membership takeover; never overlap identities.
6. Backup/restore the entire stopped identity directory, including reporting, to
   encrypted private storage. Restore only with the original owner stopped and
   after Home reconciliation. A stale backup can replay ACKed events and cannot
   restore missing post-backup usage; test Home dedupe. Do not rotate only key
   files or clone one identity across replicas. Roll back to the previously
   accepted deployment using a separate identity after draining/reconciling the
   worker. Another implementation cannot be assumed to consume this worker's
   private usage outbox. Retain the worker volume until reconciliation.

## 7. Workload qualification and limits

| Provider/feature | Qualification boundary; deployment acceptance still required |
| --- | --- |
| OpenAI-compatible API key | Supported Chat, Completions and Responses paths/bridges, JSON/SSE; not every provider extension |
| Claude API key / OAuth | Native Messages, Chat ↔ Messages translation, focused OAuth wire/signing and Home-only refresh; advanced tools, continuity and thinking replay remain limited |
| Gemini API key | Native generateContent/streamGenerateContent and focused Chat text/function-tool conversion; no general cross-format guarantee |
| Codex OAuth | HTTP Responses and constrained WebSocket sessions with per-turn accounting; full SIWC, continuation, overlap and steering semantics remain incomplete |
| Vertex | Native Gemini/OpenAPI Chat with Home-prepared token/API-key credentials as appropriate; no local service-account exchange or complete specialized API coverage |
| Antigravity | Native envelope execution and focused Chat/tool bridge; signed reasoning replay/cache fidelity, complex schemas and advanced tools remain limited |
| Image/audio | Selected source paths and configuration gates only; not general multimedia/realtime support; video routes remain unsupported |
| Other execution/auth | Gemini CLI and plugin execution unavailable; no general local login/refresh fallback |
| Membership | Conditional drained takeover and drain-only planned rebalance; no live registry/counter transfer or persisted discovery targets |
| Configuration/operations | Supported keepalive/retry/body/drain controls; known unsupported active settings reject; fresh mTLS command connections prioritize ownership isolation over throughput |
| Accounting | Durable accepted usage and supported v2 fields; crash gaps and complete provider billing equivalence remain unproven |

Do not promote a worker for a workload requiring an unsupported capability or an
unpassed gate. Test the exact credential/provider/format/tool/session combination;
a successful basic request does not qualify advanced OAuth, caching, billing,
retry or media behavior. Consult current route/provider source and the dated
[implementation checkpoint](porting-status.md), rather than treating historical
feature inventories as complete.

When needed, CLIProxyAPI can serve as an external compatibility reference using a
**separate** identity, equivalent Home policy and deterministic fixtures. Record
both revisions and sanitized behavioral differences. Such comparisons do not
establish universal feature equivalence or replace the Home accounting gates.

## Acceptance record

For each gate record `PASS`, `FAIL`, `BLOCKED`, or `NOT RUN`, UTC time, versions,
configuration revision, expected/observed result, sanitized evidence location and
operator. Keep credentials, identities, raw snapshots and private payloads out of
Git/CI artifacts. Local CI green, image build green and HTTP smoke green are
separate results. Promote only the explicitly tested provider subset after all
applicable deployment gates and rollback rehearsal pass. Unrun live gates remain
**NOT RUN**; this document itself reports no live success.

# Worker acceptance and deployment runbook

## Status and scope

This is a reproducible acceptance procedure, **not evidence of live success**.
No live Home enrollment, real-provider request, failover, billing reconciliation,
or container deployment is asserted here. Local mock tests cannot establish those
properties. Keep CPA serving production until the applicable gates below pass.

Run commands from the AI48Proxy root. No README, application source, Home or CPA
changes are required. See [home-compatibility.md](home-compatibility.md) for the
source contract. Record the AI48Proxy, Home and optional CPA comparison revisions,
`rustc --version`, `cargo --version`, OS/architecture, image digest, configuration
revision, enabled providers and UTC test window in a private acceptance record.
There is no automatic CPA source-version synchronization or compatibility promise.
A new Home revision or provider feature requires rerunning the relevant gates.

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
supervisor recovery. Inspect actual test output; a green build is not deployment
acceptance. An unrelated source failure is a failed gate, not permission to skip it.

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
3. Supply `HOME_JWT` through an operator secret manager, not a literal command,
   shell history, command-line `--home-jwt`, checked-in env file or Docker build arg.
   It is a trusted provisioning capability: its signature is **not locally
   verified**. Returned CA fingerprint checking does not encrypt enrollment or
   prevent an interception/consumption race. Never use an unprotected public path.
4. With the secret injected and the protected route active, run:

   ```sh
   umask 077
   ./target/release/ai48proxy --identity-dir /PRIVATE/worker-a
   ```

   `/PRIVATE/worker-a` is an operator-selected persistent directory, not a repo path.
   For Docker, inject the variable through your runtime secret provisioning and
   enroll into the mounted volume; Docker environment values may be visible to
   privileged inspect users. No native secret-file CLI is claimed.
5. Verify membership/config acceptance and `GET /healthz` returning 200. The HTTP
   listener only starts after the first valid Home config; initial failure may be
   connection refusal rather than 503. Inspect directory/file modes (0700/0600),
   owner, storage durability and absence of secrets in logs without printing files.
6. Stop cleanly; remove `HOME_JWT` from the restart environment. Restart using the
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
| `POST /v1/chat/completions` | JSON and SSE for native OpenAI and Chat-to-Claude; text, tools, usage, terminal marker |
| `POST /v1/completions` | Native OpenAI-compatible API-key JSON/SSE only |
| `POST /v1/responses` | OpenAI HTTP and Codex OAuth HTTP JSON/SSE; Codex terminal collection for JSON |
| `POST /v1/messages` | Claude native API-key/OAuth and Messages-to-Chat JSON/SSE; text/tools/usage |
| `POST /v1beta/models/{model}:generateContent` | Native Gemini API-key JSON |
| `POST /v1beta/models/{model}:streamGenerateContent` | Native Gemini API-key SSE |

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
2. Restore Home. Observe automatic backoff with normal registration and fresh
   lifetime/connection/registry ownership, then readiness and a new request.
   Ambiguous dispatch must fence rather than blindly redispatch. Old release ACKs
   must not clear counters in the new registry. No takeover is attempted; unresolved
   old membership may delay recovery. Do not shorten safety policy to force green.
3. With a multi-Home test cluster, lose the active target and verify discovered
   alternate-target admission. Test full process restart separately: discovered
   targets are not persisted, so explicit startup targets must still be reachable.
4. Apply a supported Home update and verify new-request behavior and immutable
   in-flight configuration. Save a private downstream snapshot and validate with
   `./target/release/ai48proxy --check-config /PRIVATE/home-snapshot.yaml`.
   Unsupported active settings must reject startup/update explicitly; check-config
   validates syntax/support, not connectivity or provider correctness. Listener
   host/port updates do not rebind the running listener; plan a drained restart.
5. Send SIGTERM during an active request with healthy Home: admission stops,
   requests cancel/drain within bounds, usage/release settle and exit status is
   recorded. Repeat with Home unavailable; unresolved admission/usage/release may
   produce nonzero exit. Confirm durable accepted usage remains and can replay.
   External restart is not safe membership takeover; never overlap identities.
6. Backup/restore the entire stopped identity directory, including reporting, to
   encrypted private storage. Restore only with the original owner stopped and
   after Home reconciliation. A stale backup can replay ACKed events and cannot
   restore missing post-backup usage; test Home dedupe. Do not rotate only key
   files or clone one identity across replicas. Roll back to CPA using a separate
   CPA identity after draining/reconciling the worker; CPA cannot consume this
   worker's private usage outbox. Retain the worker volume until reconciliation.

## 7. Supported subset and parity blockers

| Provider/feature | Current implementation boundary; live acceptance still required |
| --- | --- |
| OpenAI-compatible API key | Same-format Chat, Completions, Responses JSON/SSE; not every provider extension |
| Claude API key / OAuth | Native Messages plus Chat ↔ Messages text/tool/usage translation; OAuth fingerprint/identity and Home-only refresh after 401 are subsets |
| Gemini API key | Native generateContent/streamGenerateContent, not general cross-format translation |
| Codex OAuth | HTTP Responses, bearer/account headers and terminal SSE collection; no WebSocket/continuation parity |
| Other execution/auth | Gemini CLI, Antigravity, Vertex-specific auth, plugins, multimedia/realtime and full cross-format translation unavailable |
| Membership | Normal-registration recovery only; takeover and planned rebalance migration unavailable |
| Configuration/operations | Unsupported active settings reject; advanced provider knobs/keepalive controls incomplete, fresh command connections prioritize isolation over throughput |
| Accounting | Durable accepted usage and supported v2 fields; full provider billing parity and crash-gap recovery unproven |

Do not promote a worker for a workload requiring a blocked row. Native endpoint
support does not establish complete OAuth, tool schema, caching, billing, retry,
image/audio or session behavior parity with CPA. Compare CPA on a **separate**
identity with equivalent Home policy and deterministic fixtures when necessary;
record both revisions and sanitized differences without adding a source-version
mechanism or modifying CPA.

## Acceptance record

For each gate record `PASS`, `FAIL`, `BLOCKED`, or `NOT RUN`, UTC time, versions,
configuration revision, expected/observed result, sanitized evidence location and
operator. Keep credentials, identities, raw snapshots and private payloads out of
Git/CI artifacts. Local CI green, image build green and HTTP smoke green are
separate results. Promote only the explicitly tested provider subset after all
applicable deployment gates and rollback rehearsal pass. Unrun live gates remain
**NOT RUN**; this document itself reports no live success.

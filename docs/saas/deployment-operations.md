# Deployment and operations

> **Status:** target design · **Last updated:** 2026-10-16

## Deployment artifacts

The SaaS extension will provide:

- One versioned `figcraft-cloud` image with role selection.
- One static `figcraft-web` image or archive.
- A Docker Compose stack for local development and single-node operation.
- A Caddy configuration for TLS and route separation.
- PostgreSQL migrations embedded in or released with the cloud image.
- S3-compatible bucket initialization and lifecycle policy tooling.
- Optional observability and local-AI Compose profiles.
- Kubernetes manifests or a Helm chart after role-separated scaling is proven.

## Compose topology

### Core profile

The `deploy/saas/compose.yaml` scaffold has digest-pinned PostgreSQL, Redis and SeaweedFS images
and a reviewed PostgreSQL volume target (see the [image review](dependency-images.md)). It defines
named persistence volumes, health checks and readiness-gated startup. A one-shot migration job
applies the S2-004 and S2-005 schemas before cloud startup. The gateway then serves diagnostic
transport routes; product APIs and durable collaboration remain unfinished. Do not treat this as
an operational SaaS deployment.

| Service | Image or role | Persistence | Exposure |
|---|---|---|---|
| `gateway` | Caddy | certificates and configuration | 80 and 443 |
| `web` | static FigCraft WASM | none | internal HTTP |
| `cloud` | `figcraft-cloud`, role `all` | none local | internal HTTP and WebSocket |
| `role-provision`, `role-grants` | `figcraft-cloud --provision-db-roles` | PostgreSQL roles and grants | one-shot, private network |
| `migrate` | `figcraft-cloud --migrate` | PostgreSQL schema | one-shot, private network |
| `postgres` | supported PostgreSQL release | database volume | private network only |
| `redis` | supported Redis release | optional append-only volume | private network only |
| `seaweedfs` | pinned SeaweedFS `mini` | object volume | private network only |
| `seaweedfs-init` | cloud image, `--init-object-store` | none | one-shot, idempotent bucket, versioning and lifecycle initialization (S3-014) |

### Scale profile

Replace `cloud` with independently scalable services:

| Service | Role | Scaling |
|---|---|---|
| `cloud-api` | `api` | stateless replicas |
| `cloud-relay` | `relay` | replicas owning independent document rooms |
| `cloud-worker-interactive` | `worker` | thumbnail and webhook queues |
| `cloud-worker-heavy` | `worker` | export, snapshot, AI and WASM queues |

### Optional profiles

| Profile | Services |
|---|---|
| `observability` | OpenTelemetry collector, Prometheus-compatible metrics and log backend |
| `local-ai` | AI gateway worker and an operator-selected local model runtime |
| `mail` | Development mail sink or configured transactional-mail relay |

## Configuration

Configuration is loaded from environment variables or mounted secret files. Unknown settings fail
startup. Secrets must support a `_FILE` variant so Docker and orchestrator secrets are not forced
into environment inspection. `FIGCRAFT_ENV=production` rejects loopback, HTTP and reserved `.invalid`
public/identity endpoints and requires core credentials through mounted secret files; development
defaults are not production credentials.

Core settings:

```text
FIGCRAFT_ENV=development|production
FIGCRAFT_DEPLOYMENT=single|multi|embedded
FIGCRAFT_ROLE=all|api|relay|worker
FIGCRAFT_PUBLIC_URL=https://design.example.com
FIGCRAFT_SINGLE_TENANT_ID=00000000-0000-0000-0000-000000000001
# Several relay replicas only: where the other replicas reach THIS replica (ws:// or wss://, unique
# per replica). Turns on room leases: one replica serves each room, the others pass clients on to it.
# Leave unset for a single relay.
FIGCRAFT_RELAY_ADVERTISE_URL=ws://relay-1.internal:8082/ws/v1/collab
# Seconds a stopping process stays up, not ready, before it stops (0 to 120, default 5).
FIGCRAFT_DRAIN_SECONDS=5
# Seconds an emptied room stays open (leased, in memory) before it is saved and closed, so a client
# that reconnects finds it warm (0 to 3600, default 30; 0 closes at once).
FIGCRAFT_ROOM_IDLE_SECONDS=30
# A room with changes is snapshotted every N seconds (10 to 86400, default 300) or after N accepted
# batches (1 to 1000000, default 1000), whichever comes first: a crash replays at most that much.
FIGCRAFT_SNAPSHOT_INTERVAL_SECONDS=300
FIGCRAFT_SNAPSHOT_AFTER_EVENTS=1000
# Relay quotas, per replica (every one is bounded; these are the defaults):
FIGCRAFT_RELAY_MAX_ROOMS=1024              # documents open at once (1 to 100000)
FIGCRAFT_RELAY_MAX_CONNECTIONS=1024        # connections at once (1 to 100000)
FIGCRAFT_RELAY_MAX_PEERS_PER_ROOM=256      # clients in one room (1 to 10000)
FIGCRAFT_RELAY_MESSAGES_PER_SECOND=100     # per connection, burst 4x; past it the connection is dropped (1 to 10000)
FIGCRAFT_RELAY_MAX_MESSAGE_BYTES=67108864  # largest message a client may send (1024 to 67108864)
FIGCRAFT_RELAY_MAX_QUEUED_BYTES=167772160  # outbound bytes queued per connection; a reader this far behind is dropped

# Built-in accounts (S6-033): Argon2id cost for new password hashes (stored inside every hash, so
# raising them upgrades old hashes at the next sign-in) and how many hashes run at once. Memory in use
# is at most CONCURRENCY x MEMORY_KIB. Defaults are shown; see password-hashing-decision.md.
FIGCRAFT_PASSWORD_MEMORY_KIB=47104         # 8192 to 1048576
FIGCRAFT_PASSWORD_PASSES=2                 # 1 to 10
FIGCRAFT_PASSWORD_LANES=1                  # 1 to 8
FIGCRAFT_PASSWORD_CONCURRENCY=4            # 1 to 64
# Password sign-in lockout (S6-035): after THRESHOLD failures on an existing unlocked account it is
# locked for BASE seconds, doubling per further failure up to MAX (attempts while locked are not
# counted, so hammering cannot extend a lock). The same numbers drive the per-replica backoff that
# answers 429 for any login, known or not. BASE must not exceed MAX.
FIGCRAFT_LOGIN_LOCKOUT_THRESHOLD=5         # 1 to 100
FIGCRAFT_LOGIN_LOCKOUT_BASE_SECONDS=30     # 1 to 86400
FIGCRAFT_LOGIN_LOCKOUT_MAX_SECONDS=900     # 1 to 86400

DATABASE_URL_FILE=/run/secrets/database_url
REDIS_URL_FILE=/run/secrets/redis_url
S3_ENDPOINT=http://seaweedfs:8333
S3_REGION=us-east-1
S3_BUCKET=figcraft
S3_ACCESS_KEY_FILE=/run/secrets/s3_access_key
S3_SECRET_KEY_FILE=/run/secrets/s3_secret_key
S3_FORCE_PATH_STYLE=true

OIDC_ISSUER=https://identity.example.com
OIDC_CLIENT_ID=figcraft
OIDC_CLIENT_SECRET_FILE=/run/secrets/oidc_client_secret
OIDC_AUDIENCE=figcraft

SESSION_SIGNING_KEY_FILE=/run/secrets/session_signing_key
EMBED_ISSUER_CONFIG_FILE=/etc/figcraft/embed-issuers.toml

RUST_LOG=info
OTEL_EXPORTER_OTLP_ENDPOINT=http://otel-collector:4317
```

Logging: the long-running roles write one line per record to stderr (`<UTC time> <LEVEL> <module>:
<message>`), filtered by `RUST_LOG`: a default level and/or `module=level` directives, for example
`RUST_LOG=info,figcraft_cloud_relay=debug,sqlx=warn` (default `info`; levels `off`, `error`, `warn`,
`info`, `debug`, `trace`). The relay's warnings and errors (a lost room lease, a failed recovery or
commit, a refused open) are only visible through it. Secrets and tokens are never logged.

Draining and rolling releases: on SIGTERM or SIGINT (what `docker stop` and orchestrators send) the
process stops being ready (`/readyz` answers 503 `draining`; `/healthz` stays 200 so it is not
killed), the relay closes every new connection at once with the restarting close code (clients
retry, and the gateway sends them to a ready replica), and sessions already open carry on for
`FIGCRAFT_DRAIN_SECONDS` so the gateway's health check can notice. Then the relay stops: it commits
what is open, stores snapshots, releases its room leases and closes the remaining connections with
the restarting code, so those clients reconnect (to another replica, which takes the rooms over
at once instead of waiting for the leases to expire) and resume from their last confirmed
sequence; the process exits 0. Set the gateway's health-check interval below the drain period
and the orchestrator's stop timeout above it plus the time to commit and snapshot the open rooms
(Compose: `stop_grace_period: 30s`). A process killed without a SIGTERM skips all of this; its
rooms are then unavailable until their leases expire (15 s).

Room leases (several relay replicas): with `FIGCRAFT_RELAY_ADVERTISE_URL` set, a replica takes a
PostgreSQL lease on a document's room before restoring it and renews it every 5 s (lifetime 15 s).
A client that reaches a replica which does not own the room is passed on: that replica connects to
the owner's advertised address, sends the client's hello there and relays the session both ways, so
no gateway rule or client change is needed. Every replica must therefore be able to open WebSocket
connections to every other replica's `FIGCRAFT_RELAY_ADVERTISE_URL` (an internal address is fine;
`wss://` works). If the owner can't be reached the client is told "this document is open on another
server: try again in a moment". A replica that crashes leaves its rooms unavailable for at most the
lifetime (15 s); its late writes are rejected by the fencing token regardless. A room closes and releases its lease when its last client leaves. Without the
setting a single relay serves rooms without leases.

Single-tenant mode still stores the configured tenant identifier on every row. It may provide an
initial bootstrap administrator, but bootstrap credentials expire or are removed after setup.

### Platform operators (S6-044, S6-045)

The platform realm (`/platform/v1`) is for the people who run the deployment, separate from every
workspace. It needs `DATABASE_URL`, `SESSION_SIGNING_KEY` (the TOTP secrets are sealed with a
subkey of it) and `FIGCRAFT_PUBLIC_URL` (the same-origin check for state-changing requests).

1. Create the first operator: `figcraft-cloud --print-platform-bootstrap-token` prints a one-time
   token (valid one hour; running it again replaces it; refused once a platform admin exists).
2. Spend it: `POST /platform/v1/bootstrap` with `token`, `login`, `display_name` and `password`
   (the same password policy as workspace accounts, Argon2id).
3. Sign in at `POST /platform/v1/session`, enrol the authenticator app
   (`POST /platform/v1/session/totp/enrol`, then verify a code at `/session/totp`). A session is
   accepted by the API only after the second factor. Sessions last 15 minutes idle and 4 hours
   at most; five failed attempts lock an admin for 15 minutes; sign-in is rate limited per client.
4. Create further operators with `POST /platform/v1/admins`. The last active admin cannot be
   disabled. Every action is appended to `platform_audit_events`.

The platform cookie is `__Secure-figcraft-platform` with `Path=/platform/v1`, so it is never sent
to the workspace API. The contract is `apps/figcraft-cloud/openapi/platform-api-v1.json`, served at
`/platform/v1/openapi.json`.

## Container requirements

- Run as a numeric, non-root user.
- Use a read-only root filesystem.
- Mount a writable temporary directory with a size limit only where needed.
- Drop Linux capabilities.
- Set memory and CPU limits.
- Include a minimal init only when child-process handling requires it.
- Produce multi-architecture images for amd64 and arm64.
- Record source revision, version and build time in OCI labels.
- Generate a software bill of materials and signed image provenance.
- Scan the final image, not only build dependencies.

The API, relay and worker images share the same binary but may use different network policies and
resource limits.

The current cloud scaffold exposes `/healthz`, `/readyz` and `/startupz` on port 8081, plus
diagnostic `/api/v1/healthz` and `/ws/v1/health` transport routes. Liveness and startup indicate
that the process health server is running; readiness becomes HTTP 200 when its listener is bound
and the SQLx migration ledger has the supported schema version, and (since S3-001) one signed
`HeadBucket` request at startup proved the configured S3-compatible endpoint is reachable, accepts
the mounted credentials and has the bucket. It does not yet report
authorization or document-operation readiness. Its shell-free `--check-ready` command probes the
local readiness endpoint for the distroless container health check. Do not treat transport
readiness as a healthy SaaS product deployment.

Health responses carry a generated `X-Request-Id`; each request emits a structured JSON stdout
record. Valid W3C version-00 `traceparent` context is forwarded and included in logs, while malformed
context is discarded. An OTLP exporter is not configured yet.

## Routing

The edge routes:

```text
/                         static web application
/api/v1/*                 cloud API
/ws/v1/*                  collaboration WebSockets
/healthz                  shallow edge health
```

SeaweedFS is not published outside the private Compose network. Presigned object requests may use
a separate object hostname or an authenticated gateway path in a later release. Its master, filer,
volume and admin endpoints require further network isolation before production.

WebSocket proxy settings must preserve upgrades, client address metadata from trusted proxies,
idle timeouts longer than heartbeat intervals and bounded request headers.

## Startup sequence

1. PostgreSQL becomes reachable.
2. Run `figcraft-cloud --provision-db-roles` with the administrator URL and separate migrator and
   runtime passwords mounted only in this one-shot job. It creates the two limited logins and
   transfers ownership of existing application tables to the migrator.
3. Run `figcraft-cloud --migrate` with the migrator URL. It applies embedded, versioned SQL under a
   PostgreSQL advisory lock; application roles never migrate on startup.
4. Run role provisioning again to grant runtime access to newly created tables.
5. `figcraft-cloud --init-object-store` (the `seaweedfs-init` job) creates the bucket if absent,
   enables and verifies versioning, and applies FigCraft's lifecycle rules: expire objects tagged
   `figcraft-staging=true` (every presigned staging upload signs that tag), abort incomplete
   multipart uploads and expire noncurrent versions. It is idempotent, refuses to overwrite
   lifecycle rules FigCraft did not write, and never runs at normal startup. The application's
   reservation-expiry sweep stays authoritative; the lifecycle rule is a backstop, and whether a
   given S3-compatible store enforces it must be checked per deployment.
6. Redis becomes reachable.
7. API and relay roles start with only the runtime URL and pass readiness checks.
8. Workers start and claim jobs.
9. Gateway exposes the deployment.

Application replicas do not race to run schema migrations during normal startup. A release fails
closed when its database schema is outside the binary's supported range.

## Database migrations

The cloud image's `--migrate` command uses SQLx's embedded migration set and holds PostgreSQL's
advisory migration lock while applying pending migrations. It reads the database connection from
the configured `DATABASE_URL` secret and reports fixed diagnostics without echoing credentials or
driver errors. Migration application is deliberately separate from readiness and normal role
startup. At process startup, readiness checks the highest successfully applied migration version
against the binary's supported inclusive range. A missing ledger or unsupported version keeps
`/readyz` at 503 while liveness remains available. The newest migration (the platform realm) has version
`20261018000032`, which is the binary's supported version. Schema rollback policy is handled by
the release migration procedure.

The database administrator URL is available only to the role-provisioning jobs. The migration job
gets a separate migrator URL; cloud roles get only the runtime URL. The provisioning command binds
passwords as query parameters, checks administrator authority and emits fixed diagnostics. It
revokes public database/schema/table access, gives the migrator schema creation and ownership of
known application tables, and grants runtime only DML on those tables. Runtime can read the SQLx
ledger for readiness but cannot write it; document events and audit events allow insert and read,
not update or delete. Run provisioning after every migration release so newly added tables receive
reviewed grants. Roles and credentials are scoped to the dedicated FigCraft database; production
operators must rotate the administrator credential independently.

- Every migration has a unique ordered version and checksum.
- Migrations are forward-only in production.
- Expand-and-contract changes permit old and new replicas during rolling deployment.
- Destructive cleanup occurs in a later release after all readers stop using the old shape.
- Large backfills run as resumable jobs with progress, not one unbounded transaction.
- Migration tests apply from an empty database and every supported previous release.
- A database backup and restore point precede high-risk migrations.

## Health endpoints

| Endpoint | Meaning |
|---|---|
| `/health/live` | Process event loop is responsive; no dependency calls |
| `/health/ready` | Role can accept work and required dependencies meet policy |
| `/health/startup` | Initialization and schema compatibility completed |
| `/metrics` | Private metrics endpoint or collector integration |

Relay readiness is false when it cannot obtain or validate room leases. API readiness is false when
authorization-critical PostgreSQL access fails. Object-store failure may degrade readiness differently by
role: uploads and workers become unready while an existing relay may remain live until event or
snapshot policy requires object access.

## Observability

### Logs

Use structured logs containing timestamp, severity, service role, version, request or connection
identifier, tenant identifier, principal identifier when allowed, resource identifier, action,
outcome and safe error code.

Never log access tokens, share tokens, embed tokens, cookies, provider keys, raw document content or
presigned query strings.

### Metrics

Minimum metrics:

- HTTP request rate, latency and status by route template.
- WebSocket connections, joins, reconnects and disconnect reasons.
- Active rooms and people per room histogram.
- Collaboration commit latency and rejected batches.
- Lease acquisition, renewal and fencing failures.
- PostgreSQL pool use, transaction latency and serialization failures.
- Redis latency and ownership lookup failures.
- Object-store operation latency, bytes and checksum failures.
- Job queue depth, lease age, attempts and completion latency.
- Snapshot age and event backlog per document distribution.
- Webhook attempts and terminal failures.
- AI requests, latency, units, cost, cancellation and policy rejection.
- WASM execution time, fuel, memory and limit failures.

Tenant identifiers should not become unrestricted metric labels. Per-tenant usage belongs in
bounded aggregation tables and logs.

### Tracing

Propagate trace context through API, job, relay persistence, AI provider and object operations.
Presence frames may be sampled heavily; durable mutation and authorization spans are retained at a
higher rate.

## Backup

### PostgreSQL

- Continuous write-ahead-log archiving or managed point-in-time recovery.
- Daily logical or physical backup according to operator policy.
- Encryption at rest and in transit.
- Backup copies in a failure domain separate from the primary.
- Quarterly restoration exercise for production.

### S3-compatible object storage

- Bucket versioning.
- Lifecycle retention for snapshots, staging objects and deleted objects.
- Replication or provider durability appropriate to the plan.
- Inventory reconciliation against the PostgreSQL object table (see "Object reconciliation
  report" below).
- Independent backup of tenant encryption metadata when used.

### Object reconciliation report

`figcraft-cloud-storage::reconciliation::reconcile_tenant(transaction, store, limits)` compares one
tenant's `objects` rows (and, for staging keys, its `upload_reservations`) with a
`ListObjectsV2` listing of `tenants/<tenant>/` and returns a `ReconciliationReport`
(`format_version` 1, serde JSON). The report is read-only by construction. It sends only listing
`GET`s and runs only `SELECT`s, and it first switches its transaction to `READ ONLY`, so PostgreSQL
refuses any write later in the same transaction. Give it a transaction of its own. Run it after a
restore (step 4 of "Restore"), after an object-store incident, and on a schedule (weekly per tenant
is a reasonable start).

The top-level fields are `tenant_id`, `bucket`, `prefix`, `listing_started_unix`, `complete`,
`caps_reached`, `summary` (severity totals, rows scanned, keys and bytes listed, pages, rows in
other buckets, recent and unchecked rows and keys, open staging keys, staging and orphan bytes),
`counts` (exact per-category counts), `kinds` (live, verified and stored rows and bytes per object
kind) and one list per category:

| Category | Severity | Meaning | Action |
| --- | --- | --- | --- |
| `missing_objects`, state `verified` | critical | The database promises bytes the bucket does not have | Treat as data loss. Restore the key from a noncurrent bucket version or backup, then check the SHA-256 against the row. Never re-upload different bytes under the key. If nothing can be restored, mark the dependent snapshot, asset or version unavailable through the owning workflow. |
| `missing_objects`, state `pending` | info | An upload has not written its bytes yet | Nothing to do unless it also shows up as stuck. |
| `size_mismatches`, state `verified` | critical | The stored size differs from `size_bytes`: the key was overwritten | Restore the version whose size and SHA-256 match the row. Then investigate who wrote the key, because FigCraft never overwrites a verified key. |
| `size_mismatches`, other states | warning | An unverified row and its bytes disagree | Verification (or the garbage collector) settles it; re-run the report. |
| `orphan_keys`, `no_row` | warning | Bytes with no row | Re-run the report first: a young orphan (`last_modified` within minutes) may be a write whose row is not committed yet. Then check the backup that was restored for the row; delete the key only if no recovery point needs it. |
| `orphan_keys`, `deleted_row_only` | warning | The garbage collector marked the row deleted but the bytes stayed | Delete the key (bucket versioning keeps a noncurrent copy for the lifecycle period). |
| `orphan_keys`, `unsignable_key` | warning | A key FigCraft never writes (uppercase, spaces, …) | Something other than FigCraft wrote under the tenant prefix. Investigate credentials and remove the key with operator tooling. |
| `stale_staging_keys` | warning | Staged bytes whose reservation is missing (`no_reservation`), expired (`reservation_expired`) or closed (`reservation_closed`) | Run the reservation expiry sweep and delete the staging key. The tagged lifecycle rule removes any left after two days. |
| `stuck_objects` | warning | `pending` since creation, or `deleting` since deletion started, longer than `stuck_after` (default one hour); `in_bucket` says whether the key is listed | For `pending`: verify the object or let the reservation flow reject it. For `deleting`: re-run the garbage collector for the object. |
| `collectable_objects` | info | A `pending`, `verified` or `rejected` row with no reference and no retention hold; `eligible` once older than `MIN_COLLECTION_AGE` | The garbage collector handles it. A large or growing count points to a reference leak. |

Bounds: `ReconciliationLimits` caps rows (default 100 000, ceiling 1 000 000), listed keys (100 000
and 1 000 000), listing pages (200 and 10 000), keys per page (1 000) and findings per category
(1 000 and 10 000; the counts stay exact). Listing responses are read up to 4 MiB and parsed without
DTDs under a node limit, and keys are checked against the tenant prefix. Any cap that is reached is
named in `caps_reached`, and `complete` is then `false`. When the listing or the row scan stops
early, comparisons only cover the key range both sides fully read. Keys and rows outside it are
counted as `unchecked_keys` and `unchecked_rows`, so a cap never invents a missing or orphan
finding. To finish an incomplete report, raise the limits.

Consistency: the database clock is read before the listing starts. Rows created or verified after
that point are counted as `recent_rows` and not compared, and rows are read after the listing. Any
finding can be a race with live traffic, so re-run the report before acting on a single finding.
`clean()` is true only for a complete report with no critical and no warning findings.

### Redis

Redis data is reconstructible. Persistence may improve restart behavior but does not replace
PostgreSQL or object-store backup.

## Restore

1. Select a compatible PostgreSQL recovery point.
2. Restore PostgreSQL into an isolated environment.
3. Restore or attach the matching object-store version range.
4. Run schema compatibility and object-reference verification.
5. Rebuild Redis state from PostgreSQL and active connections.
6. Start workers without external webhook delivery until verification completes.
7. Sample document snapshot restoration and event replay.
8. Enable relay and API traffic.
9. Re-enable outbound webhooks and AI providers.
10. Record recovery point, lost interval if any and verification evidence.

## Backup and restore

A FigCraft Cloud backup has two halves that are only useful together: a PostgreSQL dump and a
manifest of the object versions that were current when the dump was taken. Object keys are
content-addressed but a key can still be overwritten or deleted, so the manifest records
version ids, not just keys. The bucket must be versioned (`initialize_bucket` enables it) and its
noncurrent-version retention (30 days by default) bounds how old a restorable backup can be.

### Taking a backup

1. Dump the database from one consistent snapshot as the administrator:
   `pg_dump --format=custom --no-owner --no-privileges --file=figcraft-<utc>.dump "$ADMIN_URL"`.
   `pg_dump` exports every table from one repeatable-read snapshot, so no extra lock is needed.
2. Record the schema version: `SELECT MAX(version) FROM _sqlx_migrations WHERE success`.
3. Immediately after the dump, list object versions per tenant prefix (`GET /<bucket>?versions&prefix=tenants/<tenant>/`,
   every page until `IsTruncated` is false) and store `(key, versionId, ETag, Size, IsLatest)` with
   the dump. Versions created between the dump and the listing are harmless: the manifest is
   filtered to the keys the dump's `objects` rows name, and restore selects the newest version
   whose bytes match the row's `sha256` and `size_bytes`.
4. Store the dump, schema version and manifest together, encrypted, in a separate failure domain.

### Restoring

The order matters, because each step is checked against the previous one.

1. Create an empty database and migrate it with the same binary release's
   `figcraft-cloud --migrate` up to the backed-up schema version; `check_schema_version` must
   report exactly that version. A dump is never restored into a different schema version.
2. Load data only: `pg_restore --data-only --disable-triggers --single-transaction --dbname=…`
   as the administrator. `documents.current_snapshot_id` and `document_snapshots` reference each
   other, so either disable triggers for the load (as above) or load `documents` without the
   pointer and set it after `document_snapshots`, which is what the S3-020 probe does.
   Run `provision_database_roles` afterwards so the runtime role's grants cover the restored tables.
3. For every `objects` row in a live state, find the manifest version for its key and `GET
   ?versionId=` it; its SHA-256 and size must equal the row. If the key's current version differs
   (the object was overwritten or deleted after the backup), copy the backed-up version back as
   current before enabling traffic. A row with no matching retained version is reported, never
   silently dropped.
4. Replay every restored document's events with `read_events` to its `current_seq`; replay
   verifies each event's checksum and contiguity.
5. Continue with step 5 of [Restore](#restore) (Redis, workers, traffic).

What must match after a restore: the schema version; the table set (every application table is in
the dump); each document's `current_seq`, `fencing_token` and `current_snapshot_id`; each live
object's key, `sha256` and `size_bytes` against the bytes of the manifest version; snapshot rows
against their objects. Anything written after the backup point (events, objects, rows, deletes) is
absent and is the recorded lost interval. A restored document's sequencer resumes with a new
claim, which advances the restored fencing token, so a writer from before the restore cannot
append.

`crates/cloud-storage/tests/backup_restore.rs` (S3-020) exercises this end to end against
PostgreSQL and the versioned bucket: it backs up a document (every application table from one
repeatable-read transaction, plus the version manifest), mutates both halves, restores into a
second migrated database and proves the backed-up state comes back and the mutations do not.
Fixtures: `deploy/saas/backup-restore-preflight.sql` and `backup-restore-cleanup.sql`.

## Scaling thresholds

Scale API replicas on CPU, request latency and database-pool saturation. Scale relay replicas on
active connections, active rooms, commit latency, queued outbound bytes and memory. Scale workers
on queue depth and oldest runnable job age.

Adding replicas must not increase total PostgreSQL connections beyond the configured database
budget. Each role has a separate bounded pool.

## Rolling releases

- API changes remain backward compatible with the previous web and desktop client release.
- Collaboration protocol capabilities are negotiated.
- Database changes follow expand, deploy, backfill and contract stages.
- Relay workers drain rooms or tell clients to reconnect before termination.
- Workers finish, return or safely lose their job lease before shutdown.
- The release process verifies upgrade from the previous supported version in Compose.
- Rollback is allowed only while the database schema remains within the prior binary's supported
  range.

## Docker Swarm profile

Compose remains the reference single-node package. When an operator has an established Docker
Swarm environment, provide a separate stack file derived from the same images and configuration:

- Put edge, API, relay and workers on an encrypted overlay network.
- Publish only the edge service; keep PostgreSQL, Redis and object-store administration private.
- Scale API, relay and worker roles independently.
- Use placement constraints and update policies that preserve relay drain time.
- Use Swarm secrets or an external secret provider instead of environment values for credentials.
- Use object storage and PostgreSQL as authoritative state; never use a shared filesystem volume as
  the collaborative document store.
- Run migrations as an explicit one-shot release action before application rollout.
- Validate restart, rollback, node loss and WebSocket reconnection in the same release matrix as
  Compose.

Swarm support is a packaging target, not a separate application architecture. It is implemented
only when a maintained deployment needs it and must not delay the Compose vertical slice.

## Kubernetes transition

Kubernetes is introduced after Compose validates service boundaries. Production resources include:

- Deployments for API, relay and workers.
- Services and ingress for HTTP and WebSocket routes.
- Jobs for migrations and S3-compatible bucket initialization.
- Pod disruption budgets for API and relay.
- Topology spread constraints.
- Network policies separating API, relay, workers, database, Redis, object storage and AI runtime.
- Horizontal autoscaling with bounded database connections.
- External secret integration.
- Graceful termination and relay drain hooks.

PostgreSQL, Redis and object storage should generally be managed outside the application chart in a
hosted SaaS. The chart may include development dependencies but must not imply that a single replica
is a highly available production database.

## Runbooks required before production

- Database unavailable.
- Redis unavailable or split ownership suspected.
- Object store unavailable or checksum mismatch.
- Relay overload and reconnect storm.
- Event backlog prevents snapshot compaction.
- Tenant storage quota exhausted.
- Provider cost spike or leaked AI credential.
- Webhook backlog.
- Migration failure.
- Backup restoration.
- Tenant isolation incident.
- Share-token or signing-key rotation.
- Region evacuation.

Each runbook identifies detection, immediate containment, customer impact, safe recovery,
verification and post-incident evidence.

## S2 gate procedure

The S2 gate ("create, edit, restart and restore a document using PostgreSQL metadata/events; a
failed commit is never confirmed and replay reaches the accepted state") is verified by one
end-to-end probe, `crates/cloud-relay/tests/s2_gate.rs`, run against a migrated disposable
database as the limited runtime role:

1. As the administrator, load `deploy/saas/s2-gate-preflight.sql` (only the synthetic tenant
   `99000000-…-0001` and its principal).
2. Run `FIGCRAFT_TEST_DATABASE_URL_FILE=<runtime url file> cargo test -p figcraft-cloud-relay --test
   s2_gate -- --test-threads=1`. Without the variable the probe skips, so a pass only counts when
   the database ran.
3. As the administrator, load `deploy/saas/s2-gate-cleanup.sql`, which removes every row under the
   tenant (events and audit rows are append-only for the runtime role). The probe reruns after it.

The probe uses public APIs only. It creates the workspace, project and document through the
metadata repositories and audits the lifecycle (create, rename, archive, restore). Two editors and a
viewer edit through a `DurableRoom`: creates and property sets are confirmed, and the viewer's batch
is refused and writes nothing. The confirmed batches must equal `read_events` exactly, in order,
with `current_seq` equal to the event count. The room is dropped and reopened over the same base,
and the replayed replica snapshot and sequence must equal the state before the restart. Two commits
are then made to fail: a sequencer fenced by a newer claim (`StaleFence`), and a room whose
connection is terminated with `pg_terminate_backend` while its append waits on the document row
(the connection is found through its unique `application_name`). Each `receive` must return an
error with nothing to deliver, the room must be poisoned, neither batch may be in the log, and a
fresh open must hold every confirmed batch and neither failed one. Finally the document is renamed
and archived: the live room's next commit fails with `InvalidState`, `DurableRoom::open` refuses it,
and after restore it opens again with its name and committed state intact.

The Compose smoke (`deploy/saas/smoke-preflight.sh`) runs the same three steps; record the passing
run as the gate's evidence in `docs/saas/tracker.md`.

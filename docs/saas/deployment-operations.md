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
| `migrate` | `figcraft-cloud --migrate` | PostgreSQL schema | one-shot, private network |
| `postgres` | supported PostgreSQL release | database volume | private network only |
| `redis` | supported Redis release | optional append-only volume | private network only |
| `seaweedfs` | pinned SeaweedFS `mini` | object volume | private network only |
| `seaweedfs-init` | pinned SeaweedFS `weed shell` | none | one-shot versioning verification |

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

Single-tenant mode still stores the configured tenant identifier on every row. It may provide an
initial bootstrap administrator, but bootstrap credentials expire or are removed after setup.

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
and the SQLx migration ledger has the supported schema version. It does not yet report
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
2. Run `figcraft-cloud --migrate` as an explicit one-shot release job. It applies embedded,
   versioned SQL under a PostgreSQL advisory lock; application roles never migrate on startup.
3. SeaweedFS creates the local bucket if absent; its initializer enables and verifies versioning.
   Lifecycle rules remain S3-014 work.
4. Redis becomes reachable.
5. API and relay roles start and pass readiness checks.
6. Workers start and claim jobs.
7. Gateway exposes the deployment.

Application replicas do not race to run schema migrations during normal startup. A release fails
closed when its database schema is outside the binary's supported range.

## Database migrations

The cloud image's `--migrate` command uses SQLx's embedded migration set and holds PostgreSQL's
advisory migration lock while applying pending migrations. It reads the database connection from
the configured `DATABASE_URL` secret and reports fixed diagnostics without echoing credentials or
driver errors. Migration application is deliberately separate from readiness and normal role
startup. At process startup, readiness checks the highest successfully applied migration version
against the binary's supported inclusive range. A missing ledger or unsupported version keeps
`/readyz` at 503 while liveness remains available. The S2-005 event and operations migration has
version `20261016000002`, which is the current supported version. Schema rollback policy is handled by
later S2 tasks.

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
- Inventory reconciliation against the PostgreSQL object table.
- Independent backup of tenant encryption metadata when used.

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

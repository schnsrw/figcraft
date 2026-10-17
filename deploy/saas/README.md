# FigCraft SaaS deployment

This directory contains the cloud image, static web image and single-node Compose topology. The
topology is a development scaffold, **not a healthy SaaS deployment**: PostgreSQL, Redis and
SeaweedFS can run locally, but cloud API, relay and worker services have not been implemented.
The dependency review is in
[`docs/saas/dependency-images.md`](../../docs/saas/dependency-images.md).
Build the cloud image from the repository root with:

```sh
docker build -f deploy/saas/cloud.Dockerfile -t figcraft-cloud:dev .
```

The image uses a Rust builder stage and a non-root distroless runtime. Run it with a read-only root
filesystem (`--read-only`) and mount secret files read-only. The transport server listens on port
8081. `/api/v1/healthz` returns a transport probe for `api` and `all`; `/ws/v1/health` validates a
WebSocket upgrade for `relay` and `all`, then closes normally. `/readyz` requires the listener and
the supported database schema version to be ready. These probes do not verify authentication,
document operations or durable collaboration; those product services remain future milestones. The
edge `/healthz` is only gateway liveness.

`web.Dockerfile` builds the existing npm workspace with Node 22.18.0, then copies the static build
into Caddy. `compose.yaml` defines the gateway, web, cloud, PostgreSQL, Redis, SeaweedFS and
one-shot database role-provisioning, migration and bucket-versioning jobs. The gateway publishes HTTP on loopback port 8080 only. It
does not configure production TLS or expose the data services. SeaweedFS `mini` creates the
configured bucket if absent; the initializer enables versioning and verifies it on every startup.
No object-store credentials appear in environment variables or command arguments. Lifecycle rules
remain S3-014 work.

The image uses the Rust 1.95 Bookworm builder and the Debian 12 distroless runtime. These tags are
versioned but not digest-pinned; production digest pinning, SBOM and provenance are release work.
PostgreSQL, Redis and SeaweedFS are digest-pinned in `env.example`. Their security updates remain
an operator's explicit review and re-pin task.

## Startup and persistence

`postgres_data`, `redis_data` and `seaweedfs_data` are named Docker volumes. The selected PostgreSQL
18 image uses `/var/lib/postgresql` as the volume target, matching its
[official image documentation](https://hub.docker.com/_/postgres). Do not carry that path across a
major upgrade without a migration plan. Redis enables append-only persistence under `/data`, even
though cloud correctness must treat Redis as replaceable state.

Compose waits for PostgreSQL health, provisions limited database roles, runs migrations as the
migrator, reconciles runtime grants, and then creates the cloud container after Redis and bucket
health checks. The gateway waits for both
web health and cloud readiness. `figcraft-cloud --check-ready` probes `/readyz` with a bounded
local HTTP request. At startup, `/readyz` is healthy only when the listener is bound and the SQLx
migration ledger is at the supported schema version `20261017000007`. This does not imply
database-backed product operations work.
The one-shot SeaweedFS initializer retries failures and fails closed if versioning cannot be
confirmed. The edge `/healthz` is only liveness after the gateway starts.

## Configuration template

Copy `env.example` to `.env` only when preparing a local stack. The checked-in template contains
only non-secret local values, reserved `.invalid` hostnames and paths where containers receive
mounted secrets. `.env` and the whole `secrets/` directory are ignored by Git. Validate the
topology without starting containers using
`docker compose --env-file deploy/saas/env.example -f deploy/saas/compose.yaml config --quiet`
from the repository root. Secret files are required before starting data services.

The application settings follow [`docs/saas/deployment-operations.md`](../../docs/saas/deployment-operations.md):

| Area | Non-secret settings | Secret files |
|---|---|---|
| Deployment | `FIGCRAFT_ENV`, `FIGCRAFT_DEPLOYMENT`, `FIGCRAFT_ROLE`, `FIGCRAFT_PUBLIC_URL`, `FIGCRAFT_SINGLE_TENANT_ID` | none |
| PostgreSQL | none | runtime `DATABASE_URL_FILE`; one-shot jobs additionally mount administrator and migrator URLs and separate role passwords |
| Redis | none | `REDIS_URL_FILE` |
| Object storage | `S3_ENDPOINT`, `S3_REGION`, `S3_BUCKET`, `S3_FORCE_PATH_STYLE` | `S3_ACCESS_KEY_FILE`, `S3_SECRET_KEY_FILE` |
| Identity | `OIDC_ISSUER`, `OIDC_CLIENT_ID`, `OIDC_AUDIENCE` | `OIDC_CLIENT_SECRET_FILE` |
| Sessions and embedding | `EMBED_ISSUER_CONFIG_FILE` | `SESSION_SIGNING_KEY_FILE` |
| Diagnostics | `RUST_LOG`, optional `OTEL_EXPORTER_OTLP_ENDPOINT` | none |

## Secret handling

- Generate unique database, Redis, object-storage, OIDC and session credentials for every
  deployment. Do not reuse values from documentation or another environment.
- Store local Compose values under the ignored `deploy/saas/secrets/` directory. Production uses
  an orchestrator secret store or dedicated secret manager.
- Supply `database_url`, `redis_url`, `s3_access_key`, `s3_secret_key`, `oidc_client_secret` and
  `session_signing_key` for the cloud service. `database_url` must authenticate as
  `figcraft_runtime`. Supply `admin_database_url`, `migration_database_url`,
  `migrator_password` and `runtime_password` for the one-shot database jobs; the administrator URL
  must use the PostgreSQL `postgres_password`, while the migration URL must use
  `figcraft_migrator` and `migrator_password`. The Redis `redis.conf` and newline-free `redis_password` must agree with
  `redis_url`. The Redis health probe reads its password on stdin, never in argv.
- Supply a SeaweedFS S3 identity configuration at `secrets/seaweedfs-s3-config.json`, mounted as
  `/run/secrets/seaweedfs_s3_config`. Its application identity's access and secret keys must agree
  with `s3_access_key` and `s3_secret_key`; give it only the actions needed by the application.
  SeaweedFS `mini` and the one-shot `weed shell` initializer use private internal endpoints; do
  not publish their master, filer, volume or admin ports. Provision the identity file through a
  trusted secret-management process; never commit credentials.
- Mount secrets read-only and pass only their in-container paths through `_FILE` settings.
- Never place credentials in `.env`, Compose YAML, image layers, command-line arguments, logs or
  support bundles.
- The cloud configuration rejects unreadable secret files and development defaults in production
  before starting the health server (S1-009 and S1-010).

D-001 selects the local data-service images; this is still a single-node development topology, not
a production security or backup design. S1-018 verifies transport routing with diagnostic API and
WebSocket probes; it does not validate application workflows.

The disposable `smoke-preflight.sh` exercises the Compose transport contract from synthetic,
randomly generated secrets. Run `bash deploy/saas/smoke-preflight.sh` from any directory with Docker
available. It builds the local images, checks service health, bucket initialization and both
PostgreSQL migrations, verifies database role privileges and tenant/document-scope constraints in
rolled-back transactions, checks transaction-local tenant context against a synthetic RLS policy
and tenant/workspace/project repositories against bounded, cross-tenant fixtures using the runtime
role, starts each cloud role, and verifies web content, API and WebSocket routing plus rejection of
unsafe production configuration. It removes its uniquely named containers, volumes and temporary
secrets on exit.
The API and WebSocket endpoints are diagnostic probes, not application workflows. If a machine or
tool is forcibly stopped before cleanup completes, use the printed project name with `docker compose --project-name <name> --env-file
deploy/saas/env.example --file deploy/saas/compose.yaml down --volumes --remove-orphans` to remove
only that disposable project.

Run `figcraft-cloud --provision-db-roles` before and after the one-shot `--migrate` action. The
first pass creates the constrained logins and transfers existing table ownership; the second grants
runtime access to newly migrated tables. The provision jobs alone receive the administrator URL.
The migration job receives the migrator URL through `DATABASE_URL_FILE` and applies embedded
versioned SQL under a PostgreSQL advisory lock, and emits only fixed, non-secret diagnostics. Normal
role startup never applies migrations. The S2-005 event, snapshot, job, idempotency and audit
schema is present, but repositories and durable collaboration behavior remain future S2 tasks.

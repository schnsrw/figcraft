# FigCraft SaaS deployment

This directory contains the cloud image, static web image and single-node Compose topology. The
topology is a development scaffold, **not a runnable or healthy SaaS deployment**: D-001 has not
selected the PostgreSQL, Redis, MinIO and matching MinIO-client image releases; cloud API, relay
and worker services have not been implemented; health and dependency ordering are follow-up work.
Build the cloud image from the repository root with:

```sh
docker build -f deploy/saas/cloud.Dockerfile -t figcraft-cloud:dev .
```

The image uses a Rust builder stage and a non-root distroless runtime. Run it with a read-only root
filesystem (`--read-only`) and mount secret files read-only. Its health-only server listens on port
8081; `/readyz` remains 503 until the API, relay and worker services are implemented. The gateway
already routes `/api/v1/*` and `/ws/v1/*` to that port, but those application routes do not exist
yet. Its `/healthz` is only an edge liveness response, not a cloud readiness signal.

`web.Dockerfile` builds the existing npm workspace with Node 22.18.0, then copies the static build
into Caddy. `compose.yaml` defines the gateway, web, cloud, PostgreSQL, Redis, MinIO and a one-shot
MinIO bucket initializer. The gateway publishes HTTP on loopback port 8080 only. It does not
configure production TLS or expose the data services. The bucket initializer uses an operator-
provided MinIO Client config file, not credentials in environment variables or command arguments.
It creates a missing bucket with versioning; lifecycle rules and verification of an existing
bucket's versioning are future work.

The image uses the Rust 1.95 Bookworm builder and the Debian 12 distroless runtime. These tags are
versioned but not digest-pinned; production digest pinning, SBOM and provenance are release work.
The D-001 decision still blocks selecting PostgreSQL, Redis and MinIO service image versions.

## Startup and persistence

`postgres_data`, `redis_data` and `minio_data` are named Docker volumes. The PostgreSQL target is
required as `POSTGRES_DATA_DIR` because the [official PostgreSQL image](https://hub.docker.com/_/postgres)
changed its declared volume location in major version 18. Set that path from the reviewed image's
documentation; do not carry a volume path across a major upgrade without a migration plan. Redis
enables append-only persistence under `/data`, even though cloud correctness must treat Redis as
replaceable state.

Compose waits for PostgreSQL and Redis health checks and for successful MinIO bucket initialization
before creating the cloud container. The gateway waits for both web health and cloud readiness.
`figcraft-cloud --check-ready` probes `/readyz` with a bounded local HTTP request and returns failure
while the role runtime is unfinished; consequently the gateway correctly remains unstarted today.
The one-shot MinIO initializer retries failures, but the selected client release and its credentials
still need D-001 validation. The edge `/healthz` is only liveness after the gateway starts.

## Configuration template

Copy `env.example` to `.env` only when preparing a local stack. Fill in reviewed image references
for `POSTGRES_IMAGE`, `REDIS_IMAGE`, `MINIO_IMAGE` and `MINIO_MC_IMAGE`, plus the matching
`POSTGRES_DATA_DIR`, after D-001 is resolved; Compose refuses to render without them. The checked-in
template contains only non-secret local values, reserved `.invalid` hostnames and paths where
containers receive mounted secrets. `.env` and the whole `secrets/` directory are ignored by Git.
With image references supplied, validate
the topology without starting containers using `docker compose -f deploy/saas/compose.yaml config
--quiet` from the repository root.

The application settings follow [`docs/saas/deployment-operations.md`](../../docs/saas/deployment-operations.md):

| Area | Non-secret settings | Secret files |
|---|---|---|
| Deployment | `FIGCRAFT_ENV`, `FIGCRAFT_DEPLOYMENT`, `FIGCRAFT_ROLE`, `FIGCRAFT_PUBLIC_URL`, `FIGCRAFT_SINGLE_TENANT_ID` | none |
| PostgreSQL | none | `DATABASE_URL_FILE` |
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
  `session_signing_key` for the cloud service. The PostgreSQL `postgres_password` must agree with
  `database_url`; the Redis `redis.conf` and newline-free `redis_password` must agree with
  `redis_url`. The Redis health probe reads its password on stdin, never in argv.
- Supply a MinIO server environment file at `secrets/minio.env` and a MinIO Client `config.json`
  at `secrets/minio-mc-config.json`. The client config must contain an alias named `local` for
  `http://minio:9000`, using credentials authorized to create the configured bucket. Keep the
  server, client and cloud object-store credentials consistent. Provision these files through a
  trusted secret-management process; never commit sample credentials.
- Mount secrets read-only and pass only their in-container paths through `_FILE` settings.
- Never place credentials in `.env`, Compose YAML, image layers, command-line arguments, logs or
  support bundles.
- The cloud configuration rejects unreadable secret files and development defaults in production
  before starting the health server (S1-009 and S1-010).

The dependency image release policy is still awaiting owner decision D-001. This Compose file
requires those image references and the PostgreSQL mount target rather than guessing production
versions. S1-016 adds named volumes, health checks and fail-closed ordering; it does not claim
runtime validation of the unselected data-service images. A healthy API/web/WebSocket smoke test
is S1-018, after the cloud application routes exist.

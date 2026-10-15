# FigCraft SaaS deployment

This directory contains the first cloud image and the safe environment contract; the reference
Docker Compose deployment for local development and single-node operation is still pending. Build
the cloud image from the repository root with:

```sh
docker build -f deploy/saas/cloud.Dockerfile -t figcraft-cloud:dev .
```

The image uses a Rust builder stage and a
non-root distroless runtime. Run it with a read-only root filesystem (`--read-only`) and mount
secret files read-only. Its health-only server listens on port 8081; `/readyz` remains 503 until
the API, relay and worker services are implemented. There is not yet a runnable SaaS Compose stack;
gateway, web, data services and healthy end-to-end routing remain future work.

The image uses the Rust 1.95 Bookworm builder and the Debian 12 distroless runtime. These tags are
versioned but not digest-pinned; production digest pinning, SBOM and provenance are release work.
The D-001 decision still blocks selecting PostgreSQL, Redis and MinIO service image versions.

## Configuration template

Copy `env.example` to `.env` when the Compose stack is available. The checked-in template contains
only non-secret local values, reserved `.invalid` hostnames and paths where containers will receive
mounted secrets. `.env` and the whole `secrets/` directory are ignored by Git.

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
- Mount secrets read-only and pass only their in-container paths through `_FILE` settings.
- Never place credentials in `.env`, Compose YAML, image layers, command-line arguments, logs or
  support bundles.
- The cloud configuration rejects unreadable secret files and development defaults in production
  before starting the health server (S1-009 and S1-010).

The dependency image release policy is still awaiting owner decision D-001. Compose files must not
guess PostgreSQL, Redis or MinIO production versions before that decision is recorded.

# FigCraft SaaS deployment

This directory will contain the reference Docker Compose deployment for local development and
single-node operation. It currently establishes only the safe environment contract from S1-015.
There is no runnable SaaS stack yet: the cloud binary, container images, Compose topology and
health checks land in S1-007 through S1-016.

## Configuration template

Copy `env.example` to `.env` when the Compose stack is available. The checked-in template contains
only non-secret local values, reserved `.invalid` hostnames and paths where containers will receive
mounted secrets. `.env` and the whole `secrets/` directory are ignored by Git.

The application settings follow [`docs/saas/deployment-operations.md`](../../docs/saas/deployment-operations.md):

| Area | Non-secret settings | Secret files |
|---|---|---|
| Deployment | `FIGCRAFT_DEPLOYMENT`, `FIGCRAFT_ROLE`, `FIGCRAFT_PUBLIC_URL`, `FIGCRAFT_SINGLE_TENANT_ID` | none |
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
- The cloud application must reject missing secret files and development defaults in production;
  that behavior belongs to S1-009 and S1-010 and is not claimed here.

The dependency image release policy is still awaiting owner decision D-001. Compose files must not
guess PostgreSQL, Redis or MinIO production versions before that decision is recorded.

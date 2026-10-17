# FigCraft SaaS platform

> **Status:** architecture baseline · **Last updated:** 2026-10-17 · **Owner:** fork maintainers

This directory defines the SaaS extension maintained by this fork. It turns FigCraft's existing
local-first editor and collaboration server into a deployable platform for three product modes:

1. **Single tenant:** one organization runs a private FigCraft service.
2. **Multi-tenant SaaS:** one deployment serves isolated organizations and workspaces.
3. **Embedded workspace:** another product offers FigCraft inside its own workspace and delegates
   identity and access through signed integration tokens.

The editor remains local-first. The desktop and browser clients keep the document engine,
rendering, interaction logic and command registry. The cloud platform owns accounts, tenancy,
authorization, durable collaboration, object storage, integration APIs, AI routing, quotas,
auditing and operations.

## Documents

| Document | Purpose |
|---|---|
| [Product requirements](product-requirements.md) | Scope, personas, deployment modes, functional requirements and non-goals |
| [Architecture](architecture.md) | Target topology, service boundaries, collaboration consistency and failure handling |
| [Data, tenancy and integration](data-tenancy-integration.md) | PostgreSQL model, S3 object layout, identity, permissions, API and embedding contracts |
| [AI and WASM](ai-wasm.md) | Provider-neutral AI relay, command execution, server-side WASM and safety boundaries |
| [Deployment and operations](deployment-operations.md) | Docker profiles, configuration, migrations, scaling, backup, restore and observability |
| [Dependency image review](dependency-images.md) | Reviewed PostgreSQL, Redis and SeaweedFS image pins and local data-service evidence |
| [Security](security.md) | Threat model, tenant isolation, secret handling, abuse controls and release gates |
| [Upstream synchronization](upstream-sync.md) | Fork strategy, conflict reduction, sync cadence and verification process |
| [Enterprise proposal review](proposal-review.md) | Source-audited disposition of the supplied PRD/HLD/LLD and its repository-specific corrections |
| [Roadmap](roadmap.md) | Milestones, dependencies, estimates and exit gates |
| [Filesystem migration report](filesystem-migration-report.md) | Read-only `figcraft-server` data-directory scanner and the versioned migration report format (S2-021) |
| [Object store client decision](object-store-client-decision.md) | S3-compatible client choice (`rusty-s3` signing over `reqwest`/Rustls/ring), licences and alternatives (S3-001) |
| [Task tracker](tracker.md) | Complete checkbox inventory for implementation and release readiness |

## Architecture decisions

These decisions are the default until a replacement is recorded in this directory.

| ID | Decision | Reason |
|---|---|---|
| SAAS-ADR-001 | Preserve the local Rust engine and renderer in clients | Editing remains responsive and offline-capable; the server does not become a remote-rendered editor |
| SAAS-ADR-002 | Build one modular Rust cloud application with `all`, `api`, `relay` and `worker` roles | A solo SaaS can deploy one image, while larger deployments scale roles independently |
| SAAS-ADR-003 | Keep `figcraft-server` as the filesystem self-hosted option | Existing offline and small-team users retain a low-dependency server |
| SAAS-ADR-004 | Use PostgreSQL as the metadata and ordered-event source of truth | Transactions, constraints, tenancy and recoverability belong in a relational store |
| SAAS-ADR-005 | Use MinIO or S3-compatible storage for immutable binary objects | Snapshots, images, libraries, plugins, exports and thumbnails are large and content-oriented |
| SAAS-ADR-006 | Use Redis only for replaceable coordination data | Presence, caching and rate limits may be rebuilt; confirmed edits may not depend on Redis persistence. S5 room leases and routing live in PostgreSQL by the later [owner decision](relay-orchestration-decision.md) |
| SAAS-ADR-007 | Assign one authoritative relay worker to a document at a time | The current collaboration protocol relies on one server order and last-writer-wins properties |
| SAAS-ADR-008 | Require fencing tokens on room ownership | A worker that lost its lease must not append events or confirm edits |
| SAAS-ADR-009 | Use one tenant-aware schema for all deployment modes | Single-tenant installations can migrate to multi-tenant without moving to a different data model |
| SAAS-ADR-010 | Run untrusted server-side WASM only in isolated workers | API and realtime processes must not execute customer code |
| SAAS-ADR-011 | Route AI through a provider-neutral server gateway | Provider secrets, quotas, policy, audit and local-model support stay out of clients |
| SAAS-ADR-012 | Apply AI mutations through the command and collaboration path | AI edits receive authorization, validation, history, synchronization and undo semantics |
| SAAS-ADR-013 | Keep fork-owned work in additive directories and adapter seams | Regular upstream merges should not repeatedly conflict with editor implementation files |
| SAAS-ADR-014 | Permit TypeScript only in a fork-owned React SaaS shell and browser bridge | React is useful for account, workspace and administration UX; Rust remains authoritative for editing, collaboration and persistence |
| SAAS-ADR-015 | Defer NATS or another durable message bus | PostgreSQL jobs and an outbox are sufficient until measured throughput or fan-out proves otherwise |
| SAAS-ADR-016 | Give each cloud document one identity across browser and desktop | Opening a cloud file must not create unrelated local copies or split its history |
| SAAS-ADR-017 | Send large document images as verified asset references, bytes over HTTP to object storage | Inline base64 images are limited to about 11 MiB by the 16 MiB event cap and loop the sender above it; see [large-images.md](large-images.md) |

## Target repository layout

```text
apps/
  figcraft-cloud/          one binary with all, api, relay and worker roles
  figcraft-server/         existing filesystem-backed self-hosted server
  figcraft-web/            existing Rust/WASM browser editor

webapp/
  saas/                    React and TypeScript workspace shell
  packages/editor-bridge/  typed lifecycle around the Rust/WASM editor
  packages/api-client/     generated or schema-verified HTTP client

crates/
  cloud-domain/            tenant, workspace, identity, role and quota types
  cloud-auth/              OIDC, sessions, service tokens and embed-token validation
  cloud-storage/           PostgreSQL repositories, migrations and S3 object operations
  cloud-relay/             room leases, routing and database-backed collaboration persistence
  cloud-jobs/              durable job execution and scheduling
  cloud-ai/                AI providers, policy, context building and command application
  cloud-wasm/              isolated server-side automation runtime

deploy/saas/
  compose.yaml
  Caddyfile
  env.example
  postgres/
  seaweedfs/
  observability/
  kubernetes/

docs/saas/
  architecture and delivery documentation
```

The exact crate split may be consolidated when implementation shows two crates always change
together. Service boundaries and dependency direction matter more than the number of crates.

## Decision review record

| Review | Date | Authority | Scope | Outcome | Amendments |
|---|---|---|---|---|---|
| S0-014 | 2026-10-16 | Product owner, by explicit session authorization | SAAS-ADR-001 through SAAS-ADR-016 | Accepted as written | None |

This acceptance authorizes implementation that depends on the architecture baseline. It does not
assign the architecture, security or operations reviewer identities tracked separately by S0-013.
For the local Compose implementation of SAAS-ADR-005, D-001 selects maintained Apache-2.0
SeaweedFS 4.48 in place of archived MinIO; see the [dependency image review](dependency-images.md).
The S3-compatible contract remains provider-neutral for hosted deployments.

## Delivery rules

- Preserve the existing clean-room and asset-provenance requirements.
- Before adding TypeScript, update the fork's root agent policy to define the narrow `webapp/`
  exception, its lint/test gates and its dependency and license rules.
- Keep handwritten TypeScript and JavaScript inside `webapp/`; the editor engine, collaboration,
  storage, AI/WASM, native applications and server remain Rust.
- Keep the editor functional without cloud services.
- Keep `figcraft-server` functional while the cloud backend is introduced.
- Add cloud functionality through new crates and narrow traits before changing shared editor code.
- Every stored resource carries a tenant identifier, including jobs, events and audit records.
- Every externally supplied identifier is authorized within its tenant before use.
- Confirm a collaboration edit only after its durable event commit succeeds.
- Never expose PostgreSQL, Redis, object-store administration or AI provider credentials to clients.
- Every milestone must satisfy its exit gate in [roadmap.md](roadmap.md).
- Update [tracker.md](tracker.md) in the same commit as completed work.

## Current baseline

FigCraft already supplies the difficult editor-side foundations:

- A persistent document model and command engine.
- Binary collaboration messages and deterministic server ordering.
- Optimistic clients, per-user undo, reconnect and offline queues.
- Desktop and browser WebSocket clients.
- A `Backend` seam in `figcraft-collab-net`.
- A filesystem server with snapshots, append-only logs, share links, libraries and plugins.
- A Rust/WASM browser build and Docker packaging for the current server.

The SaaS work is therefore a control-plane, persistence, distributed-coordination and product
integration project rather than a rewrite of the editor.

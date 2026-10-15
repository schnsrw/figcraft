# SaaS implementation tracker

> **Status:** S1 cloud foundation complete (22/22) · **Last updated:** 2026-10-16

This is the authoritative execution checklist for the fork's SaaS platform. Check a task only when
its acceptance evidence exists in the repository or linked release evidence. Update this file in
the same commit as the completed task.

## Status legend

- `[ ]` not completed
- `[x]` completed and verified
- A blocked task remains unchecked and records its blocker beneath the task group.
- Estimates live in [roadmap.md](roadmap.md); this file tracks scope and evidence.

## Milestone status

| Milestone | Status | Completed tasks | Gate |
|---|---|---:|---|
| S0 Documentation and fork governance | in progress | 14 / 15 | Reviewer assignment |
| S1 Cloud foundation | complete | 22 / 22 | Healthy role-selectable image and Compose stack |
| S2 PostgreSQL metadata and events | in progress | 2 / 21 | Durable transactional event path |
| S3 Object storage and migration | not started | 0 / 20 | Verified object storage and repeatable migration |
| S4 Identity, tenancy and authorization | not started | 0 / 25 | Isolation and role suite passes |
| S5 Distributed collaboration relay | not started | 0 / 25 | Fenced multi-replica relay passes failure tests |
| S6 Browser, desktop and embedded integration | not started | 0 / 31 | All clients complete cloud workflows |
| S7 AI gateway and server WASM | not started | 0 / 27 | Safe auditable command and asset jobs |
| S8 Operations, quotas, billing and security | not started | 0 / 33 | Production security and restore gates pass |
| S9 Production rollout | not started | 0 / 18 | General-availability review passes |

## S0 — Documentation and fork governance

- [x] **S0-001** Create the SaaS documentation index and architecture-decision baseline.
  Evidence: `docs/saas/README.md`.
- [x] **S0-002** Define personas, deployment modes, functional requirements and non-goals.
  Evidence: `docs/saas/product-requirements.md`.
- [x] **S0-003** Define modular application roles, topology, consistency and failure behavior.
  Evidence: `docs/saas/architecture.md`.
- [x] **S0-004** Define PostgreSQL, MinIO, tenancy, authentication and API contracts.
  Evidence: `docs/saas/data-tenancy-integration.md`.
- [x] **S0-005** Define provider-neutral AI and isolated server-WASM architecture.
  Evidence: `docs/saas/ai-wasm.md`.
- [x] **S0-006** Define Docker, scaling, migrations, backup, restore and observability.
  Evidence: `docs/saas/deployment-operations.md`.
- [x] **S0-007** Define threat boundaries, controls, security tests and production gate.
  Evidence: `docs/saas/security.md`.
- [x] **S0-008** Define upstream remote, branch, merge, patch-ledger and verification policy.
  Evidence: `docs/saas/upstream-sync.md`.
- [x] **S0-009** Define milestones, dependencies, estimates and release slices.
  Evidence: `docs/saas/roadmap.md`.
- [x] **S0-010** Create this task-level implementation tracker.
- [x] **S0-011** Reconcile the supplied enterprise PRD/HLD/LLD with actual repository constraints.
  Evidence: `docs/saas/proposal-review.md`.
- [x] **S0-012** Amend the root agent policy with a directory-scoped `webapp/` TypeScript exception,
  toolchain gates and dependency/license rules before adding browser-shell code.
  Evidence: root `AGENTS.md` section "Fork-owned web application".
- [ ] **S0-013** Assign architecture, security and operations reviewers without guessing identities.
- [x] **S0-014** Review decisions SAAS-ADR-001 through SAAS-ADR-016 and record accepted changes.
  Evidence: the product owner explicitly accepted SAAS-ADR-001 through SAAS-ADR-016 as written on
  2026-10-16; `docs/saas/README.md` records the scope, outcome and absence of amendments.
- [x] **S0-015** Configure the `upstream` remote and perform the first documented synchronization
  rehearsal on a temporary branch.
  Evidence: GitHub fork metadata verified `storytold/figcraft` as the parent and source; the local
  `upstream` remote and tags were fetched, and `docs/saas/upstream-sync.md` records the clean
  temporary-branch rehearsal, exact boundaries, impact review and passing gates.

## S1 — Cloud foundation

### Repository and domain

- [x] **S1-001** Add fork-owned workspace members under `apps/figcraft-cloud` and `crates/cloud-*`.
  Depends on S0-014.
  Evidence: the workspace glob discovers the compileable `figcraft-cloud` application and the
  `figcraft-cloud-domain`, `-auth`, `-storage`, `-relay`, `-jobs`, `-ai` and `-wasm` library crates;
  all inherit the workspace edition, license and crash-prevention lints and introduce no external
  dependencies. Patch-ledger entry FP-001 records the generated root lockfile additions.
- [x] **S1-002** Add `cloud-domain` identifiers for tenant, workspace, project, cloud document,
  principal, object, job and request.
  Evidence: `figcraft-cloud-domain` exposes distinct, non-interchangeable UUID-backed identifier
  types with random UUIDv4 generation, canonical non-nil parsing, safe non-echoing parse errors,
  stable display and validated string serialization; focused tests cover every identifier kind and
  generated UUID version. Patch-ledger entry FP-002 records the root lockfile dependency edge.
- [x] **S1-003** Define `TenantContext`, deployment mode, role, permission, resource state and quota
  value types without infrastructure dependencies.
  Evidence: `figcraft-cloud-domain` defines the three deployment modes, non-hierarchical access
  roles, explicit sensitive permissions, lifecycle states, checked quota values and limits, and an
  immutable `TenantContext`; focused tests cover permission separation, state gating, quota
  boundaries and overflow without adding database, HTTP, Redis, object-storage or UI dependencies.
- [x] **S1-004** Define stable cloud error codes and safe public error serialization.
  Evidence: `figcraft-cloud-domain` defines eleven explicit machine codes with fixed public
  messages and a `PublicError` envelope that can serialize only the code, allowlisted message and
  request ID. Tests pin the JSON shape, cover every code and prove rejected code text is not echoed.
  Patch-ledger entry FP-003 records the root lockfile's test-only dependency edge.
- [x] **S1-005** Add serialization, invalid-value and identifier property tests.
  Evidence: `crates/cloud-domain/tests/domain.rs` pins every identifier and policy wire shape,
  rejects nil, alternate, hostile and wrong-type values, and property-tests every non-nil UUID plus
  arbitrary input text. Patch-ledger entry FP-004 records the test-only property-test dependency.
- [x] **S1-006** Extend `cargo xtask layers` rules for cloud crates without changing existing layer
  permissions.
  Evidence: `xtask/src/layers.rs` places cloud-domain at L7, cloud adapters at L8 and the cloud
  application at L9, above existing L0–L6 editor crates. It keeps cloud-domain free of UI and
  networking dependencies; `cargo xtask layers` passes without changing existing layer allowances.
  Patch-ledger entry FP-005 records the upstream-owned checker extension.

### Application roles and configuration

- [x] **S1-007** Create `figcraft-cloud` with `all`, `api`, `relay` and `worker` role selection.
  Evidence: `apps/figcraft-cloud` defaults to `all`, accepts each role through `--role value` or
  `--role=value`, supports help, and rejects unknown, missing and duplicate arguments. The health
  server reports the selected role and becomes ready after binding its listener; product services
  remain explicit future work.
- [x] **S1-008** Implement strict configuration parsing with unknown-key rejection.
  Evidence: `apps/figcraft-cloud/src/config.rs` parses the shared deployment modes and runtime
  roles, recognizes the documented cloud setting names, rejects unknown cloud-namespaced keys,
  duplicate keys, empty values and invalid enums with non-echoing diagnostics, and ignores unrelated
  process environment. Settings without an implemented consumer are validated but not retained.
  Patch-ledger entry FP-006 records the cloud binary's dependency on the shared domain types.
- [x] **S1-009** Support mounted secret files and redact secret values from debug and errors.
  Evidence: `apps/figcraft-cloud/src/config.rs` reads configured secret-file settings with a 64 KiB
  cap, validates UTF-8/non-empty contents, and strips trailing newlines. Secret wrappers redact
  `Debug`; read, size, encoding and empty-file errors are fixed and do not include paths or values.
  Tests exercise successful reads, redaction and a failing file lookup.
- [x] **S1-010** Reject insecure development defaults when production mode is selected.
  Evidence: `FIGCRAFT_ENV=production` requires non-loopback HTTPS public and identity URLs and all
  core credentials via mounted secret files; HTTP/loopback/reserved `.invalid` endpoints and inline
  credentials are refused. `deploy/saas/env.example` explicitly selects development mode. Tests pin
  both the production rejection cases and a valid mounted-secret configuration.
- [x] **S1-011** Add liveness, readiness and startup endpoints per role.
  Evidence: `figcraft-cloud` serves bounded HTTP `GET /healthz`, `/readyz` and `/startupz` routes on
  port 8081 and includes the selected runtime role. Readiness returns 200 after the transport
  listener starts and 503 before it does; liveness/startup do not claim readiness. Database and
  product-service checks remain future work. Tests exercise each role, route errors, readiness
  transitions and real local TCP requests.
- [x] **S1-012** Add request IDs, structured logs and OpenTelemetry context propagation foundation.
  Evidence: health responses receive generated UUID request IDs; each request emits one JSON stdout
  record with timestamp, level, event, role, status and request ID. Valid W3C version-00
  `traceparent` headers are forwarded and their trace/span IDs are logged; malformed context is
  dropped. This is propagation only, not an OTLP exporter. Patch-ledger entry FP-007 records the
  cloud binary's additive `serde_json` lockfile edge.

### Containers and CI

- [x] **S1-013** Build a multi-stage, non-root, read-only-root `figcraft-cloud` image.
  Evidence: `deploy/saas/cloud.Dockerfile` builds with Rust 1.95 Bookworm and runs from Debian 12
  distroless as UID 65532; a focused Docker ignore file excludes local secrets and build output.
  The image built successfully and served `/healthz` and `/readyz` (200 after listener startup)
  with `--read-only --user 65532:65532`. Readiness indicates transport probes are serving, not that
  application operations can access a schema or persist data. Base tags are versioned but not
  digest-pinned; release pinning, SBOM and provenance remain follow-up hardening.
- [x] **S1-014** Add `deploy/saas/compose.yaml` with gateway, web, cloud, PostgreSQL, Redis,
  S3-compatible storage and bucket initialization.
  Evidence: the seven-service Compose topology includes an HTTP gateway, built static web image,
  cloud image, private data services and a one-shot versioned-bucket initializer. The initial
  topology refused unreviewed image inputs under D-001; the selected PostgreSQL, Redis and
  SeaweedFS digests are now recorded in `deploy/saas/env.example`. `docker compose config`
  validates all service definitions. The Caddy gateway configuration and static web image build
  passed. S1-016 added persistence and health ordering; S1-018 verifies the routed transport probes.
- [x] **S1-015** Add `deploy/saas/env.example` containing no working production secret.
  Evidence: `deploy/saas/env.example` contains only loopback or reserved-host development values
  and mounted-secret paths; the adjacent ignore rules and README keep `.env` and `secrets/` out of
  Git and distinguish transport health from a healthy SaaS application.
- [x] **S1-016** Add container health checks, dependency ordering and named volumes.
  Evidence: Compose defines PostgreSQL, Redis and SeaweedFS named data volumes; the PostgreSQL 18
  mount target is `/var/lib/postgresql`. Web, gateway, PostgreSQL, Redis, SeaweedFS and cloud have
  health checks, with Redis auth read from a mounted secret on stdin. The cloud image's shell-free
  readiness probe has success/failure regression tests and returns success after the transport
  listener binds. PostgreSQL and Redis health plus completed SeaweedFS bucket-versioning
  initialization gate cloud creation; web health and cloud readiness gate the
  gateway. CI renders Compose with deliberately non-deployable image fixtures. The 2026-10-16
  [image review](dependency-images.md) records pinned OCI digests and a disposable Compose run:
  all three data services became healthy; PostgreSQL and Redis markers and the versioned bucket
  survived restarts; signed S3 `PutObject` returned a version ID and `HeadObject` found it after
  restart. This is data-service evidence, not an S1-018 healthy application smoke test.
- [x] **S1-017** Add cloud formatting, lint, unit and image-build CI without weakening `cargo xtask
  ci`.
  Evidence: the dedicated `cloud` CI job checks formatting, clippy and unit tests for
  `figcraft-cloud`, then builds `deploy/saas/cloud.Dockerfile`; the existing full Rust, WASM and
  web jobs remain unchanged. The same cloud checks and image build passed locally.
- [x] **S1-018** Add a Compose smoke test that reaches healthy state and verifies API, web and
  WebSocket upgrade routing from an empty environment.
  Evidence: `deploy/saas/smoke-preflight.sh` creates a disposable Compose project with random
  synthetic secrets, builds the images, waits for all services to become healthy, verifies bucket
  initialization, and fetches the web shell and API transport probe through Caddy. It also
  completes a standards-compliant WebSocket upgrade through the relay route, starts `all`, `api`,
  `relay` and `worker` from the identical cloud image, and verifies unsafe production URLs are
  rejected. The WebSocket probe sends a normal close frame; collaboration messages are future work.
  The project and its data are removed on exit. This validates S1 transport and startup, not
  authenticated or persistent application workflows.
- [x] **S1-019** Initialize `webapp/` with React, TypeScript, Vite, a pinned package-manager version
  and one committed lockfile. Depends on S0-012.
  Evidence: `webapp/package.json`, `webapp/package-lock.json`, `webapp/saas/`; verified with
  `npm ci --prefix webapp`, `npm run check --prefix webapp` and a browser screenshot.
- [x] **S1-020** Define the versioned TypeScript-to-Rust/WASM editor bridge contract without
  duplicating document or collaboration types.
  Evidence: `webapp/packages/editor-bridge`; Rust/WASM is the single WebSocket owner and binary
  document, selection and export payloads remain opaque to TypeScript.
- [x] **S1-021** Add TypeScript formatting, typecheck, lint, unit-test and production-build CI.
  Evidence: root `webapp` scripts run Prettier, strict TypeScript, ESLint, Vitest and both production
  builds; `.github/workflows/ci.yml` installs the committed lockfile under Node 22.18.0 and runs the
  same aggregate check.
- [x] **S1-022** Add browser dependency advisory, license and generated-client drift checks.
  Evidence: `webapp` runs high/critical `npm audit` checks in CI, validates every production
  dependency license from the lockfile against a permissive allowlist with GPL-family rejection,
  and reruns the API client generator to detect content drift once that package exists.

### S1 gate

- [x] **S1-GATE** Verify one image starts in every role, Compose starts cleanly, invalid
  configuration fails safely and all existing upstream gates pass.
  Evidence: the S1-018 disposable smoke starts all roles from one image, reaches healthy Compose
  state, checks API/web/WebSocket routes and rejects unsafe production settings. `cargo xtask ci`,
  web checks, shell lint and Compose validation passed.

## S2 — PostgreSQL metadata and events

### Migration and repository foundation

- [x] **S2-001** Select an async PostgreSQL client and migration approach compatible with the Rust
  toolchain and workspace policies. Depends on S1-GATE.
  Evidence: [`postgres-client-decision.md`](postgres-client-decision.md) selects SQLx 0.9 with
  PostgreSQL-only, Tokio and Rustls features; versioned SQL migrations will be embedded and run by
  an explicit command. SQLx is isolated to `figcraft-cloud-storage`; its declared MSRV is 1.94 and
  the cloud image compiles on the workspace's Rust 1.95 builder. S2-002 owns the actual migration
  command and lock tests.
- [x] **S2-002** Add explicit migration command and advisory migration lock.
  Evidence: `figcraft-cloud --migrate` applies embedded SQL through `figcraft-cloud-storage`, sets
  SQLx PostgreSQL advisory locking on, reads the mounted database secret, and emits no connection
  error details. The disposable Compose smoke checks normal startup leaves `_sqlx_migrations`
  absent and concurrent migration commands both complete and create the ledger.
- [ ] **S2-003** Add schema-version range checks to cloud startup and readiness.
- [ ] **S2-004** Create identity, tenant, workspace, project and document metadata migrations.
- [ ] **S2-005** Create document-event, snapshot-pointer, job, idempotency and audit migrations.
- [ ] **S2-006** Add constrained database roles for migration and runtime access.
- [ ] **S2-007** Add transaction helper that sets tenant context for row-level security.

### Metadata behavior

- [ ] **S2-008** Implement tenant, workspace and project repositories with bounded pagination.
- [ ] **S2-009** Implement document create, read, rename, archive, restore and soft-delete
  repositories.
- [ ] **S2-010** Implement external mapping repository with tenant-scoped uniqueness.
- [ ] **S2-011** Implement idempotency-key reservation, replay and mismatched-request rejection.
- [ ] **S2-012** Implement append-only audit repository for foundational actions.

### Collaboration event persistence

- [ ] **S2-013** Define encoded event storage and checksum format using existing collaboration
  messages.
- [ ] **S2-014** Implement atomic sequence allocation with unique `(tenant, document, seq)`.
- [ ] **S2-015** Implement append transaction that validates current fencing token.
- [ ] **S2-016** Implement bounded event replay from a requested sequence.
- [ ] **S2-017** Implement snapshot metadata registration without object bytes.
- [ ] **S2-018** Adapt cloud collaboration durability to commit before broadcast.

### Verification and migration input

- [ ] **S2-019** Add PostgreSQL concurrency tests for sequence, fencing and idempotency races.
- [ ] **S2-020** Add failure-injection tests for commit, connection loss and retry boundaries.
- [ ] **S2-021** Build a read-only scanner for the existing filesystem server data directory and a
  migration report format.

### S2 gate

- [ ] **S2-GATE** Create, edit, restart and restore a document using PostgreSQL metadata/events;
  prove a failed commit is never confirmed and replay reaches the accepted state.

## S3 — S3-compatible objects and migration

### Object repository

- [ ] **S3-001** Implement S3-compatible object client configuration and connectivity checks.
  Depends on S1-GATE and S2-GATE.
- [ ] **S3-002** Create object, reference and upload-reservation migrations.
- [ ] **S3-003** Implement tenant-scoped immutable object metadata and checksum verification.
- [ ] **S3-004** Implement staging upload reservations with method, key, size, type and expiry.
- [ ] **S3-005** Implement upload completion, read-back verification and promotion.
- [ ] **S3-006** Implement authorized, short-lived presigned downloads.
- [ ] **S3-007** Implement object-reference accounting and retention holds.

### Stored object types

- [ ] **S3-008** Store and restore immutable document snapshots.
- [ ] **S3-009** Store content-addressed image and binary assets.
- [ ] **S3-010** Store team-library versions and usage references.
- [ ] **S3-011** Store validated plugin bundles and manifests.
- [ ] **S3-012** Store export results and thumbnails.
- [ ] **S3-013** Add media, archive, image-pixel and expanded-byte validation budgets.

### Lifecycle and migration

- [ ] **S3-014** Add idempotent S3-compatible bucket, versioning and lifecycle initialization.
- [ ] **S3-015** Add garbage-collection jobs with dry-run and minimum-age protection.
- [ ] **S3-016** Add database-to-bucket reconciliation report.
- [ ] **S3-017** Extend the filesystem scanner into a dry-run migration planner.
- [ ] **S3-018** Implement repeatable filesystem-to-PostgreSQL/S3 migration with per-resource
  checkpoints.
- [ ] **S3-019** Verify migrated snapshots, logs, links, libraries and plugins before cutover.
- [ ] **S3-020** Add backup/restore integration test covering database and bucket versions.

### S3 gate

- [ ] **S3-GATE** Verify cloud resources require no local persistent container directory, corrupt
  objects cannot replace verified state, abandoned staging data expires and migration is repeatable.

## S4 — Identity, tenancy and authorization

### Authentication

- [ ] **S4-001** Implement OIDC discovery, key rotation and authorization-code flow with PKCE.
  Depends on S2-GATE.
- [ ] **S4-002** Implement secure browser sessions, rotation and logout.
- [ ] **S4-003** Implement single-tenant bootstrap administrator with forced bootstrap completion.
- [ ] **S4-004** Implement service accounts with scoped, expiring and rotatable credentials.
- [ ] **S4-005** Implement share-token generation, hashing, exchange, expiry and revocation.
- [ ] **S4-006** Add authentication rate limits and safe failure responses.

### Tenant and role model

- [ ] **S4-007** Implement tenant creation, suspension and plan assignment.
- [ ] **S4-008** Implement tenant membership and owner/admin roles.
- [ ] **S4-009** Implement teams, invitations, team membership, team-manager role and team ownership
  or grants for workspaces, projects and libraries.
- [ ] **S4-010** Implement direct workspace/document membership, inherited team grants and
  workspace-admin/editor/commenter/viewer roles.
- [ ] **S4-011** Implement explicit sensitive permissions for billing, provider keys, support,
  export and deletion.
- [ ] **S4-012** Implement effective-role resolution and deny-by-default authorization checks.
- [ ] **S4-013** Recheck authorization for long-running jobs before committing results.

### Isolation

- [ ] **S4-014** Enable row-level security on every tenant-owned table.
- [ ] **S4-015** Add tenant to every object, cache, lease and job key.
- [ ] **S4-016** Normalize forbidden and not-found behavior according to disclosure policy.
- [ ] **S4-017** Close active sessions after direct membership, team membership, team grant,
  share-link or tenant revocation.
- [ ] **S4-018** Add support-access grants with reason, expiry and audit.

### API and testing

- [ ] **S4-019** Implement tenant, team, invitation, workspace, project, document and member APIs.
- [ ] **S4-020** Implement bounded search and opaque pagination cursors.
- [ ] **S4-021** Add permission-matrix unit tests for every route and relay action.
- [ ] **S4-022** Add cross-tenant API, repository, object, cache, job and realtime tests.
- [ ] **S4-023** Add revocation race tests for active HTTP, job and WebSocket access.
- [ ] **S4-024** Define external identity, organization, domain and group-claim-to-team mapping
  policy.
- [ ] **S4-025** Validate enterprise SAML federation through the selected OIDC identity broker,
  including login, logout, key rotation and revocation.

### S4 gate

- [ ] **S4-GATE** Run single and multi-tenant modes against the same schema; pass complete role and
  cross-tenant suites and prove revocation terminates access.

## S5 — Distributed collaboration relay

### Admission and routing

- [ ] **S5-001** Implement short-lived, document-bound realtime tickets. Depends on S3-GATE and
  S4-GATE.
- [ ] **S5-002** Add relay admission that resolves authenticated principal, tenant and maximum role.
- [ ] **S5-003** Implement Redis room registry with bounded, namespaced keys.
- [ ] **S5-004** Implement renewable room leases and ownership identity.
- [ ] **S5-005** Implement monotonically increasing PostgreSQL fencing tokens.
- [ ] **S5-006** Implement gateway or client routing to current room owner.

### Room lifecycle

- [ ] **S5-007** Restore collaboration server from verified snapshot and subsequent events.
- [ ] **S5-008** Preserve client-id and reconnect state across room-owner failure.
- [ ] **S5-009** Implement commit-before-broadcast event processing.
- [ ] **S5-010** Reject writes immediately after lease or fencing loss.
- [ ] **S5-011** Implement idle room close and snapshot scheduling.
- [ ] **S5-012** Implement graceful drain and client reconnect signal for rolling releases.
- [ ] **S5-013** Implement quarantine when no verified snapshot/event chain can restore a room.

### Roles, presence and large data

- [ ] **S5-014** Enforce viewer, commenter and editor capabilities at relay message handling.
- [ ] **S5-015** Disconnect sessions whose grant is revoked.
- [ ] **S5-016** Keep cursor, selection, chat and spotlight ephemeral and separately rate-limited.
- [ ] **S5-017** Add bounded room, connection, message, rate and outbound-byte quotas.
- [ ] **S5-018** Move large image transfer to verified asset references and negotiated capability.
- [ ] **S5-019** Preserve compatibility with current filesystem-server and desktop-hosted sessions.

### Failure and performance verification

- [ ] **S5-020** Test two relay replicas owning different rooms.
- [ ] **S5-021** Test simultaneous lease acquisition and stale-owner fencing.
- [ ] **S5-022** Kill relay before commit and prove no edit was confirmed.
- [ ] **S5-023** Kill relay after commit and prove the edit returns after reconnect.
- [ ] **S5-024** Load-test reconnect storms, slow readers and maximum supported document size.
- [ ] **S5-025** Measure room recovery and commit latency against roadmap targets.

### S5 gate

- [ ] **S5-GATE** Pass fenced multi-replica, crash, revocation, compatibility and load tests with no
  acknowledged-edit loss.

## S6 — Browser, desktop and embedded integration

### Host registration and tokens

- [ ] **S6-001** Implement registered host systems, issuers, keys, audiences and allowed origins.
  Depends on S4-GATE and S5-GATE.
- [ ] **S6-002** Implement external tenant, workspace, principal and resource mappings.
- [ ] **S6-003** Implement short-lived embed-token validation and one-use `jti` consumption.
- [ ] **S6-004** Enforce host maximum role and mapped resource boundary.
- [ ] **S6-005** Add key rotation and issuer-disable behavior.

### Embed behavior

- [ ] **S6-006** Implement URL-fragment or constrained `postMessage` token handoff without referrer
  leakage.
- [ ] **S6-007** Implement exact parent-origin validation.
- [ ] **S6-008** Define and implement ready, dirty, save, export, close and fatal-state messages.
- [ ] **S6-009** Configure CSP, frame ancestors, cookies and CORS per registered host.
- [ ] **S6-010** Close embedded sessions when the host mapping or membership is revoked.

### API and webhooks

- [ ] **S6-011** Publish versioned OpenAPI for host document and job operations.
- [ ] **S6-012** Implement idempotent host create, resolve, archive, restore and export flows.
- [ ] **S6-013** Implement webhook endpoint management and encrypted signing secrets.
- [ ] **S6-014** Implement signed delivery, retry schedule, terminal failure and manual replay.
- [ ] **S6-015** Implement webhook event filtering and payload minimization.

### Verification and example

- [ ] **S6-016** Build a minimal host integration example outside upstream-owned app code.
- [ ] **S6-017** Add issuer, audience, replay, role-escalation and origin attack tests.
- [ ] **S6-018** Add webhook signature, duplicate, timeout and retry tests.
- [ ] **S6-019** Add compatibility tests for the previous supported API version.

### Browser workspace

- [ ] **S6-020** Build React/TypeScript navigation for recents, projects, libraries, search,
  notifications and settings inside `webapp/`.
- [ ] **S6-021** Implement stable `/workspace/{workspace_id}/file/{document_id}` routing and deep
  links.
- [ ] **S6-022** Implement the typed editor mount, open, close, local-change, remote-change,
  connection-state, selection and export bridge.
- [ ] **S6-023** Keep snapshots and collaboration batches opaque to TypeScript and add tests that
  prevent browser-shell document semantics.
- [ ] **S6-024** Open, edit, reconnect and export one cloud document without manual file import.
- [ ] **S6-025** Complete WCAG 2.2 AA checks for workspace surfaces and keyboard/screen-reader audit
  of the integrated editor.

### Desktop cloud documents

- [ ] **S6-026** Add a narrow local/cloud document repository adapter at existing session seams.
- [ ] **S6-027** Add authenticated cloud workspace and document browsing to the native app.
- [ ] **S6-028** Display saved, saving, offline, conflict and error states for cloud documents.
- [ ] **S6-029** Persist a checksummed offline operation journal before reporting edits safely
  queued.
- [ ] **S6-030** Reconcile stable operation identifiers on reconnect and preserve unsafe conflicts
  as recoverable branches.
- [ ] **S6-031** Add browser/desktop identity, convergence, offline and conflict end-to-end tests.

### S6 gate

- [ ] **S6-GATE** Demonstrate browser and desktop access to one cloud document identity, recoverable
  offline work, and a host-created embedded edit/export/webhook flow without exposing credentials.

## S7 — AI gateway and server WASM

### AI provider layer

- [ ] **S7-001** Define normalized text, structured, vision, image and embedding operations. Depends
  on S3-GATE, S4-GATE and S5-GATE.
- [ ] **S7-002** Implement deterministic fake provider for unit and integration tests.
- [ ] **S7-003** Implement one external provider adapter behind feature flags and secret isolation.
- [ ] **S7-004** Implement OpenAI-compatible private/local endpoint adapter.
- [ ] **S7-005** Implement tenant provider allowlist, model aliases and fallback policy.
- [ ] **S7-006** Encrypt BYOK configuration and restrict it to AI workers.

### AI jobs and document application

- [ ] **S7-007** Implement AI job submission, cancellation, attempts and terminal outcomes.
- [ ] **S7-008** Implement quota reservation and actual-usage settlement.
- [ ] **S7-009** Implement selection-bounded document context builder.
- [ ] **S7-010** Implement prompt/response retention modes and redacted diagnostics.
- [ ] **S7-011** Implement structured command-result schema and size limits.
- [ ] **S7-012** Validate commands, parameters, referenced nodes and permitted command identifiers.
- [ ] **S7-013** Execute proposals through the engine guard against the requested sequence.
- [ ] **S7-014** Submit accepted batches through the authoritative relay as an AI service principal.
- [ ] **S7-015** Add visible attribution, one undo step and audit/usage linkage.
- [ ] **S7-016** Implement generated-image staging, validation, moderation and object promotion.

### Server-side WASM

- [ ] **S7-017** Define server module manifest, API version and immutable approval record.
- [ ] **S7-018** Create dedicated WASM worker role and restricted network policy.
- [ ] **S7-019** Enforce bundle, memory, fuel, deadline, call, log and byte limits.
- [ ] **S7-020** Implement document inspection and command capabilities.
- [ ] **S7-021** Implement verified asset read/write capabilities.
- [ ] **S7-022** Implement AI request, progress and bounded module-settings capabilities.
- [ ] **S7-023** Submit successful module mutations as one command batch and discard failed output.
- [ ] **S7-024** Implement module approval, disable and version selection APIs.

### Verification

- [ ] **S7-025** Add prompt-injection, malformed structured output and unauthorized reference tests.
- [ ] **S7-026** Add provider timeout, retry, cancellation, cost and secret-leak tests.
- [ ] **S7-027** Add WASM escape, exhaustion, capability-escalation and worker-termination tests.

### S7 gate

- [ ] **S7-GATE** Prove AI and WASM cannot bypass tenancy or collaboration, failures leave documents
  unchanged, generated assets remain quarantined until verified and usage stays correct under retry.

## S8 — Operations, quotas, billing and security

### Usage, plans and billing

- [ ] **S8-001** Define plan features and limits for storage, members, rooms, exports, AI and WASM.
  Depends on S4-GATE through S7-GATE.
- [ ] **S8-002** Implement transactional usage counters and periodic reconciliation.
- [ ] **S8-003** Enforce storage and concurrency quotas before resource allocation.
- [ ] **S8-004** Implement billing-provider abstraction and one provider adapter.
- [ ] **S8-005** Handle payment failure, grace, suspension and recovery without deleting data.
- [ ] **S8-006** Add tenant usage and operator capacity views.

### Observability

- [ ] **S8-007** Emit the required API, relay, database, Redis, object, job, webhook, AI and WASM
  metrics.
- [ ] **S8-008** Add OpenTelemetry tracing across requests, events, jobs and providers.
- [ ] **S8-009** Add structured log redaction tests.
- [ ] **S8-010** Create dashboards for service health, durability, room load, queues and tenant usage.
- [ ] **S8-011** Create actionable alerts with runbook links and tested thresholds.

### Backup, recovery and releases

- [ ] **S8-012** Automate PostgreSQL point-in-time backup policy.
- [ ] **S8-013** Configure object versioning, replication or backup and inventory reconciliation.
- [ ] **S8-014** Write and execute full restore procedure in an isolated environment.
- [ ] **S8-015** Verify declared recovery point and recovery time objectives.
- [ ] **S8-016** Implement relay drain, worker lease return and graceful role shutdown.
- [ ] **S8-017** Test rolling upgrade, schema compatibility and supported rollback.

### Security and supply chain

- [ ] **S8-018** Complete every control and test in `security.md`.
- [ ] **S8-019** Add dependency advisory, license, secret and container scanning.
- [ ] **S8-020** Generate software bill of materials for release images.
- [ ] **S8-021** Sign images and publish build provenance.
- [ ] **S8-022** Commission and remediate an external multi-tenant penetration test.
- [ ] **S8-023** Test operator support grants, tenant export and tenant deletion.

### Deployment and runbooks

- [ ] **S8-024** Add production-shaped role-separated Compose profile.
- [ ] **S8-025** Add Kubernetes or Helm deployment after Compose contracts stabilize.
- [ ] **S8-026** Apply network policies and external secret integration.
- [ ] **S8-027** Write every runbook listed in `deployment-operations.md`.
- [ ] **S8-028** Exercise database, Redis, object-store, relay, provider and migration incident runbooks.
- [ ] **S8-029** Load and soak test weighted tenant scheduling and quota enforcement.
- [ ] **S8-030** Verify upstream synchronization and emergency-security process on a release branch.
- [ ] **S8-031** Implement SCIM 2.0 user and group provisioning with idempotent deprovisioning.
- [ ] **S8-032** Add enterprise identity lifecycle, audit export and provisioning security tests.
- [ ] **S8-033** Add and verify a Docker Swarm stack when a supported managed deployment requires
  it; document node-loss, update, rollback and secret behavior.

### S8 gate

- [ ] **S8-GATE** Pass security, restore, quota, observability, rolling-release and supply-chain
  gates using the release images.

## S9 — Production rollout

### Staging and design partners

- [ ] **S9-001** Create production-equivalent staging with isolated credentials and data.
- [ ] **S9-002** Run synthetic create, edit, reconnect, export, AI-disabled and AI-enabled journeys
  continuously.
- [ ] **S9-003** Onboard internal tenants and record usability and operations findings.
- [ ] **S9-004** Onboard a limited design-partner tenant under documented limits.
- [ ] **S9-005** Onboard one embedded host integration and exercise revocation and webhooks.
- [ ] **S9-006** Resolve all critical and high launch defects with regression tests.

### Service operation

- [ ] **S9-007** Publish service limits, status page, support routes and incident communication.
- [ ] **S9-008** Publish privacy, retention, subprocessors and acceptable-use behavior matching the
  implementation.
- [ ] **S9-009** Define SLOs and error-budget policy from real telemetry.
- [ ] **S9-010** Validate capacity model and autoscaling thresholds.
- [ ] **S9-011** Establish on-call ownership and escalation.
- [ ] **S9-012** Run a game day covering relay loss, database failover and object unavailability.

### Release and traceability

- [ ] **S9-013** Exercise release, rollback and emergency upstream-security procedures.
- [ ] **S9-014** Record fork release, source commit, upstream commit, schema and protocol versions.
- [ ] **S9-015** Verify tenant export and tested deletion before general availability.
- [ ] **S9-016** Complete general-availability architecture review.
- [ ] **S9-017** Complete general-availability security review.
- [ ] **S9-018** Complete general-availability operations review.

### S9 gate

- [ ] **S9-GATE** Approve production only after design-partner journeys, runbooks, SLO measurement,
  traceability and all three general-availability reviews pass.

## Cross-cutting recurring tasks

These tasks repeat for every implementation pull request:

- [ ] **X-001** Add or update tests proportional to the behavior and failure risk.
- [ ] **X-002** Run affected upstream checks and SaaS checks.
- [ ] **X-003** Update this tracker with evidence.
- [ ] **X-004** Update architecture decisions when behavior differs from the documented decision.
- [ ] **X-005** Update the fork patch ledger for changes to upstream-owned files.
- [ ] **X-006** Check tenant scoping for every new row, object, cache key, job and metric.
- [ ] **X-007** Check secret and document-content redaction.
- [ ] **X-008** Check failure behavior before and after durable commit.
- [ ] **X-009** Check compatibility with filesystem self-hosting and offline editing.
- [ ] **X-010** Use a commit subject containing the milestone and task ID.

Recurring tasks remain unchecked because they are obligations rather than one-time completion
items.

## Blockers and decisions awaiting owners

| ID | Decision | Blocks |
|---|---|---|
| D-002 | Select HTTP framework and async PostgreSQL client after a compatibility spike | S2 implementation |
| D-003 | Select initial OIDC provider used for end-to-end testing | S4 acceptance journey |
| D-004 | Select first external AI provider and whether BYOK ships in the first AI beta | S7 external adapter |
| D-005 | Select billing provider or defer paid signup while retaining plan enforcement | S8 billing adapter |
| D-006 | Select initial hosted region and recovery objectives | S8 backup and S9 capacity |
| D-007 | Name architecture, security and operations reviewers | S0-GATE and production reviews |

These decisions do not block documentation or infrastructure-neutral domain work. They must be
resolved before the tasks named in the third column pass their gates.

Resolved decision: **D-001** selects digest-pinned PostgreSQL 18.6, BSD-licensed Redis 7.2.16 and
Apache-2.0 SeaweedFS 4.48 for the local stack; the same SeaweedFS image runs the one-shot
bucket-versioning initializer. [Review and runtime evidence](dependency-images.md). This does
not approve the single-node topology for production or complete the S1-018 application smoke test.

## Release evidence index

Add one row per preview, beta, release candidate and production release.

| Release | Source commit | Upstream commit | Schema | Protocol | Evidence |
|---|---|---|---|---|---|
| Documentation baseline | current working tree | `9dcfe4ecbcf2892702b2db72a05b3511a58a0b7d` | not created | existing upstream protocol | `docs/saas/` |

## Revision history

| Date | Change |
|---|---|
| 2026-10-16 | S1-017: verified cloud quality and image-build CI; S1 advanced to 19/22. |
| 2026-10-16 | S1-014: added a guarded seven-service Compose topology and static web image; S1 advanced to 20/22. |
| 2026-10-16 | S1-016: added named data volumes, health probes and fail-closed startup ordering; S1 advanced to 21/22. |
| 2026-10-16 | D-001: pinned and inspected supported PostgreSQL and Redis image digests and PostgreSQL volume target; documented archived MinIO blocker. S1 stays 21/22. |
| 2026-10-16 | D-001: replaced archived MinIO with pinned SeaweedFS 4.48; validated data health, versioning, signed S3 writes and restart persistence in a disposable Compose project. S1 stays 21/22. |
| 2026-10-16 | S1-018 preparation: added a disposable fail-closed Compose preflight; API/web/WebSocket acceptance remains pending. S1 stays 21/22. |
| 2026-10-16 | S1-018 and S1-GATE: implemented diagnostic API/WebSocket transport probes and validated healthy Compose routing, all roles and unsafe-config rejection. S1 complete at 22/22. |
| 2026-10-16 | S2-001: selected SQLx 0.9 with embedded SQL migrations and PostgreSQL advisory locking; SQLx declares Rust 1.94 MSRV and the image built with workspace Rust 1.95. |
| 2026-10-16 | S2-002: added the explicit `--migrate` release command with SQLx PostgreSQL advisory locking; disposable Compose concurrency verification recorded. |

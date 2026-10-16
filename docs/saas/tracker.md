# SaaS implementation tracker

> **Status:** S1 complete (22/22); S2 complete (21/21); S3 in progress (17/20) · **Last updated:** 2026-10-17

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
| S2 PostgreSQL metadata and events | complete | 21 / 21 | Durable transactional event path |
| S3 Object storage and migration | in progress | 17 / 20 | Verified object storage and repeatable migration |
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
  server reports the selected role; readiness now requires both a bound listener and the schema
  range check added in S2-003. Product services remain explicit future work.
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
  Evidence: the eight-service Compose topology includes an HTTP gateway, built static web image,
  cloud image, private data services, a one-shot migration job and a versioned-bucket initializer.
  The original S1 topology refused unreviewed image inputs under D-001; the selected PostgreSQL, Redis and
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
  listener binds and the schema range is supported (S2-003). PostgreSQL and Redis health plus
  completed SeaweedFS bucket-versioning
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
  the cloud image compiles on the workspace's Rust 1.95 builder. S2-002 owns the explicit migration
  command and lock tests; S2-003 adds startup range validation.
- [x] **S2-002** Add explicit migration command and advisory migration lock.
  Evidence: `figcraft-cloud --migrate` applies embedded SQL through `figcraft-cloud-storage`, sets
  SQLx PostgreSQL advisory locking on, reads the mounted database secret, and emits no connection
  error details. Before S2-003 made the Compose migration job an explicit cloud dependency, the
  disposable smoke verified ordinary service startup did not create `_sqlx_migrations`; concurrent
  explicit migration commands both completed successfully.
- [x] **S2-003** Add schema-version range checks to cloud startup and readiness.
  Evidence: `figcraft-cloud-storage::check_schema_version` reads the highest successful SQLx
  migration, rejects missing ledgers and out-of-range versions with safe error categories, and
  accepts the empty baseline as version 0. Cloud readiness checks this range at startup. Compose
  now runs the `migrate` service after PostgreSQL health and gates cloud startup on its successful
  completion. Unit tests cover inclusive range boundaries; the disposable Compose smoke confirms
  the migrated baseline permits cloud and gateway readiness.
- [x] **S2-004** Create identity, tenant, workspace, project and document metadata migrations.
  Evidence: `20261016000001_initial_metadata.sql` creates 13 tables for principals, identity
  links, tenants, membership and teams, workspace/project/document metadata, and direct/team
  grants. Composite foreign keys enforce tenant and parent scope; checks bound names, states,
  roles and sequence values. The disposable PostgreSQL smoke applies the embedded migration,
  verifies its version and rejects synthetic cross-tenant and invalid-row inserts in a rolled-back
  transaction (`deploy/saas/schema-preflight.sql`). Cloud readiness advanced to this version at
  S2-004; S2-005 advances it again.
- [x] **S2-005** Create document-event, snapshot-pointer, job, idempotency and audit migrations.
  Evidence: `20261016000002_events_jobs_audit.sql` adds ordered document events, snapshot
  metadata and a same-document active pointer, plus tenant-scoped jobs, idempotency keys and audit
  events. The PostgreSQL smoke checks valid records and rejects duplicate sequences, cross-tenant
  references, wrong-document snapshot pointers, malformed checksums and invalid operational rows
  in a rolled-back synthetic transaction. Cloud readiness now requires schema version
  `20261016000002`. Event commit, lease, replay and repository behavior remain later S2 tasks.
- [x] **S2-006** Add constrained database roles for migration and runtime access.
  Evidence: `figcraft-cloud --provision-db-roles` creates and rotates separate non-superuser
  `figcraft_migrator` and `figcraft_runtime` login roles from mounted secrets. Compose provisions
  before migrations, migrates as the schema-owning migrator, reconciles grants afterward and starts
  cloud with only the runtime URL. Runtime gets DML on application metadata, select/insert on
  event and audit tables, and read-only access to `_sqlx_migrations`; it cannot create or alter
  schema objects. The disposable PostgreSQL role preflight exercises both roles, denied DDL and
  reconciliation of deliberately stale runtime grants.
- [x] **S2-007** Add transaction helper that sets tenant context for row-level security.
  Evidence: `figcraft-cloud-storage::begin_tenant_transaction` accepts a typed `TenantId`, begins
  a SQLx transaction and binds `figcraft.tenant_id` as a transaction-local setting. Its PostgreSQL
  integration probe uses a synthetic RLS table to verify same-connection tenant isolation and
  cleanup after commit, rollback and drop. Authentication and production RLS policies remain later tasks.

### Metadata behavior

- [x] **S2-008** Implement tenant, workspace and project repositories with bounded pagination.
  Evidence: `figcraft-cloud-storage::metadata` provides typed tenant, workspace and project
  create/get operations, membership-scoped tenant discovery, and workspace/project keyset lists
  with a strict 1–100 row limit. `TenantTransaction` keeps the SQL tenant predicate and database
  context together. The disposable PostgreSQL integration probe covers two tenants, suspended
  membership, foreign-key rejection, cross-tenant reads and lists, and multiple pages over 100
  rows under the limited runtime role. Product authorization and HTTP cursors remain S4 work.
- [x] **S2-009** Implement document create, read, rename, archive, restore and soft-delete
  repositories.
  Evidence: `figcraft-cloud-storage::metadata` adds a typed `DocumentRecord` with tenant-scoped
  create/get, and rename/archive/restore/soft-delete transitions that are idempotent when the
  document already satisfies the target state and reject other states as `InvalidState` rather
  than a database error. `20261016000003_document_lifecycle.sql` adds a `deleted_at` grace-period
  marker constrained to the `deleting` state only; finalizing a grace period into `deleted` and
  scheduling object collection remain later work. The cloud binary's supported schema range
  advanced to this version. The disposable PostgreSQL integration probe drives every transition
  (including idempotent repeats, rejected transitions from a terminal `deleted` fixture row, missing
  documents resolving to `None`, and a wrong-parent foreign-key rejection in its own transaction)
  and confirms cross-tenant reads see nothing. The full S1/S2 Compose smoke passed end-to-end,
  which also surfaced and fixed two pre-existing defects unrelated to this task's scope: the
  disposable secrets `smoke-preflight.sh` wrote under `umask 077` were unreadable by the cloud
  image's distroless nonroot container user (Compose bind-mounts `secrets.*.file` preserving host
  permissions outside Swarm), and the cloud binary's hardcoded schema-version constants had not
  been updated past S2-005.
- [x] **S2-010** Implement external mapping repository with tenant-scoped uniqueness.
  Evidence: `figcraft-cloud-storage::external_mappings` adds a typed `ExternalMappingRecord` with
  tenant-scoped create/get over a new `external_mappings` table, unique by `(tenant_id, system_id,
  resource_kind, external_id)`. `20261016000004_external_mappings.sql` adds the table with a
  bounded `resource_kind` enum and no single foreign key on `internal_id`, since it may point at a
  tenant, workspace, project, document or principal depending on kind. The cloud binary's
  supported schema range advanced to this version. The disposable PostgreSQL integration probe
  covers tenant-scoped creation, a duplicate-natural-key `Conflict` rejected in its own rolled-back
  transaction, and cross-tenant invisibility of a committed mapping.
- [x] **S2-011** Implement idempotency-key reservation, replay and mismatched-request rejection.
  Evidence: `figcraft-cloud-storage::idempotency` adds `reserve_idempotency_key`
  (`INSERT ... ON CONFLICT (tenant_id, principal_id, route, key_hash) DO NOTHING RETURNING ...`
  followed by a conditional lookup that distinguishes a fresh `Reserved` key from a same-request
  `Replay` of a completed response, a same-request `InFlight` pending duplicate, or a
  different-request `Err(Conflict)`) and `complete_idempotency_key` (`pending -> completed` via a
  conditional update, `Err(InvalidState)` when the key is missing or already completed). No new
  migration was needed; `idempotency_keys` already existed from S2-005. The disposable PostgreSQL
  integration probe exercises all five outcomes.
- [x] **S2-012** Implement append-only audit repository for foundational actions.
  Evidence: `figcraft-cloud-storage::audit` adds `record_audit_event`, a single write-only insert
  into `audit_events` returning the caller-supplied id, with an optional actor for system/job
  actions, `outcome` left to the database `CHECK` constraint rather than re-validated in Rust, and
  `metadata` encoded to JSON text and cast `::jsonb` to avoid enabling SQLx's `json` feature
  workspace-wide. A list/export function is deliberately out of scope, since it needs its own
  compound `(created_at, id)` keyset cursor design. No new migration was needed. The disposable
  PostgreSQL integration probe covers an actored event, a system event with no actor, a paired
  `resource_id`/`request_id` event, and an invalid-outcome insert rejected as `Constraint` in its
  own isolated transaction.

### Collaboration event persistence

- [x] **S2-013** Define encoded event storage and checksum format using existing collaboration
  messages.
  Evidence: `figcraft-cloud-storage::events` stores each accepted batch as its existing
  collaboration wire form (`figcraft_collab::message::encode` of `Accepted`), the same value the
  filesystem server journals as `Journal::Accepted`, so no second document format exists. The
  32-byte checksum is SHA-256 over a versioned domain tag, the tenant and document UUID bytes, the
  big-endian sequence, the payload length and the payload, so intact bytes copied to another row,
  document or tenant fail verification. `decode_event` checks size and checksum before decoding
  and rejects a payload whose inner sequence differs from its row; `encode_event` refuses sequences
  outside `1..=i64::MAX` and payloads over the table's 16 MiB limit. Unit tests pin the checksum
  frame byte for byte and cover round trip, relocation, bit flips, truncated checksums, sequence
  bounds, oversize payloads, checksummed garbage and a mutation sweep. The format is documented in
  `data-tenancy-integration.md`. Client-id issue and refusals stay session state, not events. No
  migration was needed.
- [x] **S2-014** Implement atomic sequence allocation with unique `(tenant, document, seq)`.
  Evidence: `figcraft-cloud-storage::events::append_events` locks the tenant's document row
  `FOR UPDATE`, requires the batches to continue the committed sequence exactly
  (`current_seq + 1`, `+ 2`, ...), inserts them in one `UNNEST` statement under the existing
  `(tenant_id, document_id, seq)` primary key and advances `documents.current_seq` in the same
  transaction. Gaps, repeats, rewinds and reordering return `SequenceConflict { current }` before
  anything is written; empty or oversized appends (over 1,024 batches) and unencodable batches are
  `InvalidInput`; a non-`active` document is `InvalidState`; another tenant's document is
  `NotFound`. `20261016000005_event_sequencing.sql` adds a trigger refusing any decrease of
  `current_seq` or `fencing_token` whatever path updates the row, and advances readiness to that
  version. The disposable PostgreSQL integration probe covers consecutive multi-batch appends,
  every conflict shape, archived and missing documents, cross-tenant invisibility and both trigger
  rewinds under the limited runtime role.
- [x] **S2-015** Implement append transaction that validates current fencing token.
  Evidence: `figcraft-cloud-storage::events::claim_sequencer` locks an `active` document and
  advances its fencing token, returning the token and the committed sequence to resume from;
  archived and missing documents are refused without advancing it. `append_events` reads the
  token under the same `FOR UPDATE` lock as the sequence and refuses any token that is not the
  current one (superseded, never issued, or 0) as `StaleFence` before the sequence check, and each
  event row records the token that wrote it. The S2-014 trigger keeps the token monotonic. The
  disposable PostgreSQL probe covers unclaimed tokens, an in-transaction takeover, a not-yet-issued
  token, per-row token provenance, refused claims, and an old owner's late append after a takeover
  committed in another transaction. The distributed lease that decides who may claim remains S5.
- [x] **S2-016** Implement bounded event replay from a requested sequence.
  Evidence: `figcraft_cloud_storage::events::read_events` returns a `ReplayPage` of committed
  batches with `seq > after_seq`, each decoded with `decode_event` (checksum checked before
  decoding) and returned with its seq, fencing token and actor. The page holds at most
  `MAX_REPLAY_EVENTS` (1000, clamped) events and a byte budget (clamped to `MAX_REPLAY_BYTES`). The
  budget comes from a SQL window sum of `octet_length(payload)`, so payloads past the cut are never
  fetched, and the next event is always returned so replay keeps moving. Reads stop at
  `documents.current_seq`. A gap, a bad checksum or payload, or a missing row below `current_seq` is
  `Corrupt`. A negative `after_seq` is `InvalidInput`. An `after_seq` past `current_seq` is
  `SequenceConflict { current }`. A document the tenant cannot see is `NotFound`.
  `next_after_seq`/`has_more` drive the caller's loop. The `event_replay` probe commits a chain
  through `claim_sequencer` + `append_events` and replays it four ways: from 0, from the middle,
  from current (empty), and as cross-tenant `NotFound`. It also pages with a count cap of 2 and with
  0-byte, 1-byte and exact-two-payload budgets. Inside rolled-back transactions it plants damaged
  rows (wrong checksum, checksummed garbage, a payload moved from another document, a gap, a missing
  row, a bad row after a good one) and checks each is `Corrupt`.
- [x] **S2-017** Implement snapshot metadata registration without object bytes.
  Evidence: `figcraft-cloud-storage::snapshots` adds `register_snapshot` (inserts a `'pending'`
  row referencing `object_id`/`checksum`/`size_bytes` only — the bytes themselves are later S3
  work), `verify_snapshot`/`reject_snapshot` (idempotent `'pending' -> 'verified'/'rejected'`
  transitions following the `Ok(Some)/Err(InvalidState)/Ok(None)` pattern established by S2-009),
  and `set_current_snapshot` (relies on `documents.current_snapshot_id`'s existing composite
  foreign key to reject cross-document pointers). No new migration was needed; `document_snapshots`
  and `documents.current_snapshot_id` already existed from S2-005. The disposable PostgreSQL
  integration probe covers registration, both idempotent transitions, the invalid-state rejection,
  an unknown-document constraint rejection, and the cross-document pointer rejection, each isolated
  from the main rolled-back transaction where it would otherwise abort unrelated assertions.
- [x] **S2-018** Adapt cloud collaboration durability to commit before broadcast.
  Evidence: `figcraft-cloud-relay::DurableRoom` wraps the collaboration `Server`: `receive` takes
  the server's journal, appends its accepted batches with `append_events` under the room's fencing
  token and commits, and only then returns the confirmations and broadcasts to deliver. Any append
  or commit failure drops them and poisons the room, which refuses every later message and join so
  the transport closes it and clients reconcile from durable state, matching the filesystem
  server's flush-before-answers rule without its transport. `DurableRoom::open` commits a
  `claim_sequencer` takeover first, fencing every earlier owner, then replays the committed log page
  by page with `read_events` over the supplied base document and refuses (`Replay`) a log that does
  not apply rather than serving uncertain state; snapshot restore arrives with S3. Joins and
  refusals stay session state. `xtask layers` gains one documented same-layer edge,
  `cloud-relay -> cloud-storage`. The PostgreSQL probe proves an accepted batch is committed before
  its broadcast is returned, a refused batch writes nothing, a takeover fences the old room (nothing
  written or returned, then `Poisoned`), a reopened room replays exactly the confirmed state and
  keeps accepting edits, and a mismatched base is refused.

### Verification and migration input

- [x] **S2-019** Add PostgreSQL concurrency tests for sequence, fencing and idempotency races.
  Evidence: `crates/cloud-storage/tests/event_concurrency.rs` races real PostgreSQL connections on a
  4-worker multi-thread runtime over committed fixture data: a gate transaction holds the contended
  row or insert lock and the test waits on `pg_stat_activity` until every racer is blocked before
  releasing it, so no sleep stands in for synchronisation and every wait is bounded. Eight
  same-sequence appenders commit exactly once (the rest get `SequenceConflict`), and four retrying
  writers build an exact gap-free `1..=current_seq` log. A takeover orders strictly against an
  in-flight append (it lands first, or a later append is fenced with `StaleFence`, and a rolled-back
  takeover fences nobody). A free-running owner writes exactly up to the seq the takeover claimed
  at, and every row carries the token current at its commit. Concurrent reservations of one key give
  one `Reserved` with the rest `InFlight`, or `Conflict` for different requests, with no
  unique-violation leak; a rolled-back reservation passes the key to exactly one waiter; racing
  completions store one response, which every retry replays. A removed `FOR UPDATE` is caught by the
  probe; the tests read their starting seq/token so they rerun safely on the same fixture.
- [x] **S2-020** Add failure-injection tests for commit, connection loss and retry boundaries.
  Evidence: Real PostgreSQL, runtime role. Faults are injected by pg_terminate_backend from a second
  runtime connection, with the victim found by pg_backend_pid or by a per-run application_name, or
  by the victim's own SET LOCAL lock_timeout / statement_timeout. Every wait polls pg_stat_activity
  against a 30s deadline; no sleep is used for synchronisation.
  crates/cloud-storage/tests/failure_injection.rs: (1) kill after append_events and before COMMIT:
  commit returns Err, current_seq/fencing_token/rows are unchanged, the killed backend holds 0
  pg_locks, the same batch retried on a fresh connection lands exactly once (read_events returns the
  identical ReplayedEvent {seq, token, actor, batch}), and a second retry is
  SequenceConflict{current: seq}. (2) kill while queued on the document row FOR UPDATE behind a
  gate, with a second writer queued behind the victim: the victim gets Err(Database), nothing is
  written, the writer is still blocked until the gate rolls back, then commits seq+1 and the log
  stays exact. (3) lost acknowledgement: committed, retry refused with SequenceConflict{current ==
  the batch's own seq}; read_events returns exactly the sent batch (a run-unique batch_id, so a
  rival batch would not match); rows == current_seq, no duplicate. (4) lock_timeout=50ms on append
  and statement_timeout=50ms on claim_sequencer both give Err(Database); the aborted transaction
  refuses further statements; a COMMIT sent anyway is a rollback on the server; seq and token do not
  move, no lock is left, and the owner's append goes through after the gate. (5) idempotency: a
  reservation killed before commit, or rolled back, leaves the key free (Reserved again); a
  committed reservation whose completing transaction is killed stays pending, so retries get
  InFlight; the committed completion is replayed byte for byte, completing again is InvalidState,
  and a different request hash is Conflict. crates/cloud-relay/tests/room_failure_injection.rs: (6)
  DurableRoom on a one-connection pool tagged with application_name. An idle pooled connection
  killed between receives is replaced transparently (the room is not poisoned and the batch
  commits). A connection killed while the room's append waits on the gated row lock gives
  Err(Append(Database)): no Outgoing, the room is poisoned, server().seq() is one ahead of durable,
  the next receive is Poisoned, nothing is written. DurableRoom::open on the same pool replays only
  committed batches (the frame name is the last committed one, the token has advanced, the last
  event is the committed batch_id). Fixture: prefix 98,
  deploy/saas/failure-injection-{preflight,cleanup}.sql, rerun-safe (tests read their starting
  seq/token from the database). Ran 2 reruns plus 1 fresh run, all green; fmt and clippy -D warnings
  clean.
- [x] **S2-021** Build a read-only scanner for the existing filesystem server data directory and a
  migration report format.
  Evidence: `figcraft-cloud-storage::legacy_scan::scan_data_directory` inventories a stopped
  `figcraft-server` data directory without changing it: no writes, renames, locks or deletes, no
  symlink following, only the fixed layout descended, and every read, listing, log record and
  problem capped by `ScanLimits`. It reimplements just the `FCSN` snapshot and `FCLG` journal
  framing (CRC-32, wire-encoded `SessionState`/`Journal`), with no dependency on the server app.
  Logs are streamed one record at a time; each record is counted by kind and checked against the
  server's replay rules (already applied, replayable, sequence break), and torn, damaged,
  undecodable and stale-generation logs are classified. It also reads meta, links, libraries, usage,
  root lists and plugin bundles (by size and SHA-256), but never copies secrets, token hashes or
  addresses into the report. The versioned, serde `MigrationReport` (format 1) carries totals,
  per-document ready/degraded/blocked status, `last_accepted_seq` and stable snake_case problem
  codes, each with a fixed severity. `filesystem-migration-report.md` documents every field, what
  blocks migration, the parts it leaves unparsed (library archives, replay of the changes
  themselves) and how the S3 import will consume the report. Unit tests include a never-crash sweep
  over every truncation, byte flip and random garbage. The tempdir probe covers a healthy directory,
  damage classification, an unchanged tree after scanning, symlink escapes, plugin hash mismatches,
  caps and bad roots. No database or migration was needed.

### S2 gate

- [x] **S2-GATE** Create, edit, restart and restore a document using PostgreSQL metadata/events;
  prove a failed commit is never confirmed and replay reaches the accepted state.
  Evidence: `crates/cloud-relay/tests/s2_gate.rs` (`create_edit_restart_and_restore_a_document`) is
  one end-to-end probe that uses only public APIs. It runs as figcraft_runtime against the fixture
  `deploy/saas/s2-gate-{preflight,cleanup}.sql`. The preflight creates only tenant 99…01 and
  principal 99…09. The cleanup is admin-only and removes document_events, audit_events, documents,
  projects, workspaces, members, tenant and principal by tenant, so the probe can rerun. Create: the
  workspace (991…), project (992…) and document (993…) are created through
  `metadata::create_{workspace,project,document}`. `audit::record_audit_event` records
  document.create/rename/archive/restore, and the probe asserts 4 audit rows for the document. Edit:
  a `DurableRoom::open` over a fixed base (a page plus a frame) admits two editors and a viewer.
  Each is a real `figcraft_collab::Client` built from its Welcome and edits with `Client::edit`.
  Every Outgoing is routed to the clients and their follow-ups are sent back. The editors make 5
  batches: 2 creates and 3 property sets (name, opacity+visible). The viewer's batch is refused
  (Reject) and is never confirmed or written. The broadcast batches equal `read_events` exactly, in
  order, and current_seq equals the event count (5). Every client's `confirmed().snapshot()` equals
  `server.replica().snapshot()`. Restart: the room is dropped and reopened over the same base. The
  replica snapshot equals the one before the restart, seq equals the committed current_seq, and the
  fencing token went up. A failed commit is never confirmed: (1) A second open fences the restored
  room. Its next edit returns `Err(Append(StaleFence))` and the room is poisoned. (2) A room uses a
  1-connection pool with a unique `application_name`. While a tenant transaction holds `SELECT … FOR
  UPDATE` on the document row, its append waits on that lock. The probe finds that backend through
  `pg_stat_activity` (`wait_event_type = 'Lock'`) and kills it with `pg_terminate_backend`.
  `receive` then returns `Err(Append(Database))` with no Outgoing and the room is poisoned. In both
  cases committed stays (5,5), neither batch is in `read_events`, and a fresh open's snapshot equals
  the confirmed state ("Gate frame" and "Card" survive, not "Fenced" or "Lost"). Later edits
  continue gap-free (seq 6). Rename and archive: `rename_document` then `archive_document` (state
  Archived). The live room's next edit returns `Err(Append(InvalidState))`, `DurableRoom::open`
  returns `Err(Open(InvalidState))`, and committed is unchanged. Restore: after `restore_document`
  (Active) the document opens again with the identical snapshot, seq 6 and the new name. Final
  checks: the durable log equals every confirmed batch, and confirmed seqs are 1..=6 with no gaps.
  Procedure: documented in `docs/saas/deployment-operations.md` under "S2 gate procedure". Result:
  passes on reruns against schema 20261016000005. `crates/cloud-storage/tests/migration_upgrade.rs`
  covers the roadmap's remaining exit criterion: a scratch database migrated through an exact copy
  of every migration but the newest is refused by the binary's readiness range, upgrades with the
  embedded `migrate_database` (SQLx verifying the applied files' checksums), reruns as a no-op, and
  ends with the same columns as a database migrated from empty. All S2 probes run in the disposable
  Compose smoke.

## S3 — S3-compatible objects and migration

### Object repository

- [x] **S3-001** Implement S3-compatible object client configuration and connectivity checks.
  Depends on S1-GATE and S2-GATE.
  Evidence: `object-store-client-decision.md` records the product owner's choice of `rusty-s3`
  SigV4 signing sent with `reqwest` over Rustls/`ring` and compiled-in webpki roots (no OpenSSL,
  `aws-lc-rs` or OS store), with licences and alternatives. `figcraft-cloud-storage::objects`
  validates endpoint (http/https, host, no credentials, query, fragment or path), region, bucket
  name and credentials before building a client with redirects and proxies disabled and connect and
  total timeouts; credentials are never formatted and errors are fixed categories.
  `check_connectivity` sends one signed `HeadBucket`. The cloud binary now keeps `S3_ENDPOINT`,
  `S3_REGION`, `S3_BUCKET` and a strictly parsed `S3_FORCE_PATH_STYLE`, and readiness requires the
  supported schema and a passing object-store check. Unit tests cover settings validation,
  redaction and an unreachable endpoint; the SeaweedFS probe in the Compose smoke distinguishes
  success, a wrong secret or key (`Unauthorized`), a missing bucket (`BucketMissing`) and a closed
  port (`Unreachable`).
- [x] **S3-002** Create object, reference and upload-reservation migrations.
  Evidence: `20261017000001_objects.sql` adds `objects`, `object_references` and
  `upload_reservations` and advances readiness to that version. Constraints, not application code,
  keep every object key under its tenant's `tenants/<id>/` prefix with no `.`, `..` or empty
  segments, require a 32-byte SHA-256, a positive size and bounded kind, media type, producer
  version and state, require `verified_at` for verified objects and `deleted_at` exactly for
  `deleting`/`deleted`, and allow one live row per `(bucket, object_key)`. References and promoted
  reservations use composite `(tenant_id, object_id)` foreign keys; a reservation's staging key is
  fixed to `tenants/<tenant>/staging/<reservation>`, its method to `PUT`, its expiry after
  creation, and `promoted` requires an object. Runtime grants cover the three tables (the role
  preflight checks every table). `schema-preflight.sql` proves cross-tenant keys, traversal, empty
  segments, short digests, empty objects, unknown kinds, unverified `verified` rows, duplicate live
  keys, cross-tenant references, a foreign staging key, a non-`PUT` method, past expiry and
  object-less or unknown-object promotion are refused; the upgrade probe covers the new version.
- [x] **S3-003** Implement tenant-scoped immutable object metadata and checksum verification.
  Evidence: `figcraft-cloud-storage::objects` gains bounded `put_object`, `get_object` (refused
  past the caller's bound by `Content-Length` and while streaming) and `delete_object`, signing
  only keys of lowercase letters, digits and `/._-` without empty, `.` or `..` segments; because
  `rusty-s3` unwraps the object URL internally, the URL is built fallibly from the same key first.
  `object_metadata::keys` builds the documented tenant-scoped, content-addressed layout.
  `register_object` records a `pending` row; `verify_object` locks the row, reads the bytes back
  and marks it `verified` only when size and SHA-256 match, otherwise `rejected` (`Missing`,
  `SizeMismatch`, `ChecksumMismatch`). Verified objects are not re-read, rejected or deleted ones
  are `InvalidState`, and a store failure that says nothing about the bytes leaves the row
  `pending` (`Store`). The PostgreSQL + SeaweedFS probe covers a verified object and idempotent
  re-verify, altered, short, long and missing bytes, a rejected object staying rejected after
  correct bytes appear, an unreachable store, duplicate live keys, a foreign-prefix key and
  cross-tenant invisibility.
- [x] **S3-004** Implement staging upload reservations with method, key, size, type and expiry.
  Evidence: `figcraft-cloud-storage::uploads::reserve_upload` checks kind, size, media type and TTL
  against a per-kind policy before any SQL runs. Assets allow up to 32 MiB of
  `image/png|jpeg|webp|gif` or `application/octet-stream` (opaque binary assets, served only as
  attachments). `image/svg+xml` is not allowed because SVG can carry script. Snapshots and libraries
  allow up to 128 MiB and plugins up to 16 MiB, each with exactly one vendor type
  (`application/vnd.figcraft.snapshot|library|plugin`). Media types must match exactly: no
  parameters and no mixed case. Exports and thumbnails come from workers and cannot be reserved
  (`UnsupportedKind`). TTLs are whole seconds from 1 s up to `MAX_PRESIGN_TTL`; longer requests are
  capped. A principal may hold at most 100 unexpired open reservations per tenant
  (`TooManyReservations`). The presigned `PUT` for exactly `keys::staging(tenant, id)` is signed
  before the row is inserted, and the row's expiry is `clock_timestamp() + ttl`, so the URL never
  outlives the reservation. The function returns the row (expiry as Unix seconds, plus an `expired`
  flag taken from the database clock), the method `PUT`, the URL and the TTL. `expire_reservations`
  moves up to 500 overdue `reserved` rows to `expired`, oldest expiry first, using `FOR UPDATE SKIP
  LOCKED`, and returns their staging keys for deletion after commit. Unit tests cover the policy
  bounds, a junk media-type/state/kind sweep and TTL capping. The PostgreSQL + SeaweedFS probe
  covers: the recorded fields; `X-Amz-Expires` equal to the TTL; the PUT landing only on the staging
  key (a PUT rewritten to the final key is refused); the 900 s cap; zero, negative and oversize
  sizes; SVG, mixed case and cross-kind types; export and thumbnail kinds; a sub-second TTL; an
  unknown principal (`Constraint`); the open-reservation cap; another tenant seeing nothing; and the
  sweep expiring only overdue rows. Integration (this round): the presigned `PUT` signs the bucket's
  staging tag (`x-amz-tagging: figcraft-staging=true`, returned in `ReservedUpload::headers`), so an
  upload that drops it is refused and abandoned staging objects match the S3-014 lifecycle rule;
  assets are limited to the four image types the validator can vouch for.
- [x] **S3-005** Implement upload completion, read-back verification and promotion.
  Evidence: `uploads::complete_upload` locks the reservation row (`FOR UPDATE`). An unknown or
  other-tenant reservation is `NotFound`; an expired row, or a `reserved` row whose expiry has
  passed, is `Expired`; a rejected one is `InvalidState`; completing a promoted one again returns
  the same object (`reused`). Assets are the only kind it completes: snapshot, library and plugin
  keys need ids a reservation does not carry, so those are left to S3-008, S3-010 and S3-011. It
  reads the staging object back within the expected size. Nothing staged is `NotUploaded` and
  changes nothing. A size mismatch (including an oversize `TooLarge` read) or a SHA-256 mismatch
  marks the reservation `rejected` and deletes the staging object. Matching bytes first reuse a
  verified object with the same tenant, kind, digest and size. Otherwise they fill an existing
  pending row at the content-addressed key, or else `register_object` creates one, followed by
  `put_object` to `keys::asset` and `verify_object` (read-back of the final key). A rejected or
  deleting row holding the key is `ObjectConflict`. Then the reservation becomes `promoted` with
  `object_id` and `completed_at`, and the staging object is deleted last, so any `Err` can be rolled
  back and retried. A store outage leaves the reservation `reserved`. Unit tests sweep junk producer
  versions. The probe covers: the happy path through a credential-free presigned PUT client;
  completing before upload; cross-tenant and unknown completion; invalid producer versions; a
  verified final object whose bytes and metadata match, with the staging object deleted; idempotent
  re-completion; duplicate content deduped to one object; altered bytes rejected, with staging
  deleted, no final object written, and the reservation still refused after correct bytes appear;
  oversize and short uploads rejected with staging deleted; an expired reservation refused before
  and after the sweep; an unreachable store changing nothing before a successful retry; and a
  snapshot reservation refused as `UnsupportedKind`. Integration (this round): completion takes the
  completing principal and refuses anyone but the reserver (`NotReservationOwner`), and bytes that
  match digest and size are then run through `validation::validate`; a failure is
  `Completion::Invalid` with the reservation rejected and the staging object deleted (probed with a
  checksum-correct non-image).
- [x] **S3-006** Implement authorized, short-lived presigned downloads.
  Evidence: `figcraft-cloud-storage::downloads` signs object downloads only through
  `presign_download`, which consumes a `DownloadAuthorization`. The grant has private fields and no
  `Clone`, `Default` or public constructor; the only way to get one is the explicit step
  `authorize_download(tx, principal, object_id)`. That step requires an active `tenant_members` row
  for an active principal in the transaction's active tenant, and a `verified`, non-deleted object
  in that tenant. It is the seam S4 extends with document and asset access checks.
  `presign_download` refuses a grant from another tenant's transaction or for another object
  (`NotAuthorized`). It re-reads the object `FOR SHARE`, so a row deleted since authorization is
  refused (`Unavailable`), and it refuses an object in a bucket other than the store's. It signs one
  `GET` for the stored key, valid for 5 minutes by default; longer requests are clamped to
  `MAX_PRESIGN_TTL` (15 min), the TTL is truncated to whole seconds, and anything under 1 s is
  refused (`InvalidTtl`). The result holds the URL plus size, SHA-256 and media type, never
  credentials, and its `Debug` leaves the URL out. Each issued URL records an
  `object.download_signed` audit event (principal, object, kind, ttl_seconds) in the same
  transaction; if the audit write fails, no URL is returned. Missing, foreign, pending, rejected and
  deleting objects all give the same `Unavailable`, so other tenants' ids cannot be probed. A unit
  junk sweep over 4096 TTLs plus the extremes never leaves 1 s..=15 min. The PostgreSQL + SeaweedFS
  probe covers: a verified object downloading exactly its bytes through an independent ring-rustls
  reqwest client; `X-Amz-Expires=300` by default and 900 when 24 h is asked; sub-second TTL refused;
  a 1 s URL refused after expiry; the URL failing for a different key and for PUT; a grant not
  signing another object; pending, rejected, unknown and deleting objects refused, including
  deletion between authorization and signing; suspended members, non-members and suspended
  principals refused; a member of two tenants refused the other tenant's object; a grant not usable
  in another tenant's transaction; a suspended tenant refused; exactly three audit events for three
  issued URLs and none for refusals.
- [x] **S3-007** Implement object-reference accounting and retention holds.
  Evidence: `figcraft-cloud-storage::object_references` adds `add_reference`/`remove_reference` for
  the seven ordinary owner kinds and `place_retention_hold`/`release_retention_hold` (a hold is an
  `object_references` row with `owner_kind = 'retention_hold'` and `owner_id` = the hold id). All
  four are idempotent (`Applied`/`AlreadyApplied`). New references are accepted only on `pending` or
  `verified` objects. Holds are also accepted on `rejected` ones, because a hold keeps bytes and
  does not use them. `deleting`/`deleted` objects take neither (`InvalidState`), and an unknown or
  foreign object is `NotFound`. Holds are capped at 64 per object (`TooManyHolds`), and the module
  documentation says only operator support tooling and legal-hold workflows may place or release
  them. `reference_count` returns references and holds separately; `list_references` is bounded to
  1..=1000. `collectable_objects(tx, min_age, limit)` returns at most 1000
  `pending`/`verified`/`rejected` objects with no reference and no hold, oldest first. Their age is
  at least `MIN_COLLECTION_AGE` (24 h): a lower request is raised to it, and requests are capped at
  ten years. `begin_object_deletion` locks the row `FOR UPDATE` and re-checks age (`TooYoung`) and
  references (`Referenced`) in a separate statement before setting `deleting` and `deleted_at`; it
  is idempotent on `deleting` and `InvalidState` on `deleted`. `add_reference` (`FOR SHARE`) and
  `place_retention_hold` (`FOR UPDATE`) take conflicting row locks, and serialization failures map
  to `Conflict`. The PostgreSQL probe covers idempotent add/remove/hold/release, holds kept apart
  from references, state refusals, the hold cap, cross-tenant `NotFound`, the candidate filter
  (references, holds, freshness, a 23 h object kept safe by the floor despite `min_age = 0`, longer
  and capped ages, ordering, limits), deletion re-checks, and holds released so objects become
  collectable again. It also covers lock-ordered races (checked through `pg_stat_activity` that the
  second transaction is waiting): a reference or hold committed first makes the waiting deletion
  `Referenced`; a deletion committed first makes the waiting reference or hold `InvalidState`; a
  rolled-back deletion lets the waiting reference land. Finally, 8 free-running reference/deletion
  pairs each end with exactly one winner and never a reference on a `deleting` object. No migration.

### Stored object types

- [x] **S3-008** Store and restore immutable document snapshots.
  Evidence: `figcraft-cloud-storage::document_snapshots::store_snapshot(tx, store, document,
  &SessionState, producer_version)` encodes the state with `figcraft_collab::message::encode`, which
  is bounded to 1 B–128 MiB (`MAX_SNAPSHOT_BYTES`). A seq that does not fit i64, or an empty or
  over-64-byte producer version, is `InvalidInput`. A state past `documents.current_seq` is
  `AheadOfLog`, and nothing is uploaded for it. Valid state is uploaded under the content-addressed
  `keys::snapshot` (seq plus SHA-256) and registered as a `snapshot` object with `MEDIA_SNAPSHOT`.
  It is then read back through `verify_object`, recorded as a verified `document_snapshots` row and
  referenced with `owner_kind = 'document_snapshot'`, all in the caller's transaction. Storing the
  same state again reuses the object row and the snapshot row. The bytes are written again and
  re-read and checked, because `verify_object` does not re-read verified rows; this rewrite repaired
  a corrupted object in the test. A rejected or deleting row under the key, or a lost unique race,
  is `Conflict`. The document row is locked `FOR UPDATE`, and only then is `current_snapshot_id`
  moved. It moves only when the new seq is >= the current pointer's seq (read fresh after the lock)
  and <= `current_seq`. Otherwise the result is `Pointer::Kept`, so the pointer never moves
  backwards: a seq-3 snapshot stored after seq 4 leaves the pointer on 4.
  `restore_latest_snapshot(_with)` tries at most `MAX_RESTORE_CANDIDATES` (8) verified snapshots,
  newest first. Each must be at or below `current_seq`, with an object of kind `snapshot` that is
  verified and in this bucket. For each, the object is downloaded again within its recorded size.
  The size and SHA-256 must match both the snapshot row and the object row, the bytes must decode
  within the cap, the decoded seq must equal the row's seq, and the caller's `accept` check must
  pass. A failing candidate is skipped and reported (`SkippedSnapshot` with `Missing`,
  `SizeMismatch`, `ChecksumMismatch`, `Undecodable`, `SequenceMismatch` or `Refused`), and the
  preceding one is tried. A store outage is `Store(..)`, never a silent fallback to an older
  snapshot. Errors are fixed `SnapshotStoreError` categories.
  `figcraft-cloud-relay::DurableRoom::recover(pool, &store, ...)` claims, then calls
  `Server::restore` on the newest usable snapshot at or below the claimed seq and replays only the
  events after it. With no usable snapshot it replays the whole log over `base`, which either
  reproduces the committed state or fails with `Replay`. `open` keeps the base-only path and now
  also refuses a replay that does not reach the claimed seq. `restored_snapshot()` reports (id,
  seq), and the new `RoomError::Snapshot` covers snapshot read failures. Probes against PostgreSQL
  and SeaweedFS, fixture a6, a fresh document per run: (1) snapshot plus replay equals pure replay,
  comparing replica and seq 5, then seq 7. Recovery from the seq-3 snapshot succeeds over a base the
  log does not apply to, while `open` over that same base fails with `Replay`, which proves the base
  was not used. (2) An object byte flipped in place by a raw PUT is never used: the restore falls
  back from 4 to 3 (`ChecksumMismatch`), and after the seq-3 object is deleted it falls back to 2
  (`Missing`). The room falls back from 6 to 3, and with every snapshot corrupt it falls back to the
  base. (3) The pointer moves 2 → 4, is kept against a later 3, and moves on equal seq. (4) Another
  tenant gets no candidates, gets `None` from `get_object`, gets `NotFound` from `store_snapshot`,
  and gets `Open(NotFound)` from `recover`. Unit tests include a 2000-round junk-byte sweep through
  the decode checks.
- [x] **S3-009** Store content-addressed image and binary assets.
  Evidence: `figcraft-cloud-storage::assets` is the document-facing asset API over
  S3-002/004/005/006/007. `find_asset(tx, sha256)` returns the tenant's oldest verified, non-deleted
  `asset` object at the content-addressed key `keys::asset(tenant, sha256)`, for client-side dedupe
  before a reservation. Pending, rejected and deleting rows are never offered, and another tenant's
  identical bytes are invisible. `store_asset_bytes(tx, store, bytes, media_type, producer_version)`
  is for bytes produced by servers or workers. It runs the pure `prepare_asset` first: the producer
  version check, the exact `uploads::ASSET_MEDIA_TYPES` allowlist, 1..=`MAX_ASSET_BYTES` checked
  before any byte is read, `validation::validate` with default budgets, then SHA-256. It returns a
  matching verified object in the store's bucket (`reused`), or fills a `pending` row already on the
  key with the same digest, size and kind, or registers, writes and verifies by read-back. A
  rejected or deleting row on the key, or a row that disagrees with the bytes, is `ObjectConflict`.
  Binary assets: only PNG/JPEG/WebP/GIF are accepted. Octet-stream, SVG, PDF and fonts are
  `UnsupportedMediaType` until a validator exists in `validation`. This is documented in the module;
  validation is not weakened. `attach_asset(tx, document, object)` locks the document `FOR NO KEY
  UPDATE` and is idempotent: an existing reference is `AlreadyApplied` whatever the document's state
  now. A new attachment needs a `pending`/`active` document of the tenant (`DocumentUnavailable` /
  `DocumentNotFound`) and a verified, non-deleted `asset` object locked `FOR SHARE`
  (`AssetUnavailable` otherwise, which conflicts with `begin_object_deletion`). It is capped at
  `MAX_ASSETS_PER_DOCUMENT` = 10 000 (`TooManyAssets`), and it records `object_references`
  `owner_kind = 'document'` through `add_reference`. `detach_asset` is idempotent and returns
  `DocumentNotFound` for a foreign document. `list_document_assets(tx, document, after, limit)` is a
  keyset page by object id, bounded 1..=500 (`InvalidLimit`), with `next_after`.
  `presign_document_asset_download(tx, store, principal, document, object, ttl)` runs
  `downloads::authorize_download` first (membership; `Download(NotAuthorized)` reveals nothing about
  the document). It then needs a `pending`/`active`/`archived` document and the attachment row
  locked `FOR SHARE`, so a concurrent detach waits. It signs and audits through `presign_download`.
  Missing, foreign or unverified objects are `NotAttached`. Errors are a fixed `AssetError` enum.
  The PostgreSQL/SeaweedFS probe (fixture prefix a7) covers: store, read-back and canonical key;
  dedupe on re-store and via `find_asset` (one row); a per-tenant copy in tenant two; a pending row
  filled with the same id; a rejected row blocking with `ObjectConflict`; validation refusing
  non-image bytes as `image/png` (`MediaMismatch`), octet-stream, SVG, a mislabelled JPEG,
  media-type parameters, empty bytes and a bad producer, with nothing registered or written;
  attach/detach idempotency and reattach; refusal of pending, rejected, deleting, verified-`export`,
  unknown and other-tenant objects; refusal of archived, deleting, unknown and other-tenant
  documents; tenant two unable to attach, detach, list or download tenant one's ids; paging;
  downloads fetching exact bytes only for attached assets (another document, detached, foreign,
  unknown, outsider and zero-TTL refused); archived documents readable and deleting ones not; and
  exactly one audit event per committed URL. Unit tests: a 4 096-round junk byte/type sweep and
  corrupted PNGs. No migration.
- [x] **S3-010** Store team-library versions and usage references.
  Evidence: `figcraft-cloud-storage::libraries` adds `create_library(tx, owning_team,
  source_document, name)`. The name is trimmed, 1–255 chars with no control characters. The team
  must be an active team of the same tenant (`TeamNotFound`/`TeamNotActive`), and the source
  document must exist in the tenant and not be deleting or deleted. `publish_library_version(tx,
  store, library, publisher, bytes)` caps bytes at `MAX_LIBRARY_BYTES` and runs
  `validation::validate(MEDIA_FIGCRAFT, …)` before any SQL (`Invalid(Rejection)`). It then locks the
  library row `FOR UPDATE` and refuses archived or deleted libraries (`Archived`/`Deleted`). A
  backstop check comes next (`NotPublisher`): the publisher must be an active principal and active
  tenant member who is a tenant owner/admin, or a `team_manager`/`editor` of the owning team or of a
  team granted `publisher`, or a tenant editor when the library has no owning team. The next version
  is max+1, capped at `MAX_VERSIONS_PER_LIBRARY` = 10 000. Bytes go to `keys::library` (version plus
  SHA-256, checked with `valid_key`) through `register_object` → `put_object` → `verify_object`
  read-back. Only then are the `library_versions` row and a `library_version` object reference
  inserted. `grant_library_to_team` upserts the role idempotently, active libraries and active
  same-tenant teams only; `revoke_library_grant` is idempotent in any state. `list_grants` is
  bounded, `list_versions` lists newest first with a keyset `before` cursor, and every page is
  1..=1000 (`InvalidLimit`). `record_library_usage(tx, document, version)` keeps one row per
  document+library. It returns `Recorded`, `Switched { previous }` or `AlreadyRecorded`, and locks
  the library `FOR SHARE` against `archive_library`. Decision: an archived library keeps its
  existing usages and re-recording the current version is a no-op, but it takes no new usages, no
  version switches, no new versions and no grants; a deleted library refuses all of them; documents
  must be active. `remove_library_usage`, `usage_of_library`, `usages_of_version` (paginated by
  document id) and `archive_library` (idempotent) complete the API; errors are fixed `LibraryError`
  categories. The PostgreSQL + SeaweedFS probe covers: trimmed or refused names; foreign, unknown
  and archived teams and foreign or deleting documents refused; v1 verified at its content-addressed
  key with intact read-back and exactly one `library_version` reference. Empty, non-zip, PNG,
  truncated and `document.json`-less archives are refused and take no version number. A team viewer,
  an ungranted team editor, a tenant editor on a team-owned library and another tenant's principal
  are all `NotPublisher`, while a tenant admin publishes. A `viewer` grant still refuses publishing
  and a `publisher` grant allows it, and revoking the grant refuses it again. Listing order, cursor
  and limits are checked, and another tenant sees nothing and cannot publish, grant or use the
  versions. The usage path checks record → switch → idempotent re-record, pagination, and archived,
  deleting, foreign or unknown documents and versions. The archive freeze is verified, and a deleted
  library is refused. In the ordered concurrency test the waiter is blocked on the row lock (checked
  through `pg_stat_activity`) and gets version 2. Four free-running racers then get dense, distinct
  versions 3–6 with distinct keys, and each version has one reference.
- [x] **S3-011** Store validated plugin bundles and manifests.
  Evidence: `figcraft-cloud-storage::plugin_store::install_plugin_version(tx, store, PluginName {
  publisher, name }, version, bundle, producer_version)` finds the tenant's live plugin by publisher
  and name, or creates it `active`. It does this under a transaction advisory lock, because
  `plugins` has no unique name constraint, and then locks the plugin row `FOR UPDATE`, so installs
  of one plugin run one at a time. Names, the version (`keys::plugin_version_valid`) and the
  producer version are checked before any SQL. So is the bundle: at most `MAX_PLUGIN_BYTES`, then
  `validation::validate(MEDIA_PLUGIN)`. A separate bounded `FCPLUGB1` reader takes out the manifest,
  and its module length and asset count must match the validator's `Summary::Plugin`. The manifest
  must be a JSON object with an `id` (1–128 of `[A-Za-z0-9._-]`) and a non-blank `name` and `main`.
  It may carry a string `version` (trimmed), which must equal the requested version
  (`VersionMismatch`). It may not contain U+0000, which JSONB refuses. Every version of one plugin
  must carry the same manifest `id` (`IdMismatch`). Its size is measured as PostgreSQL renders the
  JSONB (`octet_length($1::jsonb::text)` ≤ 256 KiB). Without that check a 1e300 array that fits the
  bundle would overflow the column; it is refused as `TooLarge`, not as a constraint error. The
  bytes go to `keys::plugin` and are registered and verified by read-back. A verified object already
  at that key is reused, a pending one is filled, and a rejected or deleting one is
  `ObjectConflict`. Then a `plugin_versions` row (`verified`, with the parsed manifest) and a
  `plugin_version` object reference are written. Re-installing the identical bundle is idempotent
  (`reused`); a different bundle for an existing version is `Conflict`; a withdrawn version or a
  disabled plugin is `InvalidState`. `withdraw_plugin_version` is idempotent
  (`Applied`/`AlreadyApplied`) and keeps the row, reference and bytes. `list_versions` returns pages
  of up to 1000 manifest-free summaries, oldest first, with withdrawn versions only on request.
  `get_manifest` reads one manifest back and also returns its state. `get_plugin` and `find_plugin`
  look plugins up. All errors are fixed `PluginStoreError` and `ManifestProblem` codes. The
  PostgreSQL + SeaweedFS probe covers: first install (key, kind, media type, size, stored bytes, a
  single reference, manifest round trip); an idempotent re-install; a conflicting bundle that writes
  nothing; version and id mismatches; a second version; listing order and limits; withdrawal
  semantics and re-install refusal; cross-tenant invisibility, with the same name in another tenant
  being a separate plugin; a disabled plugin; 18 junk bundle, name and version refusals that run no
  SQL; the JSONB expansion bound; a rejected object blocking the key; a pending object being filled;
  and an unreachable store (`Store(Unreachable)`).
- [x] **S3-012** Store export results and thumbnails.
  Evidence: `figcraft-cloud-storage::exports::request_export(tx, document, principal, format,
  &options, requested_seq)` requires an active member of an active tenant (`NotAuthorized`) and an
  `active`/`archived` document in the transaction's tenant, locked `FOR KEY SHARE` so event appends
  are not blocked (`NotFound`, `DocumentUnavailable`). The sequence may not pass `current_seq`
  (`SeqAhead`) or `i64::MAX` (`InvalidSeq`). Options must be a JSON object of at most 16 levels, 1
  024 values and 8 KiB compact, with no NUL; they are walked iteratively before encoding, so a 100
  000-deep value is refused without recursing. Each principal may hold at most 32 open exports. The
  call inserts a `queued` export and a `queued` `jobs` row of kind `document.export` whose input
  names the export, document, format and sequence, linked through `job_id`. `start_export` moves
  queued to running and counts a job attempt; on a running export it is an idempotent resume.
  `fail_export` fails a queued or running export with a fixed code (1–128 bytes of `[a-z0-9._-]`).
  `complete_export(tx, store, id, bytes, filename, producer)` accepts only `running` exports and
  checks content by format. png/jpeg/webp/figcraft go through `validation::validate`. SVG must be
  UTF-8 with an `<svg` root (after an optional BOM, XML declaration and comments) that ends in
  `</svg>` or is self-closed, and may not contain script, foreignObject, iframe, embed, object,
  DOCTYPE, ENTITY, `javascript:`/`vbscript:` or `on…=` handlers. PDF needs a `%PDF-n.n` header and
  `%%EOF` in the last 1 KiB, and may not contain the `/JavaScript`, `/JS`, `/Launch`, `/SubmitForm`
  or `/ImportData` name tokens. The SVG and PDF checks are a documented tripwire, not a sanitizer,
  and failures return `ActiveContent`. A refused completion changes nothing. On success the bytes go
  to `tenants/<t>/exports/<export>/<sanitize_filename>`; `sanitize_filename` keeps only the last
  path component, lowercases, maps everything else to `-`, collapses `-`/`.` runs, trims, caps the
  stem at 96 bytes and forces the format's extension. The object is registered as `export`, verified
  by read-back and referenced (`export`, export id). The export becomes `succeeded` with
  `result_object_id` and `completed_at`, and the job becomes `succeeded` with a result.
  `expire_exports(tx, older_than ≥ 1 h, limit ≤ 500)` uses one statement with `SKIP LOCKED` to set
  old succeeded exports to `expired`, clear their result and delete the `export` reference, then
  returns `(export, object)` for the collector. `store_thumbnail(tx, store, document, seq, webp,
  producer)` validates WebP within 1 MiB, 1 024 px per side and 1 Mpx; it requires `seq ≤
  current_seq` and writes `keys::thumbnail`, a verified `thumbnail` object, a `document_thumbnails`
  row and a `thumbnail` reference owned by the document. Identical bytes for a stored seq are
  `AlreadyApplied`, different bytes are `ThumbnailExists` (objects stay immutable), and an older seq
  is `Superseded`. A newer seq drops the older rows and their references (at most 16 per call) and
  reports their objects as superseded. `latest_thumbnail` returns the newest. The PostgreSQL +
  SeaweedFS probe covers 7 direct-SQL updates refused by the exports CHECKs (success without result,
  failure without code, completion time on an open export, stray error code, unknown state/format,
  non-object options) plus clearing a succeeded result. It also covers: complete-before-start, start
  resume and attempts, MediaMismatch/Truncated/empty/bad-producer refusals that leave the export
  running, sanitized key `…/frame-1.png` from `../../Users/Me/Frame 1.PNG` with read-back bytes,
  references and the job result, terminal states refusing every transition, the fail path, SVG/PDF
  active-content refusal and success, a junk `.figcraft` refused then failed, other-tenant
  invisibility for get/start/fail/complete/request/expire, expiry after backdating while fresh
  exports are untouched, the open-export cap, thumbnail
  SeqAhead/InvalidSeq/PNG/oversize-dimension/size/unknown-document refusals, idempotent re-store,
  ThumbnailExists, replacement by a newer seq, Superseded, cross-tenant thumbnail invisibility and a
  cross-tenant `document_thumbnails` FK refusal. Unit sweeps run every format against several
  hundred junk filenames (traversal, Windows paths, Unicode, controls, 100 000-character names);
  each result is one valid key segment. They also cover junk bytes for every format and the
  thumbnail check, and junk formats, states and error codes.
- [x] **S3-013** Add media, archive, image-pixel and expanded-byte validation budgets.
  Evidence: `figcraft-cloud-storage::validation::validate(declared, bytes, &Budgets)` is a pure,
  bounded check of staged bytes before promotion. It returns `Validated { media, summary }` (image
  width/height/decoded bytes, archive entries/expanded bytes, or plugin assets/module bytes), or one
  of 23 fixed `Rejection` codes (`media_mismatch`, `pixel_budget`, `compression_ratio`,
  `unsafe_name`, `overlapping_entries`, …) that never contain input bytes. It reads only headers and
  container directories: it never decodes pixels, never decompresses and adds no new dependencies
  (`zip` is a dev-dependency only, used to write a real `.figcraft` sample). Accepted types are
  `image/png|jpeg|webp|gif`, `application/x-figcraft` (documents and libraries, which are zips) and
  `application/x-figcraft-plugin` (`FCPLUGB1` bundles). The declared type is compared without ASCII
  case, has no parameters, and must match the magic bytes. SVG and PDF are refused and the reason is
  documented. Images: PNG `IHDR` must come first with a valid CRC, depth and colour type, and
  dimensions of at most 2^31-1; JPEG is walked to the first `SOFn` (at most 1 024 segments, a
  zero/DNL height is refused); WebP must be an exact-length RIFF whose first chunk is a `VP8 ` key
  frame, `VP8L` or `VP8X`; GIF uses the logical screen. Zero dimensions, sides above 65 535, more
  than 100 MP, or more than 512 MiB decoded (4 bytes a pixel, 8 at 16-bit) are refused. Zips: the
  EOCD must end the upload exactly and follow the central directory directly. Zip64, multi-disk,
  encryption/AES and methods other than stored/deflate are refused, and stored sizes must agree.
  Each local header must repeat its central name and method, entry ranges must be disjoint and start
  at byte 0 (overlap bombs, polyglots), and names must be UTF-8 relative paths with no control
  characters, `\`, `:`, empty/`.`/`..` segments, and no duplicates ignoring ASCII case. Other
  budgets: at most 50 000 entries, 512 MiB per entry, 2 GiB declared expanded in total, and a 250:1
  ratio above 1 MiB; `document.json` is required. Plugin bundles: a UTF-8 manifest of at most 256
  KiB, a module of at most 16 MiB with the wasm v1 header, at most 32 assets with plain, non-device,
  case-unique names, and no trailing bytes. Tests cover every accepted kind, huge/zero dimensions,
  bad CRC/depth, ratio, size and count bombs, traversal/absolute/control/duplicate names,
  encryption, zip64 sentinels and locators, an overlap bomb, renamed local headers, trailing and gap
  bytes, and hostile plugin bundles. The never-crash sweep runs every truncation of 9 samples
  (containers must refuse every cut), three byte flips at every offset checked against all six
  declared types and tight budgets, and 30 000 seeded random garbage and mutated inputs, some ending
  in a forged EOCD. `docs/saas/security.md` gains "Upload validation budgets". FigCraft media types
  are canonical in `validation` (`application/vnd.figcraft.snapshot|library|plugin`) and shared by
  uploads and the migration planner.

### Lifecycle and migration

- [x] **S3-014** Add idempotent S3-compatible bucket, versioning and lifecycle initialization.
  Evidence: `figcraft-cloud-storage::bucket_admin::initialize_bucket` brings the bucket to the
  target state, safely when run repeatedly or concurrently. It sends `HeadBucket` and then
  `CreateBucket` only on a 404, with a location constraint outside us-east-1. A 409
  (`BucketAlreadyOwnedByYou`, a concurrent create) is settled by a second HEAD: 200 means already
  existed, 403 means `BucketOwnedElsewhere`. It then enables versioning (`PUT ?versioning` Enabled)
  and reads it back (`GET ?versioning`); the change counts only if it reads back as Enabled
  (otherwise `VersioningNotEnabled`). Finally it reads the lifecycle configuration, writes it only
  if it differs, and reads it back for comparison. `rusty-s3` has no versioning or lifecycle
  actions, so these sub-resource requests are signed with `rusty_s3::signing::sign` over the store's
  validated base URL and carry a signed `Content-MD5`. Response bodies are capped at 64 KiB and
  parsed with `roxmltree` (no DTD, 8192-node limit, at most 1000 rules, day counts limited to 10
  digits, text to 1024 bytes). Failures are fixed, non-leaking categories. The lifecycle has three
  rules. `figcraft-staging-expiry` expires objects tagged `figcraft-staging=true` after 2 days;
  staging keys are `tenants/<tenant>/staging/<id>`, and a prefix filter cannot wildcard the tenant.
  `figcraft-abort-incomplete-multipart` aborts uploads after 2 days, and
  `figcraft-noncurrent-expiry` removes noncurrent versions after 30 days. Periods are validated to
  1..=36500 days. Rules whose ID does not start with `figcraft-` are refused
  (`ForeignLifecycleRules`) and left untouched. A store answering 501/`NotImplemented` reports
  `LifecycleOutcome::Unsupported`. The function returns a typed `BucketInitReport`
  (Created/AlreadyExisted, Enabled{previous}/AlreadyEnabled,
  Applied/Replaced/AlreadyCorrect/Unsupported, `changed()`). `staging_tagging_header()` gives the
  `x-amz-tagging` header that presigned staging PUTs must sign. `figcraft-cloud --init-object-store`
  is a standalone explicit command that reads the normal S3 config and secrets and never runs at
  startup. The Compose `seaweedfs-init` job now runs it from the cloud image instead of `weed
  shell`. The SeaweedFS probe covers: a fresh throwaway bucket created, versioned and given verified
  lifecycle rules; two reruns making no change; a changed policy replacing the rules and a restore
  replacing them back; a foreign rule refused and kept, then Applied once removed; two concurrent
  inits both succeeding, with at most one Created and the final state correct; and a presigned PUT
  with the signed tagging header storing the tag, while an untagged or retagged PUT is refused. It
  also covers wrong credentials (`Unauthorized`), a closed port (`Unreachable`) and an invalid
  policy (`InvalidPolicy`), none of which create the bucket. Probe buckets are deleted after each
  run.
- [x] **S3-015** Add garbage-collection jobs with dry-run and minimum-age protection.
  Evidence: `figcraft-cloud-storage::garbage_collection::collect_garbage(&PgPool, &ObjectStore,
  TenantId, GcOptions { dry_run, limit 1..=1000, min_age })` runs one bounded pass over one tenant
  and returns a `GcReport` (resumed, candidates, protected with reason, skipped, deleted, failed,
  versions_removed, staging_keys, staging_failed). A requested min_age is raised to
  `MIN_COLLECTION_AGE` and capped at `MAX_COLLECTION_AGE`; `GcOptions::from_job_input` parses a
  `object_garbage_collection` job's `{dry_run, limit, min_age_seconds}`, refusing unknown fields,
  wrong types and ages below the floor. Order: objects left `deleting` by an interrupted pass are
  resumed first; candidates come from `collectable_objects` (one full page, so protected heads do
  not hide collectable objects) and are then excluded, belt and braces, while a current-snapshot
  pointer, `document_snapshots`, `library_versions`, `plugin_versions`, `exports.result_object_id`
  or `document_thumbnails` row names them, a `reserved`/`completed` upload reservation names their
  id, key or kind+digest, a queued/running job's input mentions their id or key (at most 10 000
  active jobs of at most 1 MiB scanned; more fails the pass closed with `ActiveJobsUnbounded`), or,
  for snapshot and asset objects, any tenant export is queued/running. Each candidate is marked by
  `begin_object_deletion` in its own short transaction with the protections re-checked under the row
  lock; then every version and delete marker of the exact key is removed through hand-signed
  `ListObjectVersions` (prefix = key, 100 per page, 256 KiB/32k-node bounded roxmltree parse, at
  most 100 rounds) and `DELETE ?versionId=`, falling back to a plain DELETE on stores without
  version listing; only after an empty listing is the row marked `deleted` (`deleted_at` kept).
  Store failures leave rows `deleting` for the next pass; a resumed `deleting` object that something
  names again is reported `Protected` and its bytes are kept. Overdue reservations are expired
  through `uploads::expire_reservations` and their staging keys removed with all versions. A dry run
  only reads, in rolled-back transactions. The PostgreSQL + versioned SeaweedFS probe (one fixture
  tenant per test, versions counted with an independent signed listing) covers fresh objects
  protected by age, references and holds protecting until released, a dry run changing no state or
  version, a longer min_age honoured and a zero one raised, a limit-1 pass followed by the rest, two
  versions plus a delete marker all removed, a pending row with no bytes, a bystander tenant
  untouched, all nine protection reasons with no reference row and collection once they clear, an
  active export pausing asset collection but not thumbnails, a crash after marking and a crash after
  a partial plain delete both finished by a rerun, an unreachable store leaving every touched object
  `deleting`, an idempotent idle rerun, and an expired staging upload with two versions swept and
  its reservation expired.
- [x] **S3-016** Add database-to-bucket reconciliation report.
  Evidence: `figcraft-cloud-storage::reconciliation::reconcile_tenant(tx, store,
  &ReconciliationLimits)` returns a versioned (`format_version` 1) serde `ReconciliationReport` for
  one tenant. It lists `tenants/<tenant>/` with signed `ListObjectsV2` pages, following continuation
  tokens, and compares the keys with the tenant's `objects` rows. Rows are read in `(object_key
  COLLATE "C", id)` keyset pages, which matches S3 byte order. Staging keys are compared with
  `upload_reservations`. It reports: missing objects (`verified` is critical, `pending` is info, and
  `rejected`/`deleting` without bytes are expected); orphan keys (`no_row`, `deleted_row_only`, or
  `unsignable_key` when the key is not one FigCraft writes); stale staging keys (`no_reservation`,
  `reservation_expired`, or `reservation_closed`; staging keys of open reservations are only
  counted); size mismatches between the listed size and `size_bytes` (critical when verified); stuck
  `pending` rows (age since creation) and stuck `deleting` rows (age since deletion started) past
  `stuck_after` (default 1 h), with `in_bucket`; and collectable rows with no reference and no hold
  (info, `eligible` past `MIN_COLLECTION_AGE`). It also gives severity totals, exact per-category
  counts, and live/verified/stored rows and bytes per kind. Read-only: it issues `SET TRANSACTION
  READ ONLY` before any query, so PostgreSQL refuses any later write in that transaction, and sends
  only listing `GET`s. The database clock is read before the listing; rows created or verified after
  it are counted as `recent_rows` and not compared, and rows are read after the listing. Caps: rows
  ≤1M, keys ≤1M, pages ≤10k, page size ≤1000, findings per category ≤10k (counts stay exact). Any
  cap reached is named in `caps_reached` and sets `complete=false`. Comparisons stay within the key
  range both sides fully read (`unchecked_rows`/`unchecked_keys`), so a cap never invents a missing
  or orphan finding. Listing bodies are capped at 4 MiB and parsed with roxmltree under a 64k-node
  limit with no DTD. Keys are percent-decoded strictly, kept to ≤1024 bytes and checked against the
  prefix; tokens are bounded printable ASCII; a repeated token or duplicate key is
  `MalformedListing`. Errors are fixed categories (`InvalidLimits`, `ReadOnly`, `Store(..)`,
  `MalformedListing`, `Database`, `CorruptRow`). Unit tests cover the parser with a junk sweep over
  every truncation and single-byte mutation of a page plus pseudo-random bodies. The PostgreSQL +
  SeaweedFS probe plants, under fixture prefix `ac`: a referenced healthy object; a hold-only
  object; a verified row with no bytes; a verified object overwritten by a raw put of a longer body;
  a raw orphan put; a key whose only row is `deleted`; stuck `pending` and `deleting` rows; a
  rejected row without bytes; unreserved, expired, closed and open staging keys; and another
  tenant's key. It asserts every classification, severity, count and kind total and the JSON round
  trip. It proves nothing changed by comparing rows, reservations, references and an independent
  rusty-s3 listing before and after, with the report's transaction committed. It shows a write in
  the report's transaction is refused. It checks that page-size-2 paging gives the same counts and
  that key, page, row and finding caps mark the report incomplete without false missing or orphan
  findings. Earlier runs' leftovers are cleared first, so reruns pass.
  `docs/saas/deployment-operations.md` gains "Object reconciliation report", which says how to act
  on each category.
- [x] **S3-017** Extend the filesystem scanner into a dry-run migration planner.
  Evidence: `figcraft-cloud-storage::migration_plan::plan_migration(&MigrationReport, &PlanTarget
  {tenant_id, workspace_id, project_id, principal_id, bucket})` builds a versioned (format 1),
  serde-serialisable dry-run `MigrationPlan`. It is pure over the report and target: it reads no
  files, clock, randomness or database, and writes nothing. It refuses only an unknown report
  `format_version` and an invalid bucket name. Identifiers are deterministic, so reruns are
  idempotent. The workspace `uuid` crate doesn't enable v5, so each id is the first 16 bytes of
  SHA-256 over a fixed namespace, the tenant and length-prefixed legacy parts, stamped as an RFC
  9562 version-8 UUID. Every list is sorted, so the same report gives byte-identical JSON. Each
  ready or degraded document plans: - the `documents` row (name trimmed, replaced or cut to 255
  characters; `current_seq` set to `last_accepted_seq`); - an `external_mappings` row
  (`figcraft-server`/`document`/legacy id); - a `document_snapshots` row and a snapshot object (body
  size = file minus the 17-byte header; key prefix and suffix from `object_metadata::keys`; the
  digest is left to the import because the scanner doesn't hash snapshots); - an event count and seq
  range (`log.replayable` of a log that continues the snapshot, from `seq+1` to `last_accepted_seq`,
  actor = the target principal; a mismatch excludes the document); - versioned library objects.
  Plugins with a valid bundle and an integer version get full content-addressed keys with known
  SHA-256 (keys checked with `valid_key`). Anything else is listed in `deferred` with a fixed
  reason. Excluded documents (`blocked`, `invalid_legacy_id`, `duplicate_legacy_id`,
  `metadata_unavailable`, `snapshot_unavailable`, `sequence_inconsistent`) carry their blocking
  problem codes. A document marked ready is still excluded if a blocking problem names it. Degraded
  documents keep their warning codes and plan warnings (`log_truncated`, `log_ignored`,
  `name_replaced`, `libraries_deferred`, …). Plan blockers are `report_incomplete`,
  `blocking_server_problem`, `exclusion_approval_required` and `document_limit_reached`. Totals
  cover object count, snapshot, library and plugin bytes, and estimated bytes (`upload_bytes +
  event_bytes_max`). Caps: 100,000 documents, 256 libraries per document, 10,000 plugins.
  `filesystem-migration-report.md` documents the plan format, the id derivation and every reason
  code. The probe checks: - stable output across runs, report orderings and tenants; - blocked
  documents excluded; - degraded documents included with warnings; - inconsistent, malformed and
  duplicate documents excluded; - stale logs ignored; - plugin planning and deferral; - a scanned
  synthetic directory planning its replayable range, with the tree unchanged; - the document cap; -
  the JSON format, pinned against `tests/migration_plan_golden.json`; - a 2,000-report pseudo-random
  sweep with hostile values, checking invariants: unique ids, valid keys, `BIGINT` bounds, `seq +
  count = current_seq`, totals. Unit tests add a junk-string sweep over the validators.
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
| 2026-10-16 | S2-003: gated startup readiness on the supported SQLx schema range and ordered the one-shot Compose migration job before cloud roles; empty baseline 0 is supported pending S2-004. |
| 2026-10-16 | S2-004: added the first tenant-scoped identity and document metadata migration, checked its constraints against PostgreSQL, and advanced cloud schema readiness to version 20261016000001. |
| 2026-10-16 | S2-005: added event, snapshot-pointer, job, idempotency and audit schema with synthetic PostgreSQL constraint checks; advanced readiness to version 20261016000002. |
| 2026-10-16 | S2-006: provisioned separate migration/runtime PostgreSQL logins, transferred existing schema ownership, reconciled least-privilege grants and verified runtime DDL denial in Compose. |
| 2026-10-16 | S2-007: added a typed tenant-scoped SQLx transaction helper and verified transaction-local context against a synthetic PostgreSQL RLS policy on a reused pooled connection. |
| 2026-10-16 | S2-008: added tenant/workspace/project create and read repositories, membership-scoped tenant discovery and bounded keyset pagination, verified against synthetic cross-tenant PostgreSQL fixtures. |
| 2026-10-16 | S2-009: added document create/read/rename/archive/restore/soft-delete repositories and a `deleted_at` grace-period migration advancing readiness to version 20261016000003; fixed two pre-existing Compose smoke defects (unreadable `umask 077` secrets under a nonroot container user, and a stale cloud schema-version constant) found while verifying it end-to-end. |
| 2026-10-16 | S2-010/S2-011/S2-012/S2-017: added external mapping, idempotency-key, append-only audit and snapshot metadata repositories in parallel (each in its own new module/test/fixture files, with the coordinating session wiring `lib.rs` and the Compose smoke script afterward); `20261016000004_external_mappings.sql` advanced readiness to version 20261016000004. S2 reaches 13/21. |
| 2026-10-17 | S2-010 fix: granted the runtime role access to `external_mappings` (its repository probe failed with a permission error in the full Compose smoke); `role-preflight.sql` now requires migrator ownership and runtime `SELECT` on every public table, so a future table missing from the grant list fails the smoke. |
| 2026-10-17 | S2-013: defined durable event payloads as the existing collaboration `Accepted` wire form with a domain-separated SHA-256 checksum bound to tenant, document and sequence. S2 reaches 14/21. |
| 2026-10-17 | S2-014: added the locked, gap-free event append and a trigger keeping document sequence and fencing token monotonic; readiness advanced to schema version 20261016000005. S2 reaches 15/21. |
| 2026-10-17 | S2-015: added `claim_sequencer` and a current-fencing-token check in the append under the document row lock. S2 reaches 16/21. |
| 2026-10-17 | S2-016/S2-019/S2-021: added bounded checksum-verified event replay, multi-connection PostgreSQL race probes for sequence, fencing and idempotency, and a read-only filesystem-server scanner with a versioned migration report (three parallel worktree agents, merged and verified together). S2 reaches 19/21. |
| 2026-10-17 | S2-018: added `DurableRoom`, committing accepted batches under the fencing token before confirming or broadcasting them and reopening rooms by fenced takeover plus checksum-verified replay. S2 reaches 20/21. |
| 2026-10-17 | S2-020/S2-GATE: added connection-kill, lock-wait, lost-acknowledgement, timeout, idempotency-boundary and room-level failure injection; the end-to-end create/edit/restart/restore gate probe; and a previous-schema upgrade probe. S2 complete (21/21) and its gate passes. |
| 2026-10-17 | S3-001: chose `rusty-s3` + `reqwest` (Rustls/ring) for S3-compatible storage, added the validated object-store client and a signed `HeadBucket` readiness check. S3 reaches 1/20. |
| 2026-10-17 | S3-002: added object, object-reference and upload-reservation tables with tenant-prefix key constraints; readiness advanced to schema version 20261017000001. S3 reaches 2/20. |
| 2026-10-17 | S3-003: added bounded object I/O, the content-addressed key layout and read-back SHA-256 verification of object metadata. S3 reaches 3/20. |
| 2026-10-17 | S3-004..S3-007, S3-013, S3-014, S3-017: upload reservations and validated promotion, authorized presigned downloads, references and retention holds, content validation budgets, idempotent bucket/versioning/lifecycle initialization, and a dry-run migration planner (six parallel worktree agents, then integrated: tagged staging PUTs, completer check, validation before promotion, one FigCraft media-type set). S3 reaches 10/20. |
| 2026-10-17 | S3-008..S3-012, S3-015, S3-016: snapshot store and snapshot-first room recovery, document assets, team-library versions, plugin bundles, exports and thumbnails on one shared schema migration (20261017000002), garbage collection with versioned deletes, and a read-only reconciliation report (seven parallel worktree agents; verified with targeted suites). S3 reaches 17/20. |
